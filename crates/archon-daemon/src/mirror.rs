//! The orchestrator's mirrors of the registered repositories (ARCHITECTURE
//! §13.3): `<orchestrator workspace>/repos/<name>` is a copy with no remote,
//! checked out detached at the base branch, whose git directory is under the
//! data directory. The orchestrator and its helpers read code there, so a
//! code question needs no queue item, and nothing done in a mirror reaches
//! the operator's checkout, its objects or its remotes. A mirror's work tree
//! is read-only on disk (`lock`), and a change found anyway is undone and
//! reported. One worker keeps the mirrors current; a refresh that fails is
//! posted to the feed and told to the orchestrator, and blocks neither.

pub(crate) mod git;
mod lock;
mod reach;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model::ModelChoice;
use archon_core::policy::ModelPolicy;
use archon_core::repo::Repo;
use archon_queue::store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

pub use self::reach::{Reach, reach};
use crate::context::DaemonContext;
use crate::feed;
use crate::triggers::{self, TriggerEvent};

/// The directory of the orchestrator's workspace that holds the mirrors.
pub const MIRRORS_DIR: &str = "repos";
/// The directory of the orchestrator's workspace for work in no repository.
pub const SCRATCH_DIR: &str = "scratch";
/// Under the data directory, outside the orchestrator's workspace: the hooks
/// every mirror runs, which is the one that refuses a push.
const HOOKS_DIR: &str = "mirror-hooks";
/// Under the data directory: each mirror's git directory, by name.
const GIT_DIRS: &str = "mirror-git";
/// `kv`: the [`MirrorState`] of one repository, by id.
const STATE_KEY_PREFIX: &str = "mirror_";
const MAX_DETAIL_CHARS: usize = 400;

/// Wakes the mirror worker with the mirrors it is asked to refresh.
#[derive(Debug, Default)]
pub struct MirrorQueue {
    wanted: Notify,
    asked: std::sync::Mutex<Asked>,
}

/// The mirrors a pass refreshes besides those with a problem.
#[derive(Debug, Default)]
struct Asked {
    all: bool,
    repos: BTreeSet<RepoId>,
}

impl MirrorQueue {
    /// Ask for every mirror to be brought up to date soon. Never blocks:
    /// requests made while the worker is busy share its next pass.
    pub fn request(&self) {
        self.asked().all = true;
        self.wanted.notify_one();
    }

    /// Ask for `repo`'s mirror alone, as when one of its items finished.
    pub fn request_repo(&self, repo: &RepoId) {
        self.asked().repos.insert(repo.clone());
        self.wanted.notify_one();
    }

    fn take(&self) -> Asked {
        std::mem::take(&mut *self.asked())
    }

    fn asked(&self) -> std::sync::MutexGuard<'_, Asked> {
        // Every write leaves the set whole, so a panicked holder corrupted nothing.
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Where the mirrors of an orchestrator workspace live.
pub fn mirrors_root(workspace: &Path) -> PathBuf {
    workspace.join(MIRRORS_DIR)
}

/// The push guard's directory, which every daemon-owned checkout's config names.
pub(crate) fn hooks_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(HOOKS_DIR)
}

/// Where the mirrors' git directories live, one per repository name.
pub fn git_dirs_root(data_dir: &Path) -> PathBuf {
    data_dir.join(GIT_DIRS)
}

/// Bring the mirrors up to date at start and whenever one is asked for,
/// until the daemon stops; only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    sync_all(&ctx).await?;
    loop {
        ctx.mirrors.wanted.notified().await;
        refresh_requested(&ctx).await?;
    }
}

/// One pass over every mirror; see `sync`.
pub async fn sync_all(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let every = Asked {
        all: true,
        repos: BTreeSet::new(),
    };
    sync(ctx, &every).await
}

/// One pass over the mirrors asked for since the last, and those with a problem.
pub async fn refresh_requested(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let asked = ctx.mirrors.take();
    sync(ctx, &asked).await
}

/// One pass: a mirror at its base branch per registered repository and
/// nothing else; one that cannot be made or refreshed is reported, not fatal.
/// Only the repositories [`selected`] are refreshed.
async fn sync(ctx: &DaemonContext, asked: &Asked) -> Result<(), ArchonError> {
    let repos = ctx.store.lock().await.repos()?;
    let layout = Layout::of(ctx);
    let prepared = {
        let layout = layout.clone();
        blocking(move || layout.prepare()).await
    };
    if let Err(error) = prepared {
        // Without its directories no mirror can be made: each repository says so.
        for repo in &repos {
            record(ctx, repo, Outcome::Stale(error.to_string())).await?;
        }
        return Ok(());
    }
    for repo in selected(ctx, &repos, asked).await? {
        let recorded = stored_state(&*ctx.store.lock().await, &repo.id)?.commit;
        let plan = Plan::new(ctx, &layout, repo);
        let (outcome, undone) = blocking(move || Ok(plan.refresh(recorded.as_deref())))
            .await
            .unwrap_or_else(|error| (Outcome::Stale(error.to_string()), None));
        record(ctx, repo, outcome).await?;
        if let Some(undone) = undone {
            report_undone(ctx, repo, &undone).await?;
        }
    }
    forget_unregistered(ctx, &layout, &repos).await
}

