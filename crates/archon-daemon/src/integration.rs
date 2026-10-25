//! Each repository's integration branch, verified off the items' path
//! (issue #82). One worker reads every integration branch's head; once
//! `batch_merges` merges have landed since the last verified head, or the
//! branch stood still for `idle_after`, it verifies the head: by the
//! commit's GitHub check runs when CI covers the branch, else by running
//! `[checks] full` in the host's one build slot. A red head is bisected over
//! the merges since the last green commit to the first red one, and one fix
//! item is queued onto the integration branch. `status` shows each branch.

mod bisect;
mod branch;
mod fix;
mod full;
mod github;
mod promote;
mod state;
mod verify;

use std::sync::Arc;

use archon_core::config::IntegrationConfig;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::integration::{IntegrationStatus, short};
use archon_core::notify::NotificationKind;
use archon_core::repo::{Repo, ShipMode};

use self::branch::{Head, Source};
pub use self::fix::FIX_TAG;
use self::github::GithubRepo;
pub use self::promote::{Promoter, promote};
use self::state::{BranchState, Verdict};
use crate::context::DaemonContext;
use crate::feed;
use crate::resolve::RepoEnv;

/// How one look at a branch is verified, read from its repository's tiers.
#[derive(Debug, Clone)]
struct Watch {
    repo: Repo,
    branch: String,
    release: String,
    full: Option<String>,
    github: Option<GithubRepo>,
    source: Source,
}

impl Watch {
    /// `None` when the repository has no integration branch.
    fn of(ctx: &DaemonContext, repo: &Repo) -> Result<Option<Self>, ArchonError> {
        let tiers = RepoEnv::new(Some(repo.clone()))?.tiers()?;
        let Some(branch) = tiers.integration_branch.clone() else {
            return Ok(None);
        };
        let remote = repo.ship == ShipMode::Pr;
        let github = match remote {
            true => crate::worktree::stack::origin_url(&repo.path)?
                .as_deref()
                .and_then(github::github_repo),
            false => None,
        };
        let source = Source {
            repo: repo.path.clone(),
            data_dir: ctx.paths.data_dir.clone(),
            remote,
        };
        assert_ne!(branch, tiers.release_branch(), "validated: two branches");
        Ok(Some(Self {
            repo: repo.clone(),
            release: tiers.release_branch().to_owned(),
            full: crate::executor::fast_check::trusted_full(repo)?,
            branch,
            github,
            source,
        }))
    }
}

/// The branch as one look read it.
#[derive(Debug, Clone)]
struct Reading {
    head: Head,
    /// The recorded last green commit, while it is still on the branch.
    green: Option<String>,
    /// `green`, else where the branch left the release branch.
    baseline: Option<String>,
    /// The merges after `baseline` up to the head, oldest first.
    since_green: Vec<String>,
    /// The merges after the last verified head.
    unverified: usize,
}

impl Watch {
    /// Read the head and the merges since green and since the last verdict;
    /// `None` when the branch does not exist yet.
    fn read(&self, state: &BranchState) -> Result<Option<Reading>, ArchonError> {
        let Some(head) = self.source.head(&self.branch)? else {
            return Ok(None);
        };
        let tip = head.commit.as_str();
        let green = match &state.last_green {
            Some(green) if self.source.contains(tip, green)? => Some(green.clone()),
            _ => None,
        };
        let baseline = match &green {
            Some(green) => Some(green.clone()),
            None => self.source.fork_point(&self.release, tip)?,
        };
        let since_green = match &baseline {
            Some(base) => self.source.merges(base, tip)?,
            None => Vec::new(),
        };
        let unverified = match &state.verdict {
            Some(v) if self.source.contains(tip, &v.commit)? => {
                self.source.merges(&v.commit, tip)?.len()
            }
            _ => since_green.len(),
        };
        Ok(Some(Reading {
            head,
            green,
            baseline,
            since_green,
            unverified,
        }))
    }
}

/// Verify the integration branches until the daemon stops; only a store
/// failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    settle_interrupted(&ctx).await?;
    let every = ctx.config.integration.poll_every.duration();
    assert!(!every.is_zero(), "validated: a pause between passes");
    loop {
        pass(&ctx).await?;
        tokio::time::sleep(every).await;
    }
}

