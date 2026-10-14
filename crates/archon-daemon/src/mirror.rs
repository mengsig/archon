//! The orchestrator's mirrors of the registered repositories (ARCHITECTURE
//! §13.3): `<orchestrator workspace>/repos/<name>` is a `git clone --local`
//! with no remote, checked out detached at the base branch. The orchestrator
//! and its helpers read code there, so a code question needs no queue item,
//! and nothing done in a mirror reaches the operator's checkout or its
//! remotes. One worker keeps the mirrors current; a refresh that fails is
//! posted to the feed and told to the orchestrator, and blocks neither.

mod git;
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

/// Bring the mirrors up to date at start and whenever one is asked for,
/// until the daemon stops; only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    sync_all(&ctx).await?;
    loop {
        ctx.mirrors.wanted.notified().await;
        refresh_requested(&ctx).await?;
    }
}

/// One pass over every mirror; see [`sync`].
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
        let plan = Plan::new(ctx, &layout, repo);
        let outcome = blocking(move || Ok(plan.refresh()))
            .await
            .unwrap_or_else(|error| Outcome::Stale(error.to_string()));
        record(ctx, repo, outcome).await?;
    }
    forget_unregistered(ctx, &layout, &repos).await
}

/// The repositories of `repos` a pass refreshes: every one when all were
/// asked for, else those asked for and those whose mirror has a problem or
/// was never made.
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
    hooks: PathBuf,
}

impl Layout {
    fn of(ctx: &DaemonContext) -> Self {
        let workspace = ctx.paths.orchestrator_workspace.clone();
        assert!(workspace.is_absolute(), "daemon paths are absolute");
        Self {
            root: mirrors_root(&workspace),
            hooks: ctx.paths.data_dir.join(HOOKS_DIR),
            workspace,
        }
    }

    /// Create the mirrors and scratch directories and the push guard, first
    /// removing a symlink in the mirrors' place, where git would follow it.
    fn prepare(&self) -> Result<(), ArchonError> {
        if git::is_symlink(&self.root)? {
            std::fs::remove_file(&self.root)
                .map_err(|e| ArchonError::io(format!("removing {}", self.root.display()), e))?;
        }
        for dir in [&self.root, &self.workspace.join(SCRATCH_DIR), &self.hooks] {
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
    mirror: PathBuf,
    root: PathBuf,
    hooks: PathBuf,
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
            mirror: layout.root.join(&repo.name),
            root: layout.root.clone(),
            hooks: layout.hooks.clone(),
            global: ctx.config.models.policy(),
            orchestrator: ModelChoice {
                model: ctx.config.orchestrator.model.clone(),
                effort: ctx.config.orchestrator.effort,
            },
        }
    }

    fn refresh(&self) -> Outcome {
        let config = match RepoConfig::load(&self.repo.path) {
            Ok(config) => config,
            Err(error) => return Outcome::Stale(error.to_string()),
        };
        let policy = self.global.narrowed_by(&config.models.policy());
        if let Err(refusal) = policy.check(&self.orchestrator) {
            return match git::remove_entry(&self.mirror) {
                Ok(()) => Outcome::Withheld(refusal.to_string()),
                Err(error) => Outcome::Stale(error.to_string()),
            };
        }
        let base = self.repo.base_branch.clone().or(config.repo.base_branch);
        match self.bring_to(base.as_deref()) {
            Ok(commit) => Outcome::Current(commit),
            Err(error) => Outcome::Stale(error.to_string()),
        }
    }

    /// Make the mirror if it is missing, then check it out at the commit
    /// `base` (or the repository's `HEAD`) names now. Returns that commit.
    fn bring_to(&self, base: Option<&str>) -> Result<String, ArchonError> {
        let base = git::Base::resolve(&self.repo.path, base)?;
        if !git::is_mirror(&self.mirror)? {
            git::clone(&self.repo.path, &self.root, &self.mirror, &self.hooks)?;
        }
        assert!(git::is_mirror(&self.mirror)?, "a mirror was just made");
        if git::head(&self.mirror)?.as_deref() != Some(base.commit.as_str()) {
            git::fetch(&self.mirror, &self.repo.path, &base)?;
        }
        // Also when the base stood still: a tracked file a helper changed is reset.
        git::checkout(&self.mirror, &base.commit)?;
        assert_eq!(
            git::head(&self.mirror)?.as_deref(),
            Some(base.commit.as_str()),
            "the mirror is at the base commit"
        );
        Ok(base.commit)
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

/// Remove everything in the mirrors directory but the mirrors of `repos`, and
/// forget the state of repositories no longer registered.
async fn forget_unregistered(
    ctx: &DaemonContext,
    layout: &Layout,
    repos: &[Repo],
) -> Result<(), ArchonError> {
    let names: BTreeSet<String> = repos.iter().map(|repo| repo.name.clone()).collect();
    let root = layout.root.clone();
    let removed = blocking(move || git::remove_all_but(&root, &names)).await;
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
