//! Run edits are an ordered, durable log on a live run, and a kept agent
//! passes from a finished attempt to the next one of the same slot.

mod common;

use archon_core::error::ArchonError;
use archon_core::queue::QueueState;
use archon_core::run::{RunState, StageRun, StageState};
use archon_core::steer::{RunEdit, Then};
use archon_queue::Store;
use common::*;

fn abort(stage: &str, attempt: u32) -> RunEdit {
    RunEdit::AbortStage {
        stage: stage.into(),
        attempt,
        then: Then::Retry,
    }
}

fn finished(stage: &StageRun) -> StageRun {
    StageRun {
        state: StageState::Completed,
        summary: Some("done".into()),
        finished_at: Some(t(25)),
        ..stage.clone()
    }
}

#[test]
fn edits_are_numbered_in_order_and_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let item = store.insert_item(&item(Some(&repo), "one"), None).unwrap();
    let run = start(&mut store, &item.id);
    let insert = RunEdit::InsertStage {
        after: Some("implement".into()),
        spec: serde_json::json!({"stage": {"id": "audit"}, "prompt": "Audit it."}),
    };
    assert_eq!(store.append_run_edit(&run.id, &insert, t(21)).unwrap(), 1);
    let second = abort("implement", 1);
    assert_eq!(store.append_run_edit(&run.id, &second, t(22)).unwrap(), 2);
    drop(store);

    let store = Store::open(&path).unwrap();
    let edits = store.run_edits(&run.id).unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!((edits[0].seq, &edits[0].edit), (1, &insert));
    assert_eq!((edits[1].seq, &edits[1].edit), (2, &second));
    assert_eq!(edits[1].at, t(22));
}

#[test]
fn edits_belong_to_one_run_and_only_while_it_lives() {
    let (mut store, _repo, items) = seeded(2);
    let mut run = start(&mut store, &items[0].id);
    let other = start(&mut store, &items[1].id);
    store
        .append_run_edit(&run.id, &abort("implement", 1), t(21))
        .unwrap();
    assert!(store.run_edits(&other.id).unwrap().is_empty());
    let unknown = run_for(&items[0]);
    assert!(matches!(
        store.append_run_edit(&unknown.id, &abort("implement", 1), t(21)),
        Err(ArchonError::NotFound(_))
    ));

    run.state = RunState::Cancelled { reason: None };
    run.finished_at = Some(t(30));
    let cancelled = QueueState::Cancelled { reason: None };
    store.finish_run(&run, cancelled, t(30)).unwrap();
    let refused = store.append_run_edit(&run.id, &abort("implement", 1), t(31));
    assert!(matches!(refused, Err(ArchonError::InvalidTransition(_))));
    assert_eq!(store.run_edits(&run.id).unwrap().len(), 1, "history stays");
}

#[test]
fn a_kept_agent_passes_to_the_next_attempt_of_its_slot() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let first = running_stage(&run, 0, 1);
    store.upsert_stage_run(&first).unwrap();
    let second = StageRun {
        attempt: 2,
        aether_task: None,
        started_at: t(26),
        ..running_stage(&run, 0, 1)
    };
    let early = store.upsert_stage_run(&second).unwrap_err().to_string();
    assert!(early.contains("already belongs to stage run"), "{early}");

    store.upsert_stage_run(&finished(&first)).unwrap();
    store.upsert_stage_run(&second).unwrap();
    let holder = store.stage_run_by_agent(&agent(1)).unwrap().unwrap();
    assert_eq!(holder.id, second.id, "events reach the current attempt");
    let kept = store.stage_run(&first.id).unwrap();
    assert_eq!(kept.aether_agent, Some(agent(1)), "history names its agent");

    // Rewriting the finished attempt does not take the agent back.
    store.upsert_stage_run(&finished(&first)).unwrap();
    let holder = store.stage_run_by_agent(&agent(1)).unwrap().unwrap();
    assert_eq!(holder.id, second.id);
}

#[test]
fn an_agent_never_passes_to_another_slot_or_stage() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let first = running_stage(&run, 0, 1);
    store.upsert_stage_run(&first).unwrap();
    store.upsert_stage_run(&finished(&first)).unwrap();
    let other_slot = StageRun {
        attempt: 2,
        ..running_stage(&run, 1, 1)
    };
    let other_stage = StageRun {
        attempt: 2,
        stage_id: "review".into(),
        ..running_stage(&run, 0, 1)
    };
    for stage in [other_slot, other_stage] {
        let refused = store.upsert_stage_run(&stage);
        assert!(matches!(refused, Err(ArchonError::Invalid(_))), "{stage:?}");
    }
    let holder = store.stage_run_by_agent(&agent(1)).unwrap().unwrap();
    assert_eq!(holder.id, first.id, "a refusal moves nothing");
}
