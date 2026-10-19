//! Finished-run fixtures for `archon learning`: runs whose stage runs and
//! outcome name their model choices, and the requests that correct them.

#![allow(dead_code, reason = "each test binary uses a different subset")]

use archon_core::Timestamp;
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::feedback::{ChoiceStanding, CorrectionRequest, FeedbackReceipt, LearningWhy};
use archon_core::id::{QueueItemId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::outcome::{CorrectionCause, Outcome, OutcomeResult, OutcomeStage, RouteContext};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState};

use crate::common::{self, Harness, PIPELINE, plan};

pub fn model(name: &str) -> ModelChoice {
    common::choice(&format!("codex/{name}"), Effort::Low)
}

/// One finished run of `implement` alone, which `choice` completed now.
pub async fn sample(h: &Harness, choice: &ModelChoice, cost: f64) -> PipelineRun {
    sample_with(h, &[("implement", choice)], cost, archon_core::now(), None).await
}

/// One finished run of `implement` alone that ran as `kind`; `None` is a run
/// recorded before Archon kept the kind each stage ran as.
pub async fn sample_ran_as(
    h: &Harness,
    choice: &ModelChoice,
    cost: f64,
    kind: Option<StageKind>,
) -> PipelineRun {
    let only = [("implement", choice)];
    recorded_sample(h, &only, cost, archon_core::now(), None, |_| kind).await
}

/// One finished run, finished `at`, whose stage runs ran `attempts` in order;
/// `task` puts its outcome in another task-class cohort.
pub async fn sample_with(
    h: &Harness,
    attempts: &[(&str, &ModelChoice)],
    cost: f64,
    at: Timestamp,
    task: Option<&str>,
) -> PipelineRun {
    recorded_sample(h, attempts, cost, at, task, |stage| {
        Some(kind_in_pipeline(stage))
    })
    .await
}

/// The kind of `stage` in the harness pipeline.
pub fn kind_in_pipeline(stage: &str) -> StageKind {
    match stage {
        "implement" => StageKind::Work,
        "review" => StageKind::Review,
        "ship" => StageKind::Ship,
        other => panic!("the harness pipeline has no stage {other:?}"),
    }
}

/// [`sample_with`], each stage run recording the kind `ran_as` says.
async fn recorded_sample(
    h: &Harness,
    attempts: &[(&str, &ModelChoice)],
    cost: f64,
    at: Timestamp,
    task: Option<&str>,
    ran_as: impl Fn(&str) -> Option<StageKind>,
) -> PipelineRun {
    assert!(!attempts.is_empty(), "a run ran a stage");
    let item = h.enqueue(&format!("sample {}", attempts[0].1.model)).await;
    let mut store = h.ctx.store.lock().await;
    let mut run = start(&mut store, &item, h.repo.clone(), at);
    for (n, (stage, choice)) in attempts.iter().enumerate() {
        let attempt = attempts[..n].iter().filter(|(s, _)| s == stage).count() as u32 + 1;
        let stage_run = StageRun {
            id: StageRunId::new(),
            run: run.id.clone(),
            stage_id: (*stage).into(),
            kind: ran_as(stage),
            attempt,
            fan_index: 0,
            choice: (*choice).clone(),
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            aether_agent: None,
            aether_task: None,
            state: StageState::Completed,
            verdict: None,
            summary: Some("done".into()),
            structured: None,
            cost_usd: cost,
            started_at: at,
            finished_at: Some(at),
        };
        store.upsert_stage_run(&stage_run).unwrap();
    }
    let done = RunState::Succeeded {
        summary: "done".into(),
    };
    run.finish(done, at).unwrap();
    let settled = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "done".into(),
    };
    store.finish_run(&run, settled, at).unwrap();
    let mut outcome = outcome(&run, &item, attempts, cost);
    if let Some(task) = task {
        let raw = store
            .kv_get(&format!("learning/context/{}", item.id))
            .unwrap();
        let mut context: RouteContext = serde_json::from_str(&raw.expect("enqueued")).unwrap();
        context.task_class = task.into();
        outcome.context = Some(context);
    }
    store.record_outcome(&outcome).unwrap();
    run
}

