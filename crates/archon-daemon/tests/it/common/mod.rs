//! Shared fixtures: a daemon context over temp directories, a scripted
//! agent backend, and a temp git repository carrying a test pipeline.
#![allow(dead_code)]

pub mod delivery;
pub mod fake_ops;
pub mod team;
pub mod team_worktree;

pub mod initiative;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use archon_aether::AetherLink;
use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::ports::{
    AgentBackend, AgentUpdate, CreatedNode, Descendant, DescendantState, NetworkPolicy,
    RunNodeRequest, SpawnRequest, SpawnedAgent, TreePulse,
};
use archon_core::protocol::{ClientKind, EnqueueRequest, Reply, RepoAdd, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use archon_daemon::context::{DaemonContext, DaemonPaths};
use archon_daemon::disk::DiskWatch;
use archon_daemon::executor::RunTable;
use archon_daemon::feed::ReplyTracker;
use archon_daemon::triggers::TriggerQueue;
use archon_daemon::{dispatcher, executor, handlers};
use archon_lavish::Lavish;
use archon_queue::Store;
use archon_queue::store::keys;
use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, broadcast};

/// The test pipeline, kept in the repository so no user pipeline shadows it:
/// `implement` → `review` (two agents, all must pass, loops once) → `ship`.
pub const PIPELINE: &str = "wp5-review";
pub const REPO_NAME: &str = "demo";
/// The orchestrator [`Harness::orchestrator`] makes its calls as.
pub const ORCHESTRATOR: &str = "agt_orchestrator";
pub const MODEL: &str = "claude-code/claude-sonnet-5";
pub const REVIEW_MODEL: &str = "codex/gpt-5";

const PIPELINE_TOML: &str = r#"[pipeline]
name = "wp5-review"
description = "test pipeline"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 2
gate = "all_pass"
timeout_minutes = 30
on_fail = { goto = "implement", max_loops = 1 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

/// A repository pipeline shaped like 1.0's `default-pr-review`: one reviewer
/// with up to two fix loops, then archond ships what `implement` records.
pub const REVIEWED: &str = "reviewed";
pub const REVIEWED_TOML: &str = r#"[pipeline]
name = "reviewed"
description = "implement, one review, then ship"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.ship]
stage = "implement"

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 120

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
gate = "all_pass"
timeout_minutes = 60
on_fail = { goto = "implement", max_loops = 2 }
"#;

const PROMPTS: [(&str, &str); 3] = [
    (
        "implement.md",
        // 1.0's `default-pr-review` prompts, which these tests were written for.
        include_str!("implement.md"),
    ),
    ("review.md", include_str!("review.md")),
    (
        "ship.md",
        // A pipeline of the user's own may still end in a ship agent.
        include_str!("ship.md"),
    ),
];

/// A daemon context over a temp directory with a registered repository.
pub struct Harness {
    pub ctx: Arc<DaemonContext>,
    pub backend: Arc<FakeBackend>,
    pub repo: PathBuf,
    pub dir: tempfile::TempDir,
    /// What the context's disk probe reads as free, on every path; plenty
    /// unless a test fills the fake disk.
    pub free_bytes: Arc<AtomicU64>,
    /// The installer and restart helper an upgrade uses.
    pub ops: Arc<fake_ops::FakeOps>,
}

/// Give the owner write access under `path` again, as `chmod -R u+w` does:
/// what an agent set on writing into a read-only mirror could do.
pub fn writable_again(path: &Path) {
    let opened = Command::new("chmod")
        .arg("-R")
        .arg("u+w")
        .arg(path)
        .status()
        .expect("chmod runs");
    assert!(opened.success(), "chmod -R u+w {}", path.display());
}

/// archond leaves the orchestrator's mirrors read-only, and a temporary
/// directory's removal cannot enter them: open them before it runs.
pub fn open_mirrors_for_removal(ctx: &DaemonContext) {
    let mirrors = archon_daemon::mirror::mirrors_root(&ctx.paths.orchestrator_workspace);
    if mirrors.exists() && !std::thread::panicking() {
        writable_again(&mirrors);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        open_mirrors_for_removal(&self.ctx);
    }
}

impl Harness {
    /// Default config with the scripted backend. A refused review fails the
    /// item at once: the automatic repair retry is off unless a test asks.
    pub async fn new() -> Self {
        let mut config = Config::default();
        config.review.auto_repair = false;
        Self::with_config(config).await
    }

    pub async fn with_config(config: Config) -> Self {
        let backend = Arc::new(FakeBackend::default());
        let used = backend.clone();
        Self::build(config, backend, move |_| used).await
    }

    /// The scripted backend with Lavish, e.g. the fake `lavish-axi`.
    pub async fn with_lavish(config: Config, lavish: Lavish) -> Self {
        let backend = Arc::new(FakeBackend::default());
        let used = backend.clone();
        Self::assemble(config, backend, move |_| used, Some(lavish)).await
    }

    /// `make` receives the context's Aether link (for `AetherBackend`);
    /// `fake` is the handle tests inspect when `make` returns it.
    pub async fn build(
        config: Config,
        fake: Arc<FakeBackend>,
        make: impl FnOnce(Arc<AetherLink>) -> Arc<dyn AgentBackend>,
    ) -> Self {
        Self::assemble(config, fake, make, None).await
    }

