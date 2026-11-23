mod common;

use archon_core::model::Effort;
use archon_core::queue::QueueState;
use archon_core::run::{StageRun, StageState};
use archon_queue::store::keys::{STAGE_DEFERRED_UNTIL_PREFIX, STAGE_SPAWN_PREFIX};
use common::{item, settle_run, stage, start, store, t};

#[test]
fn p90_requires_ten_completed_samples_in_the_same_cohort() {
    let mut store = store();
    let target = item(None, "target");
    store.insert_item(&target, None).unwrap();
    let run = start(&mut store, &target.id);
    let mut latest = stage(&run, 0);
    latest.state = StageState::Completed;
    let effort = latest.choice.effort;
    let other_effort = if effort == Effort::High {
        Effort::Medium
    } else {
        Effort::High
    };

    for n in 1..=9 {
        let sample = stage(&run, n);
        let observed = archon_core::run::StageRun {
            state: StageState::Completed,
            finished_at: Some(t(20 + i64::from(n) * 60)),
            ..sample
        };
        store.upsert_stage_run(&observed).unwrap();
    }
    let mut other = stage(&run, 10);
    other.state = StageState::Completed;
    other.finished_at = Some(t(80));
    other.choice.effort = other_effort;
    store.upsert_stage_run(&other).unwrap();
    other = stage(&run, 11);
    other.state = StageState::TimedOut;
    other.finished_at = Some(t(80));
    store.upsert_stage_run(&other).unwrap();

    let p90 = |store: &archon_queue::Store, pipeline: &str, stage: &str, effort| {
        store.stage_p90_seconds(pipeline, stage, effort).unwrap()
    };
    assert_eq!(p90(&store, &run.pipeline, "implement", effort), None);
    latest.finished_at = Some(t(20 + 3_600));
    store.upsert_stage_run(&latest).unwrap();
    assert_eq!(p90(&store, &run.pipeline, "implement", effort), Some(540));
    assert_eq!(p90(&store, "other", "implement", effort), None);
    assert_eq!(p90(&store, &run.pipeline, "review", effort), None);
    assert_eq!(p90(&store, &run.pipeline, "implement", other_effort), None);
}

#[test]
fn p90_excludes_retry_holds_even_after_requests_are_pruned() {
    let mut store = store();
    let target = store
        .insert_item(&item(None, "delayed retries"), None)
        .unwrap();
    let run = start(&mut store, &target.id);
    let held_until = t(320);
    let finish = t(380);
    let effort = stage(&run, 0).choice.effort;
    for index in 0..10 {
        let delayed = StageRun {
            state: StageState::Completed,
            finished_at: Some(finish),
            ..stage(&run, index)
        };
        assert_eq!(delayed.started_at, t(20));
        let (key, value) = if index < 5 {
            (
                format!("{STAGE_DEFERRED_UNTIL_PREFIX}{}", delayed.id),
                held_until.to_rfc3339(),
            )
        } else {
            (
                format!("{STAGE_SPAWN_PREFIX}{}", delayed.id),
                serde_json::json!({"not_before": held_until}).to_string(),
            )
        };
        store
            .upsert_stage_run_with_kv(&delayed, &[(&key, &value)])
            .unwrap();
    }
    assert_eq!(
        store
            .stage_p90_seconds(&run.pipeline, "implement", effort)
            .unwrap(),
        Some(60)
    );
}

#[test]
fn prune_history_removes_the_delay_metadata_of_pruned_stages() {
    let mut store = store();
    let target = store
        .insert_item(&item(None, "settled item"), None)
        .unwrap();
    let run = start(&mut store, &target.id);
    let done = StageRun {
        state: StageState::Completed,
        finished_at: Some(t(380)),
        ..stage(&run, 0)
    };
    let key = format!("{STAGE_DEFERRED_UNTIL_PREFIX}{}", done.id);
    let value = t(320).to_rfc3339();
    store
        .upsert_stage_run_with_kv(&done, &[(&key, &value)])
        .unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(381),
    );
    let kept = store.prune_history(t(300), 100, |_, _| false).unwrap();
    assert_eq!(kept.stage_runs, 0);
    assert_eq!(store.kv_get(&key).unwrap(), Some(value));
    let pruned = store.prune_history(t(400), 100, |_, _| false).unwrap();
    assert_eq!(pruned.stage_runs, 1);
    assert_eq!(store.kv_get(&key).unwrap(), None);
}