/// The `repos` a pass refreshes: all when all were asked for, else those asked for
/// and those whose mirror has a problem or was never made.
async fn selected<'a>(
    ctx: &DaemonContext,
    repos: &'a [Repo],
    asked: &Asked,
) -> Result<Vec<&'a Repo>, ArchonError> {
    if asked.all {
        return Ok(repos.iter().collect());
    }
    let store = ctx.store.lock().await;
    let mut chosen = Vec::new();
    for repo in repos {
        let state = stored_state(&store, &repo.id)?;
        let settled = state.problem.is_none() && state.commit.is_some();
        if asked.repos.contains(&repo.id) || !settled {
            chosen.push(repo);
        }
    }
    Ok(chosen)
}

/// The directories mirrors are made in.
#[derive(Debug, Clone)]
struct Layout {
    workspace: PathBuf,
    root: PathBuf,
    git_dirs: PathBuf,
    hooks: PathBuf,
}

impl Layout {
    fn of(ctx: &DaemonContext) -> Self {
        let workspace = ctx.paths.orchestrator_workspace.clone();
        assert!(workspace.is_absolute(), "daemon paths are absolute");
        Self {
            root: mirrors_root(&workspace),
            git_dirs: git_dirs_root(&ctx.paths.data_dir),
            hooks: hooks_dir(&ctx.paths.data_dir),
            workspace,
        }
    }

    /// Create the mirrors and scratch directories and the push guard, first
    /// removing a symlink in the mirrors' place, where git would follow it.
    fn prepare(&self) -> Result<(), ArchonError> {
        if git::kind(&self.root)?.is_some_and(|kind| kind.is_symlink()) {
            std::fs::remove_file(&self.root)
                .map_err(|e| ArchonError::io(format!("removing {}", self.root.display()), e))?;
        }
        let scratch = self.workspace.join(SCRATCH_DIR);
        for dir in [&self.root, &scratch, &self.git_dirs, &self.hooks] {
            std::fs::create_dir_all(dir)
                .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
        }
        assert!(
            git::is_real_dir(&self.root)?,
            "the mirrors directory is real"
        );
        git::write_push_guard(&self.hooks)
    }
}

/// Everything one repository's refresh needs, owned so it runs off the
/// async runtime.
#[derive(Debug)]
struct Plan {
    repo: Repo,
    mirror: git::Mirror,
    global: ModelPolicy,
    orchestrator: ModelChoice,
}

/// How one repository's refresh ended.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The mirror is checked out at this commit of the base branch.
    Current(String),
    /// The repository's model policy refuses the orchestrator: no mirror.
    Withheld(String),
    /// The refresh failed; the mirror, if any, is where it was.
    Stale(String),
}

impl Plan {
    fn new(ctx: &DaemonContext, layout: &Layout, repo: &Repo) -> Self {
        assert!(Repo::valid_name(&repo.name), "registered names are valid");
        Self {
            repo: repo.clone(),
            mirror: git::Mirror {
                work: layout.root.join(&repo.name),
                git_dir: layout.git_dirs.join(&repo.name),
                hooks: layout.hooks.clone(),
            },
            global: ctx.config.models.policy(),
            orchestrator: ModelChoice {
                model: ctx.config.orchestrator.model.clone(),
                effort: ctx.config.orchestrator.effort,
                tier: None,
            },
        }
    }

    /// Refresh the mirror last checked out at `recorded`. Besides the outcome,
    /// what was changed in it since, when a good refresh undid that.
    fn refresh(&self, recorded: Option<&str>) -> (Outcome, Option<String>) {
        let config = match RepoConfig::load(&self.repo.path) {
            Ok(config) => config,
            Err(error) => return (Outcome::Stale(error.to_string()), None),
        };
        let policy = self.global.narrowed_by(&config.models.policy());
        if let Err(refusal) = policy.check(&self.orchestrator) {
            let removed = lock::open(&self.mirror.work)
                .and_then(|()| git::remove_entry(&self.mirror.work))
                .and_then(|()| git::remove_entry(&self.mirror.git_dir));
            let outcome = match removed {
                Ok(()) => Outcome::Withheld(refusal.to_string()),
                Err(error) => Outcome::Stale(error.to_string()),
            };
            return (outcome, None);
        }
        let base = self.repo.base_branch.clone().or(config.repo.base_branch);
        match self.bring_to(base.as_deref(), recorded) {
            Ok((commit, undone)) => (Outcome::Current(commit), undone),
            Err(error) => (Outcome::Stale(error.to_string()), None),
        }
    }