    async fn assemble(
        mut config: Config,
        fake: Arc<FakeBackend>,
        make: impl FnOnce(Arc<AetherLink>) -> Arc<dyn AgentBackend>,
        lavish: Option<Lavish>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        if config.github.binary == Path::new("gh") {
            config.github.binary = root.join("fake-gh");
            std::fs::write(
                &config.github.binary,
                r#"#!/bin/sh
if [ "$1 $2" = "pr edit" ]; then exit 0; fi
# GitHub shows the pull request a URL names as open.
if [ "$1 $2 $4" = "pr view --json" ]; then
  printf '{"url":"%s","number":%s,"headRefName":"shown","state":"OPEN","baseRefName":"main"}' "$3" "${3##*/}"
  exit 0
fi
[ "$1 $2" = "pr list" ] || exit 2
# GitHub lists a repository's open pull requests as none, a branch's as one.
if [ "$3 $4 $5 $6 $7" = "--state open --limit 1000 --json" ]; then printf '[]'; exit 0; fi
[ "$3 $5 $7 $8 $9" = "--head --state --limit 20 --json" ] || exit 2
printf '[{"url":"https://github.com/o/r/pull/1","number":1,"headRefName":"%s","state":"OPEN","baseRefName":"main"}]' "$4"
"#,
            )
            .unwrap();
            std::fs::set_permissions(
                &config.github.binary,
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let repo = init_repo(&root.join("repo"));
        let aether = Arc::new(AetherLink::new());
        let backend = make(aether.clone());
        let free_bytes = Arc::new(AtomicU64::new(u64::MAX));
        let disk = fake_disk(&free_bytes);
        let ops = Arc::new(fake_ops::FakeOps::default());
        let ctx = context(&root, config, aether, backend, lavish, disk, &ops);
        let harness = Self {
            ctx: Arc::new(ctx),
            backend: fake,
            repo,
            dir,
            free_bytes,
            ops,
        };
        harness.add_repo().await;
        harness
    }

    async fn add_repo(&self) -> Repo {
        let add = RepoAdd {
            path: self.repo.clone(),
            name: Some(REPO_NAME.into()),
            ship: Some(ShipMode::Local),
            base_branch: Some("main".into()),
            default_pipeline: Some(PIPELINE.into()),
            auto_merge: false,
            github_watch: false,
        };
        let Ok(Reply::Repo(mut repo)) = self.cli(Request::RepoAdd(add)).await else {
            panic!("repo add failed");
        };
        // Registration refuses a pr repository without an origin, which most tests lack.
        repo.ship = ShipMode::Pr;
        self.ctx
            .store
            .lock()
            .await
            .update_repo(&repo)
            .expect("ship = pr");
        repo
    }

    /// Boxed, as a socket connection's task holds it: a debug build's deepest
    /// request would otherwise sit inside every test's own future.
    pub async fn cli(&self, request: Request) -> Result<Reply, ArchonError> {
        Box::pin(handlers::handle(&self.ctx, ClientKind::ArchonCli, request)).await
    }

    /// `request` as `archon mcp` forwards it for the orchestrator, which is
    /// [`ORCHESTRATOR`] unless the store already names another.
    pub async fn orchestrator(&self, request: Request) -> Result<Reply, ArchonError> {
        let caller = {
            let mut store = self.ctx.store.lock().await;
            if store.kv_get(keys::ORCHESTRATOR_AGENT)?.is_none() {
                store.kv_set(keys::ORCHESTRATOR_AGENT, ORCHESTRATOR)?;
            }
            store
                .kv_get(keys::ORCHESTRATOR_AGENT)?
                .expect("just stored")
        };
        let request = Request::ForAgent {
            caller: AetherAgentId::parse(&caller)?,
            request: Box::new(request),
        };
        Box::pin(handlers::handle(&self.ctx, ClientKind::ArchonMcp, request)).await
    }

    /// Enqueue `title` on the test pipeline with [`plan`].
    pub async fn enqueue(&self, title: &str) -> QueueItem {
        self.enqueue_with(title, plan()).await.expect("enqueue")
    }

    pub async fn enqueue_with(
        &self,
        title: &str,
        model_plan: ModelPlan,
    ) -> Result<QueueItem, ArchonError> {
        self.enqueue_in(PIPELINE, title, model_plan).await
    }

    /// Enqueue into `pipeline`, one of the repository's.
    pub async fn enqueue_in(
        &self,
        pipeline: &str,
        title: &str,
        model_plan: ModelPlan,
    ) -> Result<QueueItem, ArchonError> {
        self.enqueue_in_repo(REPO_NAME, pipeline, title, model_plan)
            .await
    }

    /// Enqueue into `pipeline` of the registered repository `repo`.
    pub async fn enqueue_in_repo(
        &self,
        repo: &str,
        pipeline: &str,
        title: &str,
        model_plan: ModelPlan,
    ) -> Result<QueueItem, ArchonError> {
        let request = EnqueueRequest {
            onto: None,
            stage_timeouts: Default::default(),
            repo: Some(RepoRef(repo.into())),
            pipeline: Some(pipeline.into()),
            title: title.into(),
            brief: format!("brief for {title}"),
            priority: Default::default(),
            depends_on: Vec::new(),
            tags: Vec::new(),
            model_plan: model_plan.into(),
            model_reasons: None,
            budget_usd: None,
            idempotency_key: None,
            autonomy: Default::default(),
            origin: None,
        };
        match self.cli(Request::Enqueue(request)).await? {
            Reply::Item(item) => Ok(item),
            other => panic!("enqueue replied {other:?}"),
        }
    }

    pub async fn item(&self, id: &QueueItemId) -> QueueItem {
        self.ctx.store.lock().await.item(id).expect("item")
    }

    pub async fn state(&self, id: &QueueItemId) -> QueueState {
        self.item(id).await.state
    }

    /// Every stage run of the item's runs, oldest run first.
    pub async fn stages(&self, id: &QueueItemId) -> Vec<StageRun> {
        let store = self.ctx.store.lock().await;
        let mut stages = Vec::new();
        for run in store.runs_for_item(id).expect("runs") {
            stages.extend(store.stage_runs(&run.id).expect("stage runs"));
        }
        stages
    }

    pub async fn live_stages(&self) -> Vec<StageRun> {
        self.ctx
            .store
            .lock()
            .await
            .live_stage_runs()
            .expect("live stages")
    }

    /// Running stage agents, oldest first.
    pub async fn running(&self) -> Vec<StageRun> {
        let mut running: Vec<StageRun> = self
            .live_stages()
            .await
            .into_iter()
            .filter(|s| s.state == StageState::Running)
            .collect();
        running.sort_by_key(|s| (s.started_at, s.fan_index));
        running
    }

    /// Settle every running stage agent with the outcome `answer` picks;
    /// returns the stage runs settled.
    pub async fn settle_running(
        &self,
        answer: impl Fn(&StageRun) -> StageOutcome,
    ) -> Vec<StageRun> {
        let running = self.running().await;
        for stage in &running {
            let agent = stage
                .aether_agent
                .clone()
                .expect("running stages have an agent");
            executor::stage_settled(&self.ctx, &agent, answer(stage))
                .await
                .expect("stage settles");
        }
        running
    }

    pub async fn dispatch(&self) {
        executor::coordination::reconcile(&self.ctx)
            .await
            .expect("coordination pass");
        dispatcher::dispatch_once(&self.ctx)
            .await
            .expect("dispatch pass");
    }

    /// The item's only run.
    pub async fn run(&self, id: &QueueItemId) -> PipelineRun {
        let mut runs = self.ctx.store.lock().await.runs_for_item(id).expect("runs");
        assert_eq!(runs.len(), 1, "one run for {id}");
        runs.remove(0)
    }

    /// Simulate a crash and restart: a fresh context (empty run table) over
    /// the same store and directories, sharing the backend.
    pub fn restart(&mut self) {
        let root = self.dir.path().canonicalize().expect("canonical tempdir");
        let config = self.ctx.config.clone();
        let backend = self.ctx.backend.clone();
        assert_eq!(
            Arc::strong_count(&self.ctx),
            1,
            "nothing else holds the old daemon"
        );
        let lavish = self.ctx.lavish.clone();
        let disk = fake_disk(&self.free_bytes);
        let link = Arc::new(AetherLink::new());
        let ctx = context(&root, config, link, backend, lavish, disk, &self.ops);
        self.ctx = Arc::new(ctx);
    }

    /// Set what the fake disk has free.
    pub fn set_free_bytes(&self, bytes: u64) {
        self.free_bytes.store(bytes, Ordering::SeqCst);
    }

    /// Notification texts of `kind` in the feed, oldest first.
    pub async fn notices(&self, kind: NotificationKind) -> Vec<String> {
        let feed = self
            .ctx
            .store
            .lock()
            .await
            .feed_after(0, 10_000)
            .expect("feed");
        feed.into_iter()
            .filter_map(|item| match item.entry {
                FeedEntry::Notification(n) if n.kind == kind => Some(n.text),
                _ => None,
            })
            .collect()
    }

    /// A `kv` value of the store.
    pub async fn kv(&self, key: &str) -> Option<String> {
        self.ctx.store.lock().await.kv_get(key).expect("kv")
    }

    /// Write `<repo>/.archon/config.toml`.
    pub fn repo_config(&self, toml: &str) {
        std::fs::write(self.repo.join(".archon/config.toml"), toml).expect("repo config");
    }

    /// Add a repository pipeline `name` whose stages use the test prompts.
    pub fn add_pipeline(&self, name: &str, toml: &str) {
        assert!(
            toml.contains(&format!("name = \"{name}\"")),
            "the TOML names {name}"
        );
        write_pipeline(&self.repo, name, toml);
    }
}

/// Free bytes that make the fake disk's probe hang for [`STALL`] first,
/// like a filesystem that stopped answering.
pub const STALLED_DISK: u64 = u64::MAX - 1;
pub const STALL: std::time::Duration = std::time::Duration::from_secs(3);

/// A disk watch whose every path has `free` bytes free; a disk-full hold
/// ends as soon as space is back.
pub fn fake_disk(free: &Arc<AtomicU64>) -> DiskWatch {
    let free = free.clone();
    let probe = Arc::new(move |_: &Path| {
        let bytes = free.load(Ordering::SeqCst);
        if bytes == STALLED_DISK {
            std::thread::sleep(STALL);
        }
        Ok(bytes)
    });
    DiskWatch::new(probe, std::time::Duration::ZERO)
}

/// A test never reaches the host's Docker; one that tests teardowns names
/// its fake `docker`.
pub fn without_host_docker(mut config: Config) -> Config {
    if config.worktrees.docker == Path::new("docker") {
        config.worktrees.docker = "/nonexistent/archon-test/docker".into();
    }
    config
}

/// Build a context like `archon_daemon::run` does, rooted at `root`.
pub fn context(
    root: &Path,
    config: Config,
    aether: Arc<AetherLink>,
    backend: Arc<dyn AgentBackend>,
    lavish: Option<Lavish>,
    disk: DiskWatch,
    ops: &Arc<fake_ops::FakeOps>,
) -> DaemonContext {
    let config = without_host_docker(config);
    let data_dir = root.join("data");
    let paths = DaemonPaths {
        socket: root.join("archond.sock"),
        store: data_dir.join("archon.db"),
        worktrees: root.join("worktrees"),
        orchestrator_workspace: data_dir.join("orchestrator"),
        scratch: data_dir.join("scratch"),
        reports: data_dir.join("reports"),
        logs: data_dir.join("logs"),
        archon_bin: PathBuf::from("/usr/bin/false"),
        data_dir,
    };
    create_context_dirs(&paths);
    let store = Store::open(&paths.store).expect("store");
    let readers = archon_daemon::readers::Readers::open(&store, &paths.store, 2).expect("readers");
    let operator_settings = archon_daemon::settings::SettingsBook::load(&store).expect("settings");
    let (feed, _) = broadcast::channel(1024);
    let dispatch = Arc::new(Notify::new());
    let remote =
        archon_remote::Remote::open(Default::default(), &paths.data_dir, "test").expect("remote");
    DaemonContext {
        storage: Default::default(),
        status_probes: Default::default(),
        quota: Default::default(),
        push: Default::default(),
        activity: Default::default(),
        config,
        paths,
        store: Mutex::new(store),
        readers,
        tracked: Default::default(),
        aether,
        backend,
        feed,
        dispatch: dispatch.clone(),
        coordination: Default::default(),
        runs: Mutex::new(RunTable::default()),
        replies: Mutex::new(ReplyTracker::default()),
        answering: Mutex::new(()),
        authority_changes: Mutex::new(()),
        learning_memory: Mutex::new(()),
        memory_curation: Mutex::new(()),
        memory_feedback_ready: Notify::new(),
        report_files: Arc::new(Mutex::new(())),
        triggers: TriggerQueue::default(),
        mirrors: archon_daemon::mirror::MirrorQueue::default(),
        spawn_recorded: Notify::new(),
        operator_settings: std::sync::Mutex::new(operator_settings),
        lavish,
        supervisor: archon_daemon::supervisor::Supervisor::default(),
        github: archon_daemon::github::GithubState::default(),
        started_at: archon_core::now(),
        disk,
        host: archon_daemon::host_load::HostWatch::system(),
        standby: OnceLock::new(),
        recovery: Default::default(),
        gc: Mutex::new(None),
        collecting: archon_daemon::gc::Collecting::new(dispatch),
        ops: ops.ops_state(),
        remote,
    }
}

fn create_context_dirs(paths: &DaemonPaths) {
    assert!(paths.store.starts_with(&paths.data_dir));
    assert!(paths.reports.starts_with(&paths.data_dir));
    for dir in [
        &paths.data_dir,
        &paths.worktrees,
        &paths.scratch,
        &paths.reports,
    ] {
        std::fs::create_dir_all(dir).expect("create dir");
    }
    assert!(paths.scratch.is_dir() && paths.worktrees.is_dir());
}

/// A git repository on `main` with one commit and the test pipeline.
pub fn init_repo(path: &Path) -> PathBuf {
    write_pipeline(path, PIPELINE, PIPELINE_TOML);
    std::fs::write(path.join("README.md"), "demo\n").expect("readme");
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    // The daemon's own git (team merges) runs with the repository's config.
    git(path, &["config", "user.name", "Archon Test"]);
    git(path, &["config", "user.email", "test@archon.invalid"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", "initial"]);
    path.to_owned()
}

/// What `ensure_worker` committed for a role, which it must have.
pub fn committed(
    kept: Option<archon_daemon::worktree::team::KeptWork>,
) -> archon_daemon::worktree::team::SavedWork {
    match kept {
        Some(archon_daemon::worktree::team::KeptWork::Committed(saved)) => saved,
        other => panic!("expected leftovers committed, got {other:?}"),
    }
}

/// `<repo>/.archon/pipelines/<name>/` with `toml` and the test prompts.
fn write_pipeline(repo: &Path, name: &str, toml: &str) {
    let pipeline = repo.join(".archon/pipelines").join(name);
    std::fs::create_dir_all(&pipeline).expect("pipeline dir");
    std::fs::write(pipeline.join("pipeline.toml"), toml).expect("pipeline.toml");
    for (file, text) in PROMPTS {
        std::fs::write(pipeline.join(file), text).expect("prompt");
    }
}

/// `git` with a throwaway identity, isolated from the user's config.
/// A repository that can be registered as `ship = pr`, which needs an origin.
pub fn init_repo_with_origin(path: &Path) -> PathBuf {
    let repo = init_repo(path);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/demo.git",
        ],
    );
    repo
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Archon Test",
            "-c",
            "user.email=test@archon.invalid",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

pub fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: model.parse().expect("model ref"),
        effort,
        tier: None,
    }
}

/// One implementer, two different reviewers, one shipper.
pub fn plan() -> ModelPlan {
    let mut stages = BTreeMap::new();
    stages.insert("implement".into(), vec![choice(MODEL, Effort::Medium)]);
    stages.insert(
        "review".into(),
        vec![
            choice(REVIEW_MODEL, Effort::Medium),
            choice(MODEL, Effort::Low),
        ],
    );
    stages.insert("ship".into(), vec![choice(MODEL, Effort::Low)]);
    ModelPlan(stages)
}

pub fn completed(summary: &str) -> StageOutcome {
    StageOutcome::Completed {
        summary: summary.into(),
        structured: None,
    }
}

pub fn verdict(pass: bool) -> StageOutcome {
    let word = if pass { "pass" } else { "fail" };
    StageOutcome::Completed {
        summary: format!("reviewed\nVERDICT: {word}"),
        structured: Some(serde_json::json!({"verdict": word, "findings": []})),
    }
}

/// An `AgentBackend` that records requests and answers from a script.
/// Like Aether, a repeated idempotency key returns the same agent only for
/// the same request (Aether fingerprints the frame) and refuses a different
/// one, a child may not state a wider network than its parent, and a
/// cancelled agent is no longer live. A stage agent ends with its task as
/// far as the fake knows: `is_live` reports one live only while a test
/// holds it in its turn (`hold_turn`, until `end_turn`), while a node is
/// live until cancelled or killed.
#[derive(Default)]
pub struct FakeBackend {
    state: StdMutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    host_context_supported: bool,
    /// Aether's `[network] default` is `denied`; `allowed`, as Aether
    /// ships, until a test says otherwise.
    network_denied: bool,
    /// Stage agents a test holds in their turn after their task settled.
    in_turn: BTreeSet<AetherAgentId>,
    requests: Vec<SpawnRequest>,
    by_key: BTreeMap<String, SpawnedAgent>,
    failures: VecDeque<ArchonError>,
    cancel_failures: VecDeque<ArchonError>,
    /// Agents whose accepted cancel leaves them live until `kill`.
    slow_cancels: BTreeSet<AetherAgentId>,
    outcome_failures: VecDeque<ArchonError>,
    /// Tasks whose `task_outcome` always fails with this Aether error.
    unreadable: BTreeMap<AetherTaskId, String>,
    lost_replies: u32,
    outcomes: BTreeMap<AetherTaskId, StageOutcome>,
    cancelled: Vec<AetherAgentId>,
    /// Every accepted `update_agent`, in order.
    updates: Vec<(AetherAgentId, AgentUpdate)>,
    update_failures: VecDeque<ArchonError>,
    /// Every `interrupt` call, in order.
    interrupts: Vec<AetherAgentId>,
    next: u32,
    /// The request each key was created with; a replay must repeat it.
    recorded: BTreeMap<String, SpawnRequest>,
    /// Every `create_run_node` call, including repeats.
    node_requests: Vec<RunNodeRequest>,
    node_by_key: BTreeMap<String, CreatedNode>,
    node_recorded: BTreeMap<String, RunNodeRequest>,
    node_failures: VecDeque<ArchonError>,
    lost_node_replies: u32,
    /// Agents `is_live` reports dead: cancelled ones and those `kill` took.
    dead: BTreeSet<AetherAgentId>,
    /// Agents Aether holds as failed, which `wake` revives; it refuses others.
    failed: BTreeSet<AetherAgentId>,
    /// Agents `wake` revived, in order; live until cancelled or killed.
    woken: Vec<AetherAgentId>,
    /// Every `is_live` call, in order.
    live_checks: Vec<AetherAgentId>,
    next_node: u32,
    /// Messages delivered, once per key, in order: key, recipient, content.
    delivered: Vec<(String, AetherAgentId, String)>,
    /// Every `send_message` call's key, including repeats and refused ones.
    message_calls: Vec<String>,
    message_failures: VecDeque<ArchonError>,
    /// Agents a stage agent delegated to, as Aether holds them.
    delegated: Vec<Descendant>,
    descendant_failures: VecDeque<ArchonError>,
    /// Every `descendants` call, in order.
    descendant_checks: Vec<AetherAgentId>,
    /// What a kept agent said to end the turn it was resumed in.
    answers: BTreeMap<AetherAgentId, StageOutcome>,
    /// Spawns whose objective holds the text wait for the gate to open.
    stalls: Vec<(String, Arc<tokio::sync::Semaphore>)>,
    /// What `pulse` reports of an agent's tree; an empty pulse otherwise.
    pulses: BTreeMap<AetherAgentId, TreePulse>,
    /// Runs once, after the next `update_agent` is recorded: what else
    /// happened while that update was in flight.
    update_hook: Option<Box<dyn FnOnce() + Send>>,
}

impl FakeState {
    fn knows(&self, agent: &AetherAgentId) -> bool {
        self.is_stage_agent(agent) || self.is_node(agent)
    }

