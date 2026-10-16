//! Fixtures shared by the store tests.
#![allow(dead_code)]

use std::path::PathBuf;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{Autonomy, AutonomyPatch};
use archon_core::catalog::StageKind;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState};
use archon_queue::Store;
use chrono::TimeZone;

/// A fixed instant `secs` seconds after an arbitrary epoch, with nanoseconds
/// so round trips prove full precision survives.
pub fn t(secs: i64) -> Timestamp {
    chrono::Utc
        .timestamp_opt(1_790_000_000 + secs, 123_456_789)
        .unwrap()
}

pub fn store() -> Store {
    Store::open_in_memory().unwrap()
}

pub fn repo(name: &str) -> Repo {
    Repo {
        id: RepoId::new(),
        name: name.into(),
        path: PathBuf::from(format!("/src/{name}")),
        ship: ShipMode::Local,
        base_branch: Some("main".into()),
        default_pipeline: Some("direct".into()),
        autonomy_max: AutonomyPatch::default(),
        grant_merge: false,
        attribution: Default::default(),
        gh_user: None,
        tiers: Default::default(),
        confirmed_fast_check: None,
        added_at: t(0),
    }
}

pub fn choice() -> ModelChoice {
    ModelChoice {
        model: "claude-code/claude-opus-5-5".parse().unwrap(),
        effort: Effort::High,
        tier: None,
    }
}

pub fn item(repo: Option<&Repo>, title: &str) -> QueueItem {
    let mut plan = ModelPlan::default();
    plan.0.insert("implement".into(), vec![choice()]);
    QueueItem {
        onto: None,
        id: QueueItemId::new(),
        repo: repo.map(|r| r.id.clone()),
        pipeline: "direct".into(),
        title: title.into(),
        brief: format!("do {title}"),
        priority: Priority::Normal,
        dependencies: vec![],
        tags: vec!["t1".into()],
        model_plan: plan,
        model_reasons: None,
        budget_usd: Some(2.5),
        autonomy: Autonomy::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        state: QueueState::Queued,
        created_at: t(1),
        updated_at: t(1),
    }
}

/// A store with one repository and `n` queued items in it.
pub fn seeded(n: usize) -> (Store, Repo, Vec<QueueItem>) {
    let mut store = store();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let items = (0..n)
        .map(|i| {
            store
                .insert_item(&item(Some(&repo), &format!("item {i}")), None)
                .unwrap()
        })
        .collect();
    (store, repo, items)
}

pub fn run_for(item: &QueueItem) -> PipelineRun {
    PipelineRun {
        id: PipelineRunId::new(),
        item: item.id.clone(),
        pipeline: item.pipeline.clone(),
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
        started_at: t(10),
        finished_at: None,
    }
}

/// Moves a queued item to `running` with a fresh live run; returns the run.
pub fn start(store: &mut Store, item: &QueueItemId) -> PipelineRun {
    let current = store
        .transition_item(item, QueueState::Dispatching, t(5))
        .unwrap();
    let run = run_for(&current);
    store.insert_run(&run).unwrap();
    store
        .transition_item(
            item,
            QueueState::Running {
                run: run.id.clone(),
            },
            t(6),
        )
        .unwrap();
    run
}

/// Finishes `run` and settles its item to the terminal `next`, as the
/// executor does when a run ends.
pub fn settle_run(
    store: &mut Store,
    run: &PipelineRun,
    next: QueueState,
    at: Timestamp,
) -> QueueItem {
    let state = match &next {
        QueueState::Succeeded { summary, .. } => RunState::Succeeded {
            summary: summary.clone(),
        },
        QueueState::Failed { error, .. } => RunState::Failed {
            stage_id: "implement".into(),
            error: error.clone(),
        },
        QueueState::Cancelled { reason } => RunState::Cancelled {
            reason: reason.clone(),
        },
        other => panic!("{} is not terminal", other.kind()),
    };
    let finished = PipelineRun {
        state,
        finished_at: Some(at),
        ..run.clone()
    };
    store.finish_run(&finished, next, at).unwrap().item
}

pub fn stage(run: &PipelineRun, fan_index: u32) -> StageRun {
    StageRun {
        id: StageRunId::new(),
        run: run.id.clone(),
        stage_id: "implement".into(),
        kind: Some(StageKind::Work),
        attempt: 1,
        fan_index,
        model_segments: Vec::new(),
        infrastructure_failure: false,
        host_context: Default::default(),
        choice: choice(),
        aether_agent: None,
        aether_task: None,
        state: StageState::Spawning,
        verdict: None,
        summary: None,
        structured: None,
        cost_usd: 0.0,
        started_at: t(20),
        finished_at: None,
    }
}

pub fn agent(n: u32) -> AetherAgentId {
    AetherAgentId::parse(&format!("agt_{n:04}")).unwrap()
}

/// A stage run that Aether acknowledged: `running` with agent `n`.
pub fn running_stage(run: &PipelineRun, fan_index: u32, n: u32) -> StageRun {
    StageRun {
        aether_agent: Some(agent(n)),
        aether_task: Some(format!("tsk_{n:04}").try_into().unwrap()),
        state: StageState::Running,
        ..stage(run, fan_index)
    }
}

/// xorshift64*: deterministic, dependency-free randomness for tests.
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        (self.next() % n as u64) as usize
    }
}