/// A run the daemon stopped in the middle of verifies nothing now.
async fn settle_interrupted(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    for (repo, mut state) in state::all(&store)? {
        if state.verifying.take().is_some() {
            state::save(&mut store, &repo, &state)?;
        }
    }
    Ok(())
}

/// One look at every repository's integration branch, verifying those due.
/// A repository that cannot be looked at keeps its problem in its state.
pub async fn pass(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let repos = ctx.store.lock().await.repos()?;
    for repo in &repos {
        match examine(ctx, repo).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => record_problem(ctx, repo, &error.to_string()).await?,
        }
    }
    let mut store = ctx.store.lock().await;
    for (id, _) in state::all(&store)? {
        if repos.iter().all(|repo| repo.id != id) {
            state::forget(&mut store, &id)?;
        }
    }
    let kept: Vec<RepoId> = repos.iter().map(|repo| repo.id.clone()).collect();
    promote::forget_unregistered(&mut store, &kept)
}

/// The integration branches, as `status` shows them.
pub async fn status(ctx: &DaemonContext) -> Result<Vec<IntegrationStatus>, ArchonError> {
    let store = ctx.read_store().await?;
    let repos = store.repos()?;
    let name = |id: &RepoId| repos.iter().find(|r| &r.id == id).map(|r| r.name.clone());
    Ok(state::all(&store)?
        .into_iter()
        .filter_map(|(id, state)| Some(state.status(&name(&id)?)))
        .collect())
}

async fn examine(ctx: &DaemonContext, repo: &Repo) -> Result<(), ArchonError> {
    let Some(watch) = Watch::of(ctx, repo)? else {
        return state::forget(&mut *ctx.store.lock().await, &repo.id);
    };
    let loaded = state::load(&*ctx.store.lock().await, &repo.id)?;
    let mut state = match loaded {
        Some(state) if state.branch == watch.branch => state,
        // Saved first, so a failing first read is shown on this branch.
        _ => {
            let fresh = BranchState::new(&watch.branch);
            state::save(&mut *ctx.store.lock().await, &repo.id, &fresh)?;
            fresh
        }
    };
    // Running GitHub checks are read again on this look.
    state.verifying = None;
    let told = state.problem.clone();
    let reading = {
        let (watch, state) = (watch.clone(), state.clone());
        blocking(move || watch.read(&state)).await?
    };
    let Some(reading) = reading else {
        state.set_problem(&format!("the branch `{}` does not exist yet", watch.branch));
        return save(ctx, repo, &told, &state).await;
    };
    let now = archon_core::now();
    state.observe(&reading.head.commit, now);
    state.merges_since_green = u32::try_from(reading.since_green.len()).unwrap_or(u32::MAX);
    // A force-push may have left the recorded green commit off the branch.
    state.last_green = reading.green.clone();
    let red_on_head = |state: &BranchState| state.verdict_on_head().is_some_and(|v| !v.green);
    // A green full check stood in for GitHub, whose checks may judge the head later.
    let read_again = |state: &BranchState| {
        state.verdict_on_head().is_some_and(|v| {
            v.via == verify::VIA_GITHUB
                || (v.green && v.via == verify::VIA_FULL && watch.github.is_some())
        })
    };
    if state.verdict_on_head().is_none() && due(&ctx.config.integration, &state, &reading, now) {
        let verified = verify::head(ctx, &watch, &mut state, &reading).await;
        kept(repo, &mut state, verified)?;
    } else if read_again(&state) {
        // A rerun may have turned the head since, so it is read before an owed bisection.
        let rechecked = verify::recheck(ctx, &watch, &mut state, &reading).await;
        kept(repo, &mut state, rechecked)?;
    } else if state.bisecting.is_some() && red_on_head(&state) {
        let resumed = verify::resume(ctx, &watch, &mut state, &reading).await;
        kept(repo, &mut state, resumed)?;
    }
    // A head whose checks run again is repaired once they judge it.
    if red_on_head(&state) && state.bisecting.is_none() && state.verifying.is_none() {
        ensure_fix(ctx, &watch, &mut state, &reading).await?;
    }
    save(ctx, repo, &told, &state).await
}

/// A verification that failed is the branch's problem, and what it found
/// so far is kept: a red verdict and the bisection it owes go on next look.
fn kept(
    repo: &Repo,
    state: &mut BranchState,
    result: Result<(), ArchonError>,
) -> Result<(), ArchonError> {
    let error = match result {
        Err(error) if !error.is_store() => error,
        other => return other,
    };
    tracing::warn!(repo = %repo.name, %error, "integration branch verification failed");
    state.verifying = None;
    state.set_problem(&error.to_string());
    assert!(state.problem.is_some(), "the failure is shown");
    Ok(())
}

