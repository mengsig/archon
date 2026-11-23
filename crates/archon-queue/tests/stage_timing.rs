mod common;

use archon_core::model::Effort;
use archon_core::run::StageState;
use common::{item, stage, start, store, t};

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