    fn network_default(&self) -> NetworkPolicy {
        match self.network_denied {
            true => NetworkPolicy::Denied,
            false => NetworkPolicy::Allowed,
        }
    }

    fn is_stage_agent(&self, agent: &AetherAgentId) -> bool {
        self.by_key.values().any(|s| s.agent == *agent)
    }

    fn is_node(&self, agent: &AetherAgentId) -> bool {
        self.node_by_key.values().any(|n| n.agent == *agent)
    }

    /// Created long-lived: it outlives its task until it or its run node
    /// is cancelled, as Aether's cancel takes the node's subtree with it.
    fn is_kept(&self, agent: &AetherAgentId) -> bool {
        let kept = |key: &String| {
            let request = &self.recorded[key];
            request.keep
                && !request
                    .parent
                    .as_ref()
                    .is_some_and(|p| self.dead.contains(p))
        };
        self.by_key
            .iter()
            .any(|(key, s)| s.agent == *agent && kept(key))
    }

    /// The network a node's request stated.
    fn node_network(&self, node: &AetherAgentId) -> Option<NetworkPolicy> {
        let (key, _) = self.node_by_key.iter().find(|(_, n)| n.agent == *node)?;
        Some(self.node_recorded[key].network)
    }

    /// The Aether run of a node, which its children join.
    fn node_run(&self, node: &AetherAgentId) -> Option<AetherRunId> {
        let created = self.node_by_key.values().find(|n| n.agent == *node)?;
        Some(created.run.clone())
    }
}

/// Aether's `invalid_argument` for a key replayed with another frame.
fn conflict(key: &str) -> ArchonError {
    ArchonError::Invalid(format!(
        "aether: idempotency_key `{key}` already created an agent; a different request needs a new key"
    ))
}

/// Aether's `invalid_argument` for a child scope wider than its parent's.
fn widens(parent: &AetherAgentId) -> ArchonError {
    ArchonError::Invalid(format!(
        "aether: child scope must narrow its parent's: {parent} denies the network"
    ))
}

impl FakeBackend {
    pub fn set_host_context_supported(&self, supported: bool) {
        self.state().host_context_supported = supported;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The next `spawn` calls fail with these errors, in order.
    pub fn fail_next(&self, error: ArchonError) {
        self.state().failures.push_back(error);
    }

    /// The next `cancel` calls fail with these errors, in order.
    pub fn fail_next_cancel(&self, error: ArchonError) {
        self.state().cancel_failures.push_back(error);
    }

    /// A cancel of `agent` is accepted but, being cooperative, leaves it
    /// live (`is_live`) until `kill`.
    pub fn cancel_slowly(&self, agent: &AetherAgentId) {
        self.state().slow_cancels.insert(agent.clone());
    }

    /// The next `task_outcome` calls fail with these errors, in order.
    pub fn fail_next_outcome(&self, error: ArchonError) {
        self.state().outcome_failures.push_back(error);
    }

    /// The next `spawn` creates its agent but the reply is lost
    /// (`Unavailable`), like Aether acting before the daemon crashed.
    pub fn lose_next_reply(&self) {
        self.state().lost_replies += 1;
    }

    /// Every `spawn` call, including repeats and refused ones.
    pub fn requests(&self) -> Vec<SpawnRequest> {
        self.state().requests.clone()
    }

    /// Distinct agents created (one per idempotency key).
    pub fn created(&self) -> usize {
        self.state().by_key.len()
    }

    pub fn cancelled(&self) -> Vec<AetherAgentId> {
        self.state().cancelled.clone()
    }

    /// The next `update_agent` calls fail with these errors, in order.
    pub fn fail_next_update(&self, error: ArchonError) {
        self.state().update_failures.push_back(error);
    }

    /// Every update a live agent took, oldest first.
    pub fn updates(&self) -> Vec<(AetherAgentId, AgentUpdate)> {
        self.state().updates.clone()
    }

    /// Every agent `interrupt` was asked to stop, in order.
    pub fn interrupts(&self) -> Vec<AetherAgentId> {
        self.state().interrupts.clone()
    }

    /// `task_outcome` reports this for `task` from now on.
    pub fn settle_task(&self, task: &AetherTaskId, outcome: StageOutcome) {
        self.state().outcomes.insert(task.clone(), outcome);
    }

    /// The kept `agent` ended the turn it was resumed in with `outcome`;
    /// `resumed_outcome` reports it until the agent is resumed again.
    pub fn answer(&self, agent: &AetherAgentId, outcome: StageOutcome) {
        self.state().answers.insert(agent.clone(), outcome);
    }

    /// The next `create_run_node` calls fail with these errors, in order.
    pub fn fail_next_node(&self, error: ArchonError) {
        self.state().node_failures.push_back(error);
    }

    /// The next `create_run_node` creates its node but the reply is lost
    /// (`Unavailable`).
    pub fn lose_next_node_reply(&self) {
        self.state().lost_node_replies += 1;
    }

    /// Aether recorded `key` from a daemon that created stage agents as
    /// roots: the frame it holds names no parent and no network, so only a
    /// replay without them is the same request, and the agent sits in a
    /// run of its own. That daemon created no run node either.
    pub fn recorded_as_root(&self, key: &str) {
        let mut state = self.state();
        let recorded = state.recorded.get_mut(key).expect("a created key");
        recorded.parent = None;
        recorded.network = None;
        let root = AetherRunId::parse("run_root").expect("run id");
        state.by_key.get_mut(key).expect("a created key").run = root;
        state.node_by_key.clear();
        state.node_recorded.clear();
        state.node_requests.clear();
    }

    /// Every `create_run_node` call, including repeats.
    pub fn node_requests(&self) -> Vec<RunNodeRequest> {
        self.state().node_requests.clone()
    }

    /// Distinct run nodes created (one per idempotency key).
    pub fn nodes_created(&self) -> usize {
        self.state().node_by_key.len()
    }

    /// Run node ids in creation order.
    pub fn node_ids(&self) -> Vec<AetherAgentId> {
        let state = self.state();
        let mut ids: Vec<&AetherAgentId> = state.node_by_key.values().map(|n| &n.agent).collect();
        ids.sort();
        ids.into_iter().cloned().collect()
    }

    /// Every agent `is_live` was asked about, in order, since the last
    /// `take_live_checks`.
    pub fn take_live_checks(&self) -> Vec<AetherAgentId> {
        std::mem::take(&mut self.state().live_checks)
    }

    /// Aether holds `agent` as failed, so `wake` revives it once.
    pub fn fail_agent(&self, agent: &AetherAgentId) {
        let mut state = self.state();
        assert!(state.knows(agent), "only a created agent fails");
        state.failed.insert(agent.clone());
        state.in_turn.remove(agent);
    }

    /// Every agent `wake` revived, in order.
    pub fn woken(&self) -> Vec<AetherAgentId> {
        self.state().woken.clone()
    }

    /// Aether forgot `agent`: `is_live` reports it dead from now on.
    pub fn kill(&self, agent: &AetherAgentId) {
        let mut state = self.state();
        assert!(state.knows(agent), "kill knows only created agents");
        state.dead.insert(agent.clone());
        state.in_turn.remove(agent);
    }

    /// Run `hook` once, right after the next `update_agent` is recorded.
    pub fn on_next_update(&self, hook: impl FnOnce() + Send + 'static) {
        self.state().update_hook = Some(Box::new(hook));
    }

    /// `pulse` reports `pulse` for `agent`'s tree from now on.
    pub fn set_pulse(&self, agent: &AetherAgentId, pulse: TreePulse) {
        self.state().pulses.insert(agent.clone(), pulse);
    }

    /// `network_default` reports `network` from now on: Aether's
    /// `[network] default` changed, as after a restart with a new config.
    pub fn set_network_default(&self, network: NetworkPolicy) {
        self.state().network_denied = network == NetworkPolicy::Denied;
    }

    /// The stage agent keeps running after its task settles, as an engine
    /// agent does while it ends its turn: `is_live` reports it live until
    /// `end_turn`, `cancel` or `kill`.
    pub fn hold_turn(&self, agent: &AetherAgentId) {
        let mut state = self.state();
        assert!(state.is_stage_agent(agent), "only stage agents take turns");
        state.in_turn.insert(agent.clone());
    }

    /// The next `send_message` calls fail with these errors, in order.
    pub fn fail_next_message(&self, error: ArchonError) {
        self.state().message_failures.push_back(error);
    }

    /// Messages delivered, once per idempotency key, oldest first.
    pub fn messages(&self) -> Vec<(AetherAgentId, String)> {
        let state = self.state();
        state
            .delivered
            .iter()
            .map(|(_, to, content)| (to.clone(), content.clone()))
            .collect()
    }

    /// What the kept agent of stage run `stage` was resumed with.
    pub fn resumed_with(&self, stage: &StageRun) -> String {
        let state = self.state();
        let told = |(key, _, _): &&(String, AetherAgentId, String)| key == stage.id.as_str();
        let (_, to, content) = state.delivered.iter().find(told).expect("a resume message");
        assert_eq!(Some(to), stage.aether_agent.as_ref(), "sent to its agent");
        content.clone()
    }

    /// Every `send_message` call's idempotency key, including repeats.
    pub fn message_calls(&self) -> Vec<String> {
        self.state().message_calls.clone()
    }

    /// The held stage agent's turn ended: it is `completed` in Aether.
    pub fn end_turn(&self, agent: &AetherAgentId) {
        let held = self.state().in_turn.remove(agent);
        assert!(held, "end_turn ends a held turn: {agent}");
    }

    /// `parent` delegated to a new agent running `model`, live until
    /// `end_delegated`: like Aether, a cancel of its parent only asks it to end.
    pub fn delegate(&self, parent: &AetherAgentId, model: &str) -> AetherAgentId {
        let mut state = self.state();
        let known = state.knows(parent) || state.delegated.iter().any(|d| d.agent == *parent);
        assert!(known, "only a created agent delegates: {parent}");
        let n = state.delegated.len() + 1;
        let agent = AetherAgentId::parse(&format!("agt_child{n:04}")).expect("agent id");
        let task = AetherTaskId::parse(&format!("tsk_child{n:04}")).expect("task id");
        state.delegated.push(Descendant {
            agent: agent.clone(),
            parent: parent.clone(),
            model: model.parse().expect("model"),
            effort: Some(Effort::Medium),
            state: DescendantState::Live,
            task: Some(task),
            deadline: None,
            cost_budget_usd: None,
        });
        agent
    }

    /// The delegated `agent` completed its task with `structured` as its
    /// result, as a reviewer returns its verdict.
    pub fn complete_delegated(&self, agent: &AetherAgentId, structured: serde_json::Value) {
        self.complete_delegated_with(
            agent,
            StageOutcome::Completed {
                summary: "reviewed".into(),
                structured: Some(structured),
            },
        );
    }

    /// The delegated `agent` completed with Aether's recorded outcome.
    pub fn complete_delegated_with(&self, agent: &AetherAgentId, outcome: StageOutcome) {
        assert!(matches!(outcome, StageOutcome::Completed { .. }));
        let mut state = self.state();
        let delegated = state.delegated.iter_mut().find(|d| d.agent == *agent);
        let delegated = delegated.expect("a delegated agent");
        delegated.state = DescendantState::Completed;
        let task = delegated
            .task
            .clone()
            .expect("a delegated agent has a task");
        state.outcomes.insert(task, outcome);
    }

    /// Aether answers every read of the delegated `agent`'s result with `error`.
    pub fn unreadable_delegated(&self, agent: &AetherAgentId, error: &str) {
        let mut state = self.state();
        let delegated = state.delegated.iter().find(|d| d.agent == *agent);
        let task = delegated.and_then(|d| d.task.clone());
        let task = task.expect("a delegated agent has a task");
        state.unreadable.insert(task, error.to_owned());
    }

    /// The delegated agent ended as `ended`.
    pub fn end_delegated(&self, agent: &AetherAgentId, ended: DescendantState) {
        assert_ne!(ended, DescendantState::Live, "an ended agent is not live");
        let mut state = self.state();
        let delegated = state.delegated.iter_mut().find(|d| d.agent == *agent);
        delegated.expect("a delegated agent").state = ended;
    }

    /// The next `descendants` calls fail with these errors, in order.
    pub fn fail_next_descendants(&self, error: ArchonError) {
        self.state().descendant_failures.push_back(error);
    }

    /// Hold every spawn whose objective contains `text` until the returned
    /// gate gets a permit.
    pub fn stall_spawns(&self, text: &str) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.state().stalls.push((text.to_owned(), gate.clone()));
        gate
    }