fn start(
    store: &mut archon_queue::Store,
    item: &QueueItem,
    workspace: std::path::PathBuf,
    at: Timestamp,
) -> PipelineRun {
    let now = archon_core::now();
    store
        .transition_item(&item.id, QueueState::Dispatching, now)
        .unwrap();
    let run = PipelineRun {
        id: archon_core::id::PipelineRunId::new(),
        item: item.id.clone(),
        pipeline: PIPELINE.into(),
        shipped: false,
        aether_run: None,
        workspace,
        team_root: None,
        branch: "archon/sample".into(),
        state: RunState::Running {
            stage_id: "implement".into(),
            attempt: 1,
        },
        cost_usd: 0.0,
        started_at: at,
        finished_at: None,
    };
    store.insert_run(&run).unwrap();
    let running = QueueState::Running {
        run: run.id.clone(),
    };
    store.transition_item(&item.id, running, now).unwrap();
    run
}

/// Each stage's last choice passed: the run is one routing sample per stage.
fn outcome(
    run: &PipelineRun,
    item: &QueueItem,
    attempts: &[(&str, &ModelChoice)],
    cost: f64,
) -> Outcome {
    let mut stages: Vec<OutcomeStage> = Vec::new();
    for (stage, choice) in attempts {
        stages.retain(|s| s.stage != *stage);
        stages.push(OutcomeStage {
            stage: (*stage).into(),
            choice: (*choice).clone(),
            cost_usd: cost,
            wall_secs: 30,
            attempts: 1,
            passed: Some(true),
        });
    }
    Outcome {
        run: run.id.clone(),
        item: item.id.clone(),
        repo: item.repo.clone(),
        pipeline: PIPELINE.into(),
        diff: None,
        stages,
        review_rounds: 0,
        result: OutcomeResult::Succeeded,
        failure: None,
        corrections: Vec::new(),
        feedback: Vec::new(),
        context: None,
        merged: None,
        cost_usd: cost,
        cost_provisional: true,
        count_estimates: true,
        wall_secs: 30,
        finished_at: run.finished_at.expect("finished"),
    }
}

/// Five successful runs each of a cheap and a dear choice, finished `at`;
/// the cheap runs.
pub async fn two_choices_at(h: &Harness, at: Timestamp) -> Vec<PipelineRun> {
    let (cheap_choice, dear_choice) = (model("cheap"), model("dear"));
    let mut cheap = Vec::new();
    for _ in 0..5 {
        let only = [("implement", &cheap_choice)];
        cheap.push(sample_with(h, &only, 0.2, at, None).await);
        sample_with(h, &[("implement", &dear_choice)], 2.0, at, None).await;
    }
    cheap
}

pub async fn two_choices(h: &Harness) -> Vec<PipelineRun> {
    two_choices_at(h, archon_core::now()).await
}

/// The test plan without `implement`, which a learned default then fills.
pub fn without_implement() -> ModelPlan {
    let mut partial = plan();
    partial.0.remove("implement");
    partial
}

/// A new item's `implement` model and the routing reason kept for it.
pub async fn default_route(h: &Harness) -> (QueueItem, String, Option<String>) {
    let item = h
        .enqueue_with("learned default", without_implement())
        .await
        .unwrap();
    let model = item.model_plan.0["implement"][0].model.to_string();
    let reasons = h.kv(&format!("learning/routing/{}", item.id)).await;
    let reason = reasons.and_then(|raw| {
        let mut map: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&raw).unwrap();
        map.remove("implement")
    });
    (item, model, reason)
}

pub async fn default_model(h: &Harness) -> String {
    default_route(h).await.1
}

pub fn request(run: &PipelineRun, stage: &str, cause: CorrectionCause) -> CorrectionRequest {
    CorrectionRequest {
        item: run.item.clone(),
        run: run.id.clone(),
        stage: stage.into(),
        cause,
        reason: "Ignored the current Git identity rule".into(),
        forget_memory: None,
    }
}

pub async fn correct(
    h: &Harness,
    request: CorrectionRequest,
) -> Result<FeedbackReceipt, ArchonError> {
    receipt(h, Request::LearningCorrect(request)).await
}

/// The receipt of `correct`, `undo`, `restore-memory` or `retry-memory`.
pub async fn receipt(h: &Harness, request: Request) -> Result<FeedbackReceipt, ArchonError> {
    match h.cli(request).await? {
        Reply::Feedback(receipt) => Ok(*receipt),
        other => panic!("a learning mutation replied {other:?}"),
    }
}

pub async fn learning_why(h: &Harness, item: &QueueItemId) -> LearningWhy {
    let request = Request::LearningWhy { item: item.clone() };
    match h.cli(request).await.unwrap() {
        Reply::LearningWhy(why) => *why,
        other => panic!("learning why replied {other:?}"),
    }
}

/// A standing as (model name, runs, passed).
pub fn standing(standing: Option<&ChoiceStanding>) -> Option<(String, u32, u32)> {
    standing.map(|s| (s.choice.model.model.clone(), s.runs, s.passed))
}