/// A head is verified once enough merges landed since the last verdict, or
/// once the branch stood still long enough.
fn due(
    config: &IntegrationConfig,
    state: &BranchState,
    reading: &Reading,
    now: archon_core::Timestamp,
) -> bool {
    assert_eq!(
        state.head.as_ref(),
        Some(&reading.head.commit),
        "observed first"
    );
    if reading.unverified >= config.batch_merges as usize {
        return true;
    }
    let since = state.head_since.expect("an observed head has a time");
    let idle =
        chrono::Duration::from_std(config.idle_after.duration()).unwrap_or(chrono::Duration::MAX);
    now - since >= idle
}

/// Queue the fix item for a red head unless [`fix::repair`] finds one owed
/// no more; a refused enqueue is the branch's problem, tried again next look.
async fn ensure_fix(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    reading: &Reading,
) -> Result<(), ArchonError> {
    let verdict = state.verdict_on_head().cloned().expect("a red head");
    assert!(!verdict.green, "only a red head is repaired");
    let merges = {
        let (source, commits) = (watch.source.clone(), reading.since_green.clone());
        blocking(move || {
            commits
                .iter()
                .map(|c| Ok(format!("{} {}", short(c), source.subject(c)?)))
                .collect::<Result<Vec<_>, ArchonError>>()
        })
        .await?
    };
    let red = fix::Red {
        head: verdict.commit,
        via: verdict.via,
        excerpt: verdict.excerpt,
        culprit: state.culprit.clone(),
        merges,
    };
    match fix::repair(ctx, &watch.repo, state, &red).await {
        Ok(_) => Ok(()),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            state.set_problem(&format!("no fix item was queued: {error}"));
            Ok(())
        }
    }
}

/// Note a green verdict on the head, telling the operator when it was red.
async fn turned_green(
    ctx: &DaemonContext,
    watch: &Watch,
    state: &mut BranchState,
    verdict: Verdict,
) -> Result<(), ArchonError> {
    assert!(verdict.green, "a green verdict");
    let was_red = state.verdict.as_ref().is_some_and(|v| !v.green);
    state.last_green = Some(verdict.commit.clone());
    state.merges_since_green = 0;
    state.culprit = None;
    state.bisecting = None;
    let text = format!(
        "The integration branch `{}` of `{}` is green again at {} ({}).",
        watch.branch,
        watch.repo.name,
        short(&verdict.commit),
        verdict.via
    );
    state.verdict = Some(verdict);
    if was_red {
        feed::notify(ctx, NotificationKind::Outcome, text, None, None).await?;
    }
    Ok(())
}

/// Record `problem` in `repo`'s state, if its integration branch has one.
async fn record_problem(
    ctx: &DaemonContext,
    repo: &Repo,
    problem: &str,
) -> Result<(), ArchonError> {
    tracing::warn!(repo = %repo.name, problem, "integration branch not verified");
    let Some(mut state) = state::load(&*ctx.store.lock().await, &repo.id)? else {
        return Ok(());
    };
    let told = state.problem.clone();
    state.verifying = None;
    state.set_problem(problem);
    save(ctx, repo, &told, &state).await
}

/// Save `state`; a problem other than the one `told` before is posted to the feed.
async fn save(
    ctx: &DaemonContext,
    repo: &Repo,
    told: &Option<String>,
    state: &BranchState,
) -> Result<(), ArchonError> {
    state::save(&mut *ctx.store.lock().await, &repo.id, state)?;
    let Some(problem) = state.problem.as_ref().filter(|p| Some(*p) != told.as_ref()) else {
        return Ok(());
    };
    let note = format!(
        "Archon could not verify the integration branch `{}` of `{}`: {problem}. It tries again \
         every {}.",
        state.branch, repo.name, ctx.config.integration.poll_every
    );
    feed::system(ctx, note).await.map(drop)
}

/// Run blocking git or filesystem `work` off the async runtime.
async fn blocking<T, F>(work: F) -> Result<T, ArchonError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ArchonError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ArchonError::io("the integration branch task", std::io::Error::other(e)))?
}