    /// Make the mirror if it is missing, then check it out at the commit
    /// `base` (or the repository's `HEAD`) names now, its work tree opened
    /// only meanwhile. Returns that commit, and what was changed in the
    /// mirror since the daemon checked out `recorded`.
    fn bring_to(
        &self,
        base: Option<&str>,
        recorded: Option<&str>,
    ) -> Result<(String, Option<String>), ArchonError> {
        let base = git::Base::resolve(&self.repo.path, base)?;
        let undone = self.changes_since(&base, recorded)?;
        lock::open(&self.mirror.work)?;
        let checked_out = self.check_out(&base);
        let closed = lock::close(&self.mirror.work);
        let commit = checked_out?;
        closed?;
        Ok((commit, undone))
    }

    /// What was changed in an existing mirror since `recorded`; `None` for
    /// a mirror not made yet or never checked out.
    fn changes_since(
        &self,
        base: &git::Base,
        recorded: Option<&str>,
    ) -> Result<Option<String>, ArchonError> {
        let Some(recorded) = recorded else {
            return Ok(None);
        };
        let made = git::is_real_dir(&self.mirror.git_dir)? && git::is_real_dir(&self.mirror.work)?;
        if !made {
            return Ok(None);
        }
        self.mirror.changes_since(base, recorded)
    }

    /// Make the mirror if it is missing and check it out at `base`.
    fn check_out(&self, base: &git::Base) -> Result<String, ArchonError> {
        self.mirror.prepare(base)?;
        assert!(
            git::is_real_dir(&self.mirror.git_dir)?,
            "a mirror was just made"
        );
        if self.mirror.head(base)?.as_deref() != Some(base.commit.as_str()) {
            self.mirror.fetch(&self.repo.path, base)?;
        }
        // Also when the base stood still: a tracked file changed anyway is reset.
        self.mirror.checkout(base)?;
        assert_eq!(
            self.mirror.head(base)?.as_deref(),
            Some(base.commit.as_str()),
            "the mirror is at the base commit"
        );
        Ok(base.commit.clone())
    }
}

/// What the store remembers of a mirror, so a problem is reported once and
/// its end is reported too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct MirrorState {
    /// The base commit of its last good refresh.
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    problem: Option<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Problem {
    /// `stale` or `withheld`.
    state: String,
    detail: String,
}

fn state_key(repo: &RepoId) -> String {
    format!("{STATE_KEY_PREFIX}{repo}")
}

/// What the store remembers of `repo`'s mirror; nothing yet for a new one.
fn stored_state(store: &Store, repo: &RepoId) -> Result<MirrorState, ArchonError> {
    let key = state_key(repo);
    match store.kv_get(&key)? {
        None => Ok(MirrorState::default()),
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}"))),
    }
}

/// Remember how the refresh of `repo` ended. A problem that began, changed
/// or ended is posted to the feed and held as quiet news for the orchestrator.
async fn record(ctx: &DaemonContext, repo: &Repo, outcome: Outcome) -> Result<(), ArchonError> {
    let key = state_key(&repo.id);
    let (before, after) = {
        let mut store = ctx.store.lock().await;
        let before = stored_state(&store, &repo.id)?;
        let after = state_after(&before, outcome);
        if after == before {
            return Ok(());
        }
        store.kv_set(&key, &serde_json::to_string(&after)?)?;
        (before, after)
    };
    if after.problem == before.problem {
        return Ok(());
    }
    let (state, detail, note) = announcement(&repo.name, after.problem.as_ref());
    tracing::warn!(repo = %repo.name, state, %detail, "mirror state changed");
    feed::system(ctx, note).await?;
    let event = TriggerEvent::Mirror {
        repo: repo.name.clone(),
        state: state.to_owned(),
        detail,
    };
    triggers::push_quiet(ctx, event).await
}

