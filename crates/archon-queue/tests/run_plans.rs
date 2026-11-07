mod common;
use archon_core::catalog::StageSummary;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::RunPlan;
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, RunState};
use common::*;

fn stage() -> StageSummary {
    let summary = serde_json::json!({
        "id": "implement", "kind": "work", "fan_out": 1, "gate": "all_pass",
        "timeout_minutes": 30, "nudge": null, "loops_to": null, "max_loops": 0
    });
    serde_json::from_value(summary).expect("a stage summary")
}

fn finished_plan(run: &PipelineRunId, revision: &str) -> RunPlan {
    RunPlan {
        run: run.clone(),
        pipeline: "default".into(),
        revision: revision.into(),
        stages: vec![stage()],
        edited: false,
        settings: Vec::new(),
        finished: true,
    }
}

fn plant(store: &mut archon_queue::Store, plan: &RunPlan) {
    let raw = serde_json::to_string(plan).unwrap();
    store
        .kv_set(&format!("run_plan:{}", plan.run), &raw)
        .unwrap();
}

fn cancelled(run: &PipelineRun) -> PipelineRun {
    PipelineRun {
        state: RunState::Cancelled { reason: None },
        finished_at: Some(t(30)),
        ..run.clone()
    }
}

#[test]
fn a_finish_keeps_its_plan_and_replaces_an_earlier_one() {
    let mut store = store();
    let queued = store.insert_item(&item(None, "planned"), None).unwrap();
    let run = start(&mut store, &queued.id);
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), None, "none yet");
    plant(&mut store, &finished_plan(&run.id, "stale"));

    let kept = finished_plan(&run.id, "final");
    let next = QueueState::Cancelled { reason: None };
    let done = store.finish_run_with_plan(&cancelled(&run), Some(&kept), next, t(30));
    assert_eq!(done.unwrap().item.state.kind(), "cancelled");
    assert_eq!(
        store.kept_run_plan(&run.id).unwrap(),
        Some(kept),
        "replaced"
    );

    store
        .kv_set(&format!("run_plan:{}", run.id), "{\"run\": 1}")
        .unwrap();
    let malformed = store.kept_run_plan(&run.id).unwrap_err();
    assert!(matches!(malformed, ArchonError::Store(_)), "{malformed:?}");
}

#[test]
fn a_finish_without_a_plan_leaves_no_earlier_one() {
    let mut store = store();
    let queued = store.insert_item(&item(None, "unplanned"), None).unwrap();
    let run = start(&mut store, &queued.id);
    plant(&mut store, &finished_plan(&run.id, "stale"));
    let next = QueueState::Cancelled { reason: None };
    store
        .finish_run_with_plan(&cancelled(&run), None, next, t(30))
        .unwrap();
    assert_eq!(
        store.kept_run_plan(&run.id).unwrap(),
        None,
        "no stale plan stands for this finish"
    );
}

/// A finish the store refuses keeps nothing, so its retry, with settings
/// changed meanwhile, keeps only its own plan.
#[test]
fn a_refused_finish_keeps_no_plan_and_its_retry_keeps_its_own() {
    let (mut store, _repo, items) = seeded(1);
    let dispatching = store
        .transition_item(&items[0].id, QueueState::Dispatching, t(2))
        .unwrap();
    let run = run_for(&dispatching);
    store.insert_run(&run).unwrap();
    let succeeded = PipelineRun {
        state: RunState::Succeeded {
            summary: "s".into(),
        },
        finished_at: Some(t(3)),
        ..run.clone()
    };
    let first = finished_plan(&run.id, "first");
    // dispatching -> succeeded is no edge: the item refuses after the plan is written.
    let refused = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "s".into(),
    };
    let error = store
        .finish_run_with_plan(&succeeded, Some(&first), refused, t(3))
        .unwrap_err();
    assert!(
        matches!(error, ArchonError::InvalidTransition(_)),
        "{error:?}"
    );
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), None, "rolled back");
    assert!(!store.run(&run.id).unwrap().state.is_terminal());

    let retried = finished_plan(&run.id, "retried with other settings");
    let next = QueueState::Cancelled { reason: None };
    store
        .finish_run_with_plan(&cancelled(&run), Some(&retried), next, t(30))
        .unwrap();
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), Some(retried));
}

#[test]
fn a_finish_of_no_run_keeps_nothing() {
    let mut store = store();
    let queued = store.insert_item(&item(None, "nowhere"), None).unwrap();
    let run = run_for(&queued);
    let plan = finished_plan(&run.id, "orphan");
    let next = QueueState::Cancelled { reason: None };
    let refused = store.finish_run_with_plan(&cancelled(&run), Some(&plan), next, t(30));
    assert!(refused.is_err(), "no such run");
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), None);
}