    /// Every agent `descendants` was asked about, in order.
    pub fn descendant_checks(&self) -> Vec<AetherAgentId> {
        self.state().descendant_checks.clone()
    }
}

#[async_trait]
impl AgentBackend for FakeBackend {
    async fn host_context(
        &self,
        requested: archon_core::config::HostContext,
    ) -> Result<archon_core::config::HostContext, ArchonError> {
        Ok(if self.state().host_context_supported {
            requested
        } else {
            archon_core::config::HostContext::Full
        })
    }

    async fn spawn(&self, request: SpawnRequest) -> Result<SpawnedAgent, ArchonError> {
        let stalled = self.state().stalls.iter().find_map(|(text, gate)| {
            request
                .objective
                .contains(text.as_str())
                .then(|| gate.clone())
        });
        if let Some(gate) = stalled {
            let _open = gate.acquire().await.expect("a stall gate is never closed");
        }
        let mut state = self.state();
        state.requests.push(request.clone());
        if let Some(error) = state.failures.pop_front() {
            return Err(error);
        }
        if let Some(existing) = state.by_key.get(&request.idempotency_key) {
            if state.recorded[&request.idempotency_key] != request {
                return Err(conflict(&request.idempotency_key));
            }
            return Ok(existing.clone());
        }
        let network = request.network.unwrap_or(state.network_default());
        if let Some(parent) = &request.parent
            && state.node_network(parent) == Some(NetworkPolicy::Denied)
            && network == NetworkPolicy::Allowed
        {
            return Err(widens(parent));
        }
        state.next += 1;
        let n = state.next;
        let run = request
            .attach_run
            .clone()
            .or_else(|| state.node_run(request.parent.as_ref()?))
            .unwrap_or_else(|| AetherRunId::parse(&format!("run_{n:04}")).expect("run id"));
        let spawned = SpawnedAgent {
            agent: AetherAgentId::parse(&format!("agt_{n:04}")).expect("agent id"),
            task: AetherTaskId::parse(&format!("tsk_{n:04}")).expect("task id"),
            run,
        };
        state
            .by_key
            .insert(request.idempotency_key.clone(), spawned.clone());
        state
            .recorded
            .insert(request.idempotency_key.clone(), request);
        if state.lost_replies > 0 {
            state.lost_replies -= 1;
            return Err(ArchonError::Unavailable("reply lost".into()));
        }
        Ok(spawned)
    }

