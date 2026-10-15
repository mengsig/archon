//! A team pipeline fixture: `plan`, a team of two roles, `review`, `ship`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::QueueItem;
use archon_core::run::{PipelineRun, StageRun};
use archon_pipeline::team::worker_branch;

use super::{Harness, MODEL, REPO_NAME, choice, completed};

pub const TEAM_PIPELINE: &str = "team-build";

/// `plan` → `implement` (a team of api and ui, looping to `plan` once on a
/// conflict) → `review` → `ship`.
pub const TEAM_TOML: &str = r#"[pipeline]
name = "team-build"
description = "team test pipeline"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "plan"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "implement"
kind = "team"
prompt = "implement.md"
roles = ["api", "ui"]
timeout_minutes = 30
on_fail = { goto = "plan", max_loops = 1 }

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
gate = "all_pass"
timeout_minutes = 30

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

pub const BUDGET_USD: f64 = 6.0;

pub fn team_plan() -> ModelPlan {
    let mut stages = BTreeMap::new();
    for stage in ["plan", "review", "ship"] {
        stages.insert(stage.to_owned(), vec![choice(MODEL, Effort::Low)]);
    }
    stages.insert("implement".into(), vec![choice(MODEL, Effort::Medium); 2]);
    ModelPlan(stages)
}

pub async fn enqueue_team(h: &Harness, title: &str) -> QueueItem {
    h.add_pipeline(TEAM_PIPELINE, TEAM_TOML);
    match h.cli(team_request(title)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// The enqueue of a team item titled `title` in the harness repository.
pub fn team_request(title: &str) -> Request {
    Request::Enqueue(EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(TEAM_PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: team_plan().into(),
        model_reasons: None,
        budget_usd: Some(BUDGET_USD),
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    })
}

/// Start the item: its `plan` stage is running.
pub async fn start_plan(h: &Harness, title: &str) -> (QueueItem, PipelineRun) {
    let item = enqueue_team(h, title).await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    (item, run)
}

/// Start the item and settle its `plan` stage: the two roles are running.
pub async fn start_team(h: &Harness, title: &str) -> (QueueItem, PipelineRun) {
    let (item, run) = start_plan(h, title).await;
    let root = h.ctx.paths.worktrees.join(REPO_NAME).join(item.id.as_str());
    assert_eq!(run.workspace, root.join("main"));
    assert_eq!(run.team_root.as_deref(), Some(root.as_path()));
    let nodes = h.backend.node_requests();
    assert_eq!(nodes[0].workspace, root, "the node is scoped to the item");
    assert_eq!(h.backend.requests()[0].workspace, run.workspace);
    h.settle_running(|_| completed("split into api and ui"))
        .await;
    h.dispatch().await;
    (item, run)
}

pub fn worker(run: &PipelineRun, role: &str) -> PathBuf {
    let root = run.team_root.as_ref().expect("a team run");
    root.join(format!("implement-{role}"))
}

pub fn branch(run: &PipelineRun, role: &str, attempt: u32) -> String {
    worker_branch(&run.branch, &run.id, role, attempt)
}

pub fn role_of(stage: &StageRun) -> &'static str {
    ["api", "ui"][stage.fan_index as usize]
}