fn state_after(before: &MirrorState, outcome: Outcome) -> MirrorState {
    let problem = |state: &str, detail: String| {
        Some(Problem {
            state: state.to_owned(),
            detail: detail.chars().take(MAX_DETAIL_CHARS).collect(),
        })
    };
    match outcome {
        Outcome::Current(commit) => MirrorState {
            commit: Some(commit),
            problem: None,
        },
        Outcome::Withheld(why) => MirrorState {
            commit: None,
            problem: problem("withheld", why),
        },
        Outcome::Stale(why) => MirrorState {
            commit: before.commit.clone(),
            problem: problem("stale", why),
        },
    }
}

/// The news of a mirror's `problem`, or of its end: the state, the detail
/// for the orchestrator, and the line for the operator's feed.
fn announcement(repo: &str, problem: Option<&Problem>) -> (&'static str, String, String) {
    let Some(problem) = problem else {
        let detail = "refreshed at the base branch".to_owned();
        let note = format!("The orchestrator's mirror of `{repo}` is current again.");
        return ("current", detail, note);
    };
    let detail = problem.detail.clone();
    match problem.state.as_str() {
        "withheld" => {
            let note = format!(
                "The orchestrator gets no mirror of `{repo}`: the repository's [models] refuses \
                 the orchestrator's model ({detail}). It reads that repository through `ask` items."
            );
            ("withheld", detail, note)
        }
        _ => {
            let note = format!(
                "Archon could not refresh the orchestrator's mirror of `{repo}`, which may be \
                 missing or behind its base branch: {detail}. It is tried again when an item \
                 finishes or the operator writes."
            );
            ("stale", detail, note)
        }
    }
}

/// Tell the operator's feed and the orchestrator that the mirror of `repo`
/// had been changed (`undone` says how) and was put back at its base.
async fn report_undone(ctx: &DaemonContext, repo: &Repo, undone: &str) -> Result<(), ArchonError> {
    assert!(!undone.is_empty(), "a change was found");
    let undone: String = undone.chars().take(MAX_DETAIL_CHARS).collect();
    tracing::warn!(repo = %repo.name, %undone, "a write into a read-only mirror was undone");
    let note = format!(
        "Something in the orchestrator's workspace wrote into its read-only mirror of `{}` \
         ({undone}). Archon put the mirror back at its base commit: a change to a repository \
         ships only as a queued item.",
        repo.name
    );
    feed::system(ctx, note).await?;
    let event = TriggerEvent::Mirror {
        repo: repo.name.clone(),
        state: "reverted".to_owned(),
        detail: format!(
            "mirrors are read-only; this write was undone: {undone}. Queue the change."
        ),
    };
    triggers::push(ctx, event).await
}

/// Remove everything in the mirrors directory but the mirrors of `repos`, and
/// forget the state of repositories no longer registered.
async fn forget_unregistered(
    ctx: &DaemonContext,
    layout: &Layout,
    repos: &[Repo],
) -> Result<(), ArchonError> {
    let names: BTreeSet<String> = repos.iter().map(|repo| repo.name.clone()).collect();
    let roots = [layout.root.clone(), layout.git_dirs.clone()];
    let removed = blocking(move || {
        roots
            .iter()
            .try_for_each(|root| git::remove_all_but(root, &names))
    })
    .await;
    if let Err(error) = removed {
        tracing::warn!(%error, "an unregistered mirror was not removed; retried next pass");
    }
    let mut store = ctx.store.lock().await;
    for (key, _) in store.kv_prefix(STATE_KEY_PREFIX)? {
        let id = key
            .strip_prefix(STATE_KEY_PREFIX)
            .expect("ranged by prefix");
        if repos.iter().all(|repo| repo.id.as_str() != id) {
            store.kv_delete(&key)?;
        }
    }
    Ok(())
}

/// The policy of an orchestrator's helper whose scope reads `reach`: the
/// global one narrowed by each repository whose mirror it reaches.
pub(crate) async fn helper_policy(
    ctx: &DaemonContext,
    reach: &Reach,
) -> Result<ModelPolicy, ArchonError> {
    let repos = ctx.store.lock().await.repos()?;
    let mut policy = ctx.config.models.policy();
    for repo in repos.iter().filter(|repo| reach.includes(&repo.name)) {
        match RepoConfig::load(&repo.path) {
            Ok(config) => policy = policy.narrowed_by(&config.models.policy()),
            // The dispatcher refuses such a repo's new work, and it has no
            // mirror; keep watching without its narrowing.
            Err(error) => {
                tracing::warn!(repo = %repo.name, %error, "repo config unreadable; its policy is not applied");
            }
        }
    }
    Ok(policy)
}

/// Run blocking git or filesystem `work` off the async runtime; a task that
/// panicked is that work's failure.
async fn blocking<T, F>(work: F) -> Result<T, ArchonError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ArchonError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ArchonError::io("the mirror refresh task", std::io::Error::other(e)))?
}