    async fn create_run_node(&self, request: RunNodeRequest) -> Result<CreatedNode, ArchonError> {
        let mut state = self.state();
        state.node_requests.push(request.clone());
        if let Some(error) = state.node_failures.pop_front() {
            return Err(error);
        }
        if let Some(existing) = state.node_by_key.get(&request.idempotency_key) {
            if state.node_recorded[&request.idempotency_key] != request {
                return Err(conflict(&request.idempotency_key));
            }
            return Ok(existing.clone());
        }
        state.next_node += 1;
        let n = state.next_node;
        let run = request
            .attach_run
            .clone()
            .unwrap_or_else(|| AetherRunId::parse(&format!("run_node{n:04}")).expect("run id"));
        let agent = CreatedNode {
            agent: AetherAgentId::parse(&format!("agt_node{n:04}")).expect("id"),
            run,
        };
        state
            .node_by_key
            .insert(request.idempotency_key.clone(), agent.clone());
        state
            .node_recorded
            .insert(request.idempotency_key.clone(), request);
        if state.lost_node_replies > 0 {
            state.lost_node_replies -= 1;
            return Err(ArchonError::Unavailable("node reply lost".into()));
        }
        Ok(agent)
    }

    async fn network_default(&self) -> Result<NetworkPolicy, ArchonError> {
        Ok(self.state().network_default())
    }

