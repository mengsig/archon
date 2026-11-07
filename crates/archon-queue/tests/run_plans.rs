mod common;
use archon_core::catalog::StageSummary;
use archon_core::id::PipelineRunId;
use archon_core::protocol::RunPlan;
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

#[test]
fn a_finish_replaces_or_clears_the_plan_an_earlier_finish_kept() {
    let mut store = store();
    let queued = store.insert_item(&item(None, "planned"), None).unwrap();
    let run = start(&mut store, &queued.id);
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), None, "none yet");

    let earlier = finished_plan(&run.id, "a");
    store.keep_run_plan(&run.id, Some(&earlier)).unwrap();
    assert_eq!(store.kept_run_plan(&run.id).unwrap(), Some(earlier));
    let later = finished_plan(&run.id, "b");
    store.keep_run_plan(&run.id, Some(&later)).unwrap();
    assert_eq!(
        store.kept_run_plan(&run.id).unwrap(),
        Some(later),
        "replaced"
    );
    store.keep_run_plan(&run.id, None).unwrap();
    assert_eq!(
        store.kept_run_plan(&run.id).unwrap(),
        None,
        "a finish with no plan leaves no stale one"
    );

    let nowhere = PipelineRunId::new();
    assert!(store.keep_run_plan(&nowhere, None).is_err(), "no such run");
    store
        .kv_set(&format!("run_plan:{}", run.id), "{\"run\": 1}")
        .unwrap();
    let malformed = store.kept_run_plan(&run.id).unwrap_err();
    assert!(
        matches!(malformed, archon_core::error::ArchonError::Store(_)),
        "{malformed:?}"
    );
}
