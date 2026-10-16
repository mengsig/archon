//! A `DaemonContext` wired to the fake aetherd, plus store fixtures, for the
//! daemon-link tests.

#![allow(dead_code, reason = "each test binary uses a different subset")]

#[path = "../common/fake_ops.rs"]
pub mod fake_ops;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use archon_aether::fake::FakeAether;
use archon_aether::{AetherBackend, AetherClient, AetherLink};
use archon_core::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::ports::{NetworkPolicy, SpawnRequest};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState};
use archon_daemon::context::{DaemonContext, DaemonPaths};
use archon_daemon::disk::DiskWatch;
use archon_daemon::executor::RunTable;
use archon_daemon::feed::ReplyTracker;
use archon_daemon::handlers;
use archon_daemon::supervisor::{Backoff, Supervisor};
use archon_daemon::triggers::{self, TriggerEvent, TriggerQueue};
use archon_lavish::Lavish;
use archon_queue::Store;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify, broadcast};

pub const WAIT: Duration = Duration::from_secs(5);

pub struct Harness {
    pub dir: TempDir,
    pub fake: FakeAether,
    pub ctx: Arc<DaemonContext>,
    /// Held trigger events already returned by `drain_triggers`.
    drained: usize,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(Config::default(), None).await
    }

    /// A fresh in-memory store and a fake aetherd, not yet connected.
    pub async fn with(config: Config, lavish: Option<Lavish>) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let fake = FakeAether::start(&dir.path().join("aetherd.sock"))
            .await
            .expect("fake aetherd binds");
        fake.set_default("trigger", archon_aether::fake::Scripted::ok());
        let link = Arc::new(AetherLink::new());
        let backend = Arc::new(AetherBackend::new(link.clone(), config.models.policy()));
        let dispatch = Arc::new(Notify::new());
        let ctx = DaemonContext {
            storage: Default::default(),
            status_probes: Default::default(),
            quota: Default::default(),
            paths: paths(dir.path()),
            config,
            store: Mutex::new(Store::open_in_memory().expect("in-memory store")),
            aether: link,
            backend,
            feed: broadcast::channel(256).0,
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
            supervisor: Supervisor::default(),
            github: archon_daemon::github::GithubState::default(),
            triggers: TriggerQueue::default(),
            mirrors: archon_daemon::mirror::MirrorQueue::default(),
            spawn_recorded: Notify::new(),
            operator_settings: Default::default(),
            lavish,
            started_at: archon_core::now(),
            disk: DiskWatch::new(Arc::new(|_: &Path| Ok(u64::MAX)), Duration::ZERO),
            standby: OnceLock::new(),
            gc: Mutex::new(None),
            collecting: archon_daemon::gc::Collecting::new(dispatch),
            ops: Arc::new(fake_ops::FakeOps::default()).ops_state(),
        };
        Self {
            dir,
            fake,
            ctx: Arc::new(ctx),
            drained: 0,
        }
    }

    /// Connect to the fake and install the client, as the pump does.
    pub async fn connect(&self) -> Arc<AetherClient> {
        let client = AetherClient::connect(&self.fake.endpoint())
            .await
            .expect("connects to the fake");
        let client = Arc::new(client);
        self.ctx.aether.replace(Some(client.clone())).unwrap();
        client
    }

    pub async fn feed(&self) -> Vec<FeedEntry> {
        let store = self.ctx.store.lock().await;
        let items = store.feed_after(0, 10_000).unwrap();
        items.into_iter().map(|item| item.entry).collect()
    }

    pub async fn system_notes(&self) -> Vec<String> {
        let feed = self.feed().await;
        feed.into_iter()
            .filter_map(|entry| match entry {
                FeedEntry::System { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    pub async fn notifications(&self, kind: NotificationKind) -> Vec<String> {
        let feed = self.feed().await;
        feed.into_iter()
            .filter_map(|entry| match entry {
                FeedEntry::Notification(n) if n.kind == kind => Some(n.text),
                _ => None,
            })
            .collect()
    }

    pub async fn orchestrator_replies(&self) -> Vec<String> {
        let feed = self.feed().await;
        feed.into_iter()
            .filter_map(|entry| match entry {
                FeedEntry::Orchestrator { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    pub async fn kv(&self, key: &str) -> Option<String> {
        let store = self.ctx.store.lock().await;
        store.kv_get(key).unwrap().filter(|v| !v.is_empty())
    }

    pub async fn set_kv(&self, key: &str, value: &str) {
        self.ctx.store.lock().await.kv_set(key, value).unwrap();
    }

    /// `request` as `archon mcp` forwards it for `caller`, the agent Aether
    /// names as the one making the call.
    pub async fn mcp_as(&self, caller: &str, request: Request) -> Result<Reply, ArchonError> {
        let request = Request::ForAgent {
            caller: agent_id(caller),
            request: Box::new(request),
        };
        handlers::handle(&self.ctx, ClientKind::ArchonMcp, request).await
    }

    /// Move the orchestrator's last revival `secs` into the past, as if that
    /// much time had passed.
    pub async fn rewind_backoff(&self, secs: f64) {
        const BACKOFF_KEY: &str = "orchestrator_backoff";
        let raw = self.kv(BACKOFF_KEY).await.expect("a revival was recorded");
        let mut backoff: Backoff = serde_json::from_str(&raw).unwrap();
        let last = backoff.last.expect("a revival time");
        let millis = (secs * 1000.0) as i64;
        backoff.last = Some(last - chrono::TimeDelta::milliseconds(millis));
        let raw = serde_json::to_string(&backoff).unwrap();
        self.set_kv(BACKOFF_KEY, &raw).await;
    }

    /// News held for the orchestrator since the last drain, for a test that
    /// runs no delivery.
    pub async fn drain_triggers(&mut self) -> Vec<TriggerEvent> {
        let held = triggers::held(&self.ctx).await.unwrap();
        assert!(held.len() >= self.drained, "nothing delivers held news");
        let fresh = held[self.drained..].to_vec();
        self.drained = held.len();
        fresh
    }

    /// The next news event held for the orchestrator, waiting up to [`WAIT`].
    pub async fn next_trigger(&mut self) -> TriggerEvent {
        let drained = self.drained;
        self.eventually("a trigger is held", || async {
            triggers::held(&self.ctx).await.unwrap().len() > drained
        })
        .await;
        let event = triggers::held(&self.ctx).await.unwrap().remove(drained);
        self.drained += 1;
        event
    }

    /// Poll `check` until it holds or [`WAIT`] passes.
    pub async fn eventually<F, Fut>(&self, what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !check().await {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn paths(root: &Path) -> DaemonPaths {
    let data = root.join("data");
    DaemonPaths {
        socket: root.join("archond.sock"),
        store: data.join("archon.db"),
        worktrees: data.join("worktrees"),
        orchestrator_workspace: data.join("orchestrator"),
        scratch: data.join("scratch"),
        reports: data.join("reports"),
        logs: data.join("logs"),
        archon_bin: PathBuf::from("/usr/bin/archon"),
        data_dir: data,
    }
}

pub fn agent_id(raw: &str) -> AetherAgentId {
    AetherAgentId::parse(raw).expect("valid agent id")
}

/// A repository at `path` with one running item, its run and a running
/// stage for `agent`/`task`.
pub async fn running_stage(ctx: &DaemonContext, path: &Path, agent: &str, task: &str) -> StageRun {
    let mut store = ctx.store.lock().await;
    let run = running_run(&mut store, path);
    let mut stage = stage_run(&run.id, StageState::Running);
    stage.aether_agent = Some(agent_id(agent));
    stage.aether_task = Some(AetherTaskId::parse(task).expect("valid task id"));
    store.upsert_stage_run(&stage).unwrap();
    stage
}

/// Like `running_stage`, but still `spawning`: its pipeline run joined
/// Aether run `aether_run`, and its stored create is bound under `node`
/// (so it may be in Aether already), or unbound and never sent (`None`).
pub async fn spawning_stage(
    ctx: &DaemonContext,
    path: &Path,
    aether_run: &str,
    node: Option<&str>,
) -> StageRun {
    let mut store = ctx.store.lock().await;
    let mut run = running_run(&mut store, path);
    run.aether_run = Some(AetherRunId::parse(aether_run).expect("valid run id"));
    store.update_run(&run).unwrap();
    let stage = stage_run(&run.id, StageState::Spawning);
    let request = SpawnRequest {
        host_context: Default::default(),
        idempotency_key: stage.id.to_string(),
        role: "archon-stage:direct/implement".into(),
        name: "implement · link test item".into(),
        objective: "do it".into(),
        instructions: "contract".into(),
        choice: choice(),
        workspace: run.workspace.clone(),
        repository: Some("alpha".into()),
        repository_root: None,
        labels: Default::default(),
        hidden_tools: vec![],
        leaf: true,
        cost_budget_usd: None,
        aether_cost_budget: None,
        deadline: None,
        attach_run: None,
        parent: node.map(agent_id),
        network: node.map(|_| NetworkPolicy::Allowed),
        keep: false,
    };
    let binding = if node.is_some() { "bound" } else { "pending" };
    let spawn = serde_json::json!({"request": request, "timeout_minutes": 30, "parent": binding});
    let key = format!("spawn_request:{}", stage.id);
    store.kv_set(&key, &spawn.to_string()).unwrap();
    store.upsert_stage_run(&stage).unwrap();
    stage
}

/// A queued item of the repository at `path`, registered unless it is.
pub async fn queued_item(ctx: &DaemonContext, path: &Path) -> QueueItemId {
    let mut store = ctx.store.lock().await;
    let known = store.repos().unwrap().into_iter().find(|r| r.path == path);
    let repo = known.unwrap_or_else(|| {
        let repo = repo_at(path);
        store.insert_repo(&repo).unwrap();
        repo
    });
    store.insert_item(&queue_item(&repo), None).unwrap().id
}

/// A repository at `path` with one item the operator cancelled.
pub async fn cancelled_item(ctx: &DaemonContext, path: &Path) -> QueueItemId {
    let item = queued_item(ctx, path).await;
    let cancelled = QueueState::Cancelled { reason: None };
    let mut store = ctx.store.lock().await;
    store
        .transition_item(&item, cancelled, archon_core::now())
        .unwrap();
    item
}

fn repo_at(path: &Path) -> Repo {
    Repo {
        id: RepoId::new(),
        name: "alpha".into(),
        path: path.to_path_buf(),
        ship: ShipMode::Local,
        base_branch: Some("main".into()),
        default_pipeline: Some("direct".into()),
        autonomy_max: Default::default(),
        grant_merge: false,
        attribution: Default::default(),
        tiers: Default::default(),
        added_at: archon_core::now(),
    }
}

/// A repository at `path` with one running item and its run.
fn running_run(store: &mut Store, path: &Path) -> PipelineRun {
    let repo = repo_at(path);
    store.insert_repo(&repo).unwrap();
    let item = store.insert_item(&queue_item(&repo), None).unwrap();
    let now = archon_core::now();
    store
        .transition_item(&item.id, QueueState::Dispatching, now)
        .unwrap();
    let run = pipeline_run(&item.id);
    store.insert_run(&run).unwrap();
    let running = QueueState::Running {
        run: run.id.clone(),
    };
    store.transition_item(&item.id, running, now).unwrap();
    run
}

fn stage_run(run: &PipelineRunId, state: StageState) -> StageRun {
    StageRun {
        id: StageRunId::new(),
        run: run.clone(),
        stage_id: "implement".into(),
        kind: None,
        attempt: 1,
        fan_index: 0,
        model_segments: Vec::new(),
        infrastructure_failure: false,
        host_context: Default::default(),
        choice: choice(),
        aether_agent: None,
        aether_task: None,
        state,
        verdict: None,
        summary: None,
        structured: None,
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    }
}

pub fn choice() -> ModelChoice {
    ModelChoice {
        model: "claude-code/claude-opus-5-5".parse().unwrap(),
        effort: Effort::High,
        tier: None,
    }
}

fn queue_item(repo: &Repo) -> QueueItem {
    let mut plan = ModelPlan::default();
    plan.0.insert("implement".into(), vec![choice()]);
    QueueItem {
        id: QueueItemId::new(),
        repo: Some(repo.id.clone()),
        pipeline: "direct".into(),
        title: "link test item".into(),
        brief: "exercise the daemon link".into(),
        priority: Priority::Normal,
        dependencies: vec![],
        tags: vec![],
        model_plan: plan,
        model_reasons: None,
        budget_usd: None,
        autonomy: Default::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        state: QueueState::Queued,
        created_at: archon_core::now(),
        updated_at: archon_core::now(),
    }
}

fn pipeline_run(item: &QueueItemId) -> PipelineRun {
    PipelineRun {
        id: PipelineRunId::new(),
        item: item.clone(),
        pipeline: "direct".into(),
        shipped: false,
        aether_run: None,
        workspace: "/wt/alpha/x".into(),
        team_root: None,
        branch: "archon/x".into(),
        state: RunState::Running {
            stage_id: "implement".into(),
            attempt: 1,
        },
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    }
}