    async fn is_live(&self, agent: &AetherAgentId) -> Result<bool, ArchonError> {
        let mut state = self.state();
        state.live_checks.push(agent.clone());
        if state.dead.contains(agent) {
            return Ok(false);
        }
        let woken = state.woken.contains(agent);
        Ok(state.is_node(agent) || state.in_turn.contains(agent) || state.is_kept(agent) || woken)
    }

    async fn wake(&self, agent: &AetherAgentId) -> Result<(), ArchonError> {
        let mut state = self.state();
        if state.dead.contains(agent) || !state.failed.remove(agent) {
            return Err(ArchonError::Invalid(format!(
                "aether: agent {agent} did not fail; only a failed agent is woken"
            )));
        }
        state.woken.push(agent.clone());
        Ok(())
    }

    async fn cancel(&self, agent: &AetherAgentId, _reason: &str) -> Result<(), ArchonError> {
        let mut state = self.state();
        if let Some(error) = state.cancel_failures.pop_front() {
            return Err(error);
        }
        state.cancelled.push(agent.clone());
        if state.slow_cancels.contains(agent) {
            state.in_turn.insert(agent.clone());
            return Ok(());
        }
        state.dead.insert(agent.clone());
        state.in_turn.remove(agent);
        if state.is_node(agent) {
            let children: Vec<_> = state
                .by_key
                .iter()
                .filter_map(|(key, child)| {
                    (state.recorded[key].parent.as_ref() == Some(agent))
                        .then_some(child.agent.clone())
                })
                .collect();
            for child in children {
                if !state.slow_cancels.contains(&child) {
                    state.dead.insert(child.clone());
                    state.in_turn.remove(&child);
                }
            }
        }
        Ok(())
    }

    async fn update_agent(
        &self,
        agent: &AetherAgentId,
        update: AgentUpdate,
    ) -> Result<(), ArchonError> {
        assert!(!update.is_empty(), "an update changes something");
        let mut state = self.state();
        if let Some(error) = state.update_failures.pop_front() {
            return Err(error);
        }
        if !state.knows(agent) || state.dead.contains(agent) {
            return Err(ArchonError::NotFound(format!(
                "aether: no live agent {agent}"
            )));
        }
        state.updates.push((agent.clone(), update));
        let hook = state.update_hook.take();
        drop(state);
        if let Some(hook) = hook {
            hook();
        }
        Ok(())
    }

    async fn update_agent_keyed(
        &self,
        agent: &AetherAgentId,
        update: AgentUpdate,
        key: &str,
    ) -> Result<(), ArchonError> {
        assert!(!key.is_empty(), "a keyed update has a key");
        self.update_agent(agent, update).await
    }

    /// Ends a held turn, as Aether's interrupt does.
    async fn interrupt(&self, agent: &AetherAgentId) -> Result<bool, ArchonError> {
        let mut state = self.state();
        state.interrupts.push(agent.clone());
        if !state.knows(agent) {
            return Err(ArchonError::NotFound(format!("aether: no agent {agent}")));
        }
        Ok(state.in_turn.remove(agent))
    }

    async fn descendants(&self, agent: &AetherAgentId) -> Result<Vec<Descendant>, ArchonError> {
        let mut state = self.state();
        state.descendant_checks.push(agent.clone());
        if let Some(error) = state.descendant_failures.pop_front() {
            return Err(error);
        }
        let mut found: Vec<Descendant> = Vec::new();
        let mut parents = vec![agent.clone()];
        while let Some(parent) = parents.pop() {
            for child in state.delegated.iter().filter(|d| d.parent == parent) {
                parents.push(child.agent.clone());
                found.push(child.clone());
            }
        }
        Ok(found)
    }

    async fn agent_effort(&self, agent: &AetherAgentId) -> Result<Option<Effort>, ArchonError> {
        let state = self.state();
        let delegated = state.delegated.iter().find(|d| d.agent == *agent);
        Ok(delegated.and_then(|d| d.effort))
    }

    async fn pulse(&self, agent: &AetherAgentId) -> Result<Option<TreePulse>, ArchonError> {
        let state = self.state();
        if !state.knows(agent) {
            return Ok(None);
        }
        Ok(Some(state.pulses.get(agent).copied().unwrap_or_default()))
    }

    async fn task_outcome(&self, task: &AetherTaskId) -> Result<Option<StageOutcome>, ArchonError> {
        let mut state = self.state();
        if let Some(error) = state.outcome_failures.pop_front() {
            return Err(error);
        }
        if let Some(text) = state.unreadable.get(task) {
            return Err(ArchonError::Aether(text.clone()));
        }
        Ok(state.outcomes.get(task).cloned())
    }

    async fn resumed_outcome(
        &self,
        agent: &AetherAgentId,
        message: &str,
    ) -> Result<Option<StageOutcome>, ArchonError> {
        let mut state = self.state();
        if let Some(error) = state.outcome_failures.pop_front() {
            return Err(error);
        }
        let told = |(_, to, text): &(String, AetherAgentId, String)| to == agent && text == message;
        assert!(
            state.delivered.iter().any(told),
            "asked about a message never sent"
        );
        if state.dead.contains(agent) {
            return Ok(Some(StageOutcome::Failed {
                error:
                    "the agent kept for this stage could not answer: Aether holds it as cancelled"
                        .into(),
                infra: true,
            }));
        }
        Ok(state.answers.get(agent).cloned())
    }

    async fn send_message(
        &self,
        to: &AetherAgentId,
        content: &str,
        idempotency_key: &str,
    ) -> Result<(), ArchonError> {
        // A real call leaves the task, so deliveries to one stage interleave.
        tokio::task::yield_now().await;
        let mut state = self.state();
        state.message_calls.push(idempotency_key.to_owned());
        if let Some(error) = state.message_failures.pop_front() {
            return Err(error);
        }
        let delegated = state.delegated.iter().any(|d| d.agent == *to);
        if !state.knows(to) && !delegated {
            return Err(ArchonError::NotFound(format!("aether: no agent {to}")));
        }
        let seen = state
            .delivered
            .iter()
            .find(|(key, ..)| key == idempotency_key);
        match seen {
            Some((_, first_to, first)) if first_to == to && first == content => Ok(()),
            Some(_) => Err(ArchonError::Invalid(format!(
                "aether: idempotency_key `{idempotency_key}` already sent another message"
            ))),
            None => {
                let delivery = (idempotency_key.to_owned(), to.clone(), content.to_owned());
                state.delivered.push(delivery);
                // A new message starts a new turn: the last answer is history.
                state.answers.remove(to);
                Ok(())
            }
        }
    }
}

/// Move every settled stage of `run` `minutes` into the past, as if the
/// run had finished that long ago.
pub async fn age_settlements(h: &Harness, run: &PipelineRunId, minutes: i64) {
    age_settlements_except(h, run, minutes, &[]).await;
}

/// Age every settled stage of `run` except those of the stages named in
/// `keep`, as when they settled long before the run's last stage did.
pub async fn age_settlements_except(h: &Harness, run: &PipelineRunId, minutes: i64, keep: &[&str]) {
    let mut store = h.ctx.store.lock().await;
    let stages = store.stage_runs(run).expect("stage runs");
    assert!(!stages.is_empty(), "a run with stages");
    let mut aged = 0;
    for mut settled in stages {
        assert!(settled.state.is_terminal(), "only settled stages age");
        if keep.contains(&settled.stage_id.as_str()) {
            continue;
        }
        let finished = settled.finished_at.expect("settled stages have finished");
        settled.finished_at = Some(finished - chrono::Duration::minutes(minutes));
        store.upsert_stage_run(&settled).expect("aged stage run");
        aged += 1;
    }
    assert!(aged > 0, "some stage aged");
}

/// Move `stage`'s persisted deadline two hours into the past, as if its
/// agent had outlived it; returns the new deadline.
pub async fn expire_deadline(h: &Harness, stage: &StageRun) -> Timestamp {
    let past = archon_core::now() - chrono::Duration::hours(2);
    set_deadline(h, stage, past).await;
    past
}

/// Set `stage`'s persisted deadline to `at`.
pub async fn set_deadline(h: &Harness, stage: &StageRun, at: Timestamp) {
    write_deadline(&mut *h.ctx.store.lock().await, &stage.id, at);
}

/// Set the deadline in stage run `id`'s stored request to `at`, as a steer does.
pub fn write_deadline(store: &mut Store, id: &StageRunId, at: Timestamp) {
    let key = format!("spawn_request:{id}");
    let raw = store.kv_get(&key).expect("kv").expect("stored spawn");
    let mut spawn: serde_json::Value = serde_json::from_str(&raw).expect("json");
    spawn["request"]["deadline"] = serde_json::to_value(at).expect("timestamp");
    store.kv_set(&key, &spawn.to_string()).expect("kv set");
}

/// `stage`'s persisted deadline.
pub async fn stored_deadline(h: &Harness, stage: &StageRun) -> Option<Timestamp> {
    let key = format!("spawn_request:{}", stage.id);
    let raw = h
        .ctx
        .store
        .lock()
        .await
        .kv_get(&key)
        .expect("kv")
        .expect("stored spawn");
    let spawn: serde_json::Value = serde_json::from_str(&raw).expect("json");
    serde_json::from_value(spawn["request"]["deadline"].clone()).expect("a deadline or null")
}

/// The `--exact` filter for test `name` in the module at `module_path`
/// (pass `module_path!()`): libtest names omit the binary's crate segment.
pub fn exact_test_name(module_path: &str, name: &str) -> String {
    let (krate, module) = module_path
        .split_once("::")
        .expect("tests live in a module of the test binary");
    assert!(!krate.is_empty() && !module.is_empty(), "{module_path}");
    format!("{module}::{name}")
}
