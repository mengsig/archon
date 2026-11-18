//! A failed run's class comes from its stage records, never its error's words.
mod common;
use archon_core::catalog::StageKind;
use archon_core::outcome::{ClassSource, FailureClass, Outcome};
use archon_core::queue::QueueState;
use archon_core::run::{BUDGET_STAGE, PipelineRun, RunState, StageRun, StageState, Verdict};
use archon_queue::Store;
use common::*;

/// The words 1.6.3 read a class from, in one error.
const MISLEADING: &str = "stage `review` failed: aether reported a timeout near the budget";

/// Fails a fresh run under `stage_id` after `stages` settled, and captures it.
fn failed_with(
    stage_id: &str,
    error: &str,
    stages: impl Fn(&PipelineRun) -> Vec<StageRun>,
) -> Outcome {
    let mut store = store();
    let item = store.insert_item(&item(None, "classed"), None).unwrap();
    let run = start(&mut store, &item.id);
    for stage in stages(&run) {
        store.upsert_stage_run(&stage).unwrap();
    }
    fail(&mut store, &run, stage_id, error);
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    store.outcome(&run.id).unwrap().unwrap()
}

fn fail(store: &mut Store, run: &PipelineRun, stage_id: &str, error: &str) {
    let state = RunState::Failed {
        stage_id: stage_id.into(),
        error: error.into(),
    };
    let finished = PipelineRun {
        state,
        finished_at: Some(t(50)),
        ..run.clone()
    };
    let next = QueueState::Failed {
        run: Some(run.id.clone()),
        error: error.into(),
    };
    store.finish_run(&finished, next, t(50)).unwrap();
}

fn settled(run: &PipelineRun, stage_id: &str, kind: StageKind, state: StageState) -> StageRun {
    let mut stage = stage(run, 0);
    stage.stage_id = stage_id.into();
    stage.kind = Some(kind);
    stage.state = state;
    stage.finished_at = Some(t(30));
    stage
}

fn review_fail(run: &PipelineRun) -> StageRun {
    let mut review = settled(run, "review", StageKind::Review, StageState::Completed);
    review.verdict = Some(Verdict::Fail);
    review.summary = Some(MISLEADING.into());
    review
}

#[test]
fn a_review_fail_whose_error_names_aether_timeout_and_budget_is_a_review_failure() {
    let outcome = failed_with("review", MISLEADING, |run| vec![review_fail(run)]);
    assert_eq!(outcome.failure, Some(FailureClass::Review));
    assert_eq!(outcome.failure_source, Some(ClassSource::Structure));
    assert!(outcome.is_scored(), "a review failure judged the work");
}

#[test]
fn deadlines_backends_and_cost_caps_are_read_from_the_stage_that_ended_the_run() {
    let timed_out = failed_with("review", "review failed", |run| {
        vec![settled(
            run,
            "review",
            StageKind::Review,
            StageState::TimedOut,
        )]
    });
    assert_eq!(timed_out.failure, Some(FailureClass::Timeout));
    let infra = failed_with("implement", "the work went wrong", |run| {
        let mut stage = settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed {
                error: "gone".into(),
            },
        );
        stage.infrastructure_failure = true;
        vec![stage]
    });
    assert_eq!(infra.failure, Some(FailureClass::Infrastructure));
    let capped = failed_with("implement", "stage failed", |run| {
        let error = "agent stopped: cost budget of $2.00 reached".into();
        vec![settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed { error },
        )]
    });
    assert_eq!(capped.failure, Some(FailureClass::Budget));
    let guard = failed_with(
        BUDGET_STAGE,
        "BLOCKED: the item's budget is used up: $1.00 spent of its $1.00 cap ($0.00 over)",
        |_| Vec::new(),
    );
    assert_eq!(guard.failure, Some(FailureClass::Budget), "the usage guard");
    let spent = failed_with(
        "implement",
        "BLOCKED: the item's budget is used up: $1.00 spent of its $1.00 cap ($0.00 over)",
        |_| Vec::new(),
    );
    assert_eq!(
        spent.failure,
        Some(FailureClass::Budget),
        "spent before dispatch"
    );
    let unpriced = failed_with(
        BUDGET_STAGE,
        "BLOCKED: cannot enforce the item's dollar budget: Aether reported usage",
        |_| Vec::new(),
    );
    assert_eq!(unpriced.failure, Some(FailureClass::Budget));
    let named = failed_with(BUDGET_STAGE, "BLOCKED: needs a token", |_| Vec::new());
    assert_eq!(
        named.failure,
        Some(FailureClass::Blocked),
        "a stage named budget is no budget stop"
    );
    let failed_agent = failed_with("implement", "stage failed", |run| {
        let error = "agent stopped: cost budget of $2.00 reached".into();
        let mut stage = settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed { error },
        );
        stage.infrastructure_failure = true;
        vec![stage]
    });
    assert_eq!(
        failed_agent.failure,
        Some(FailureClass::Budget),
        "Aether's cost stop also marks the agent failed"
    );
    let disk = failed_with(
        "implement",
        "create_agent failed on a full disk 5 times (x)",
        |run| {
            let error = "create_agent failed on a full disk 5 times (x)".into();
            vec![settled(
                run,
                "implement",
                StageKind::Work,
                StageState::Failed { error },
            )]
        },
    );
    assert_eq!(
        disk.failure,
        Some(FailureClass::Infrastructure),
        "a 1.6.3 disk stop"
    );
    for class in [
        timed_out,
        infra,
        capped,
        guard,
        spent,
        unpriced,
        failed_agent,
        disk,
    ]
    .map(|o| o.failure.unwrap())
    {
        assert!(archon_core::outcome::UNSCORED_FAILURES.contains(&class));
    }
}

#[test]
fn words_alone_never_make_a_class_and_a_retried_slot_ends_with_its_last_try() {
    let worded = failed_with("implement", MISLEADING, |run| {
        let error = "Aether timed out; budget left".into();
        vec![settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed { error },
        )]
    });
    assert_eq!(worded.failure, Some(FailureClass::Other));
    let checks = failed_with("test", MISLEADING, |run| {
        vec![settled(run, "test", StageKind::Test, StageState::Completed)]
    });
    assert_eq!(checks.failure, Some(FailureClass::Verification));
    // An infrastructure retry replaced the slot; the review then failed.
    let retried = failed_with("review", MISLEADING, |run| {
        let mut first = settled(run, "review", StageKind::Review, StageState::TimedOut);
        first.started_at = t(21);
        let mut last = review_fail(run);
        last.started_at = t(22);
        vec![first, last]
    });
    assert_eq!(retried.failure, Some(FailureClass::Review));
    let passed = failed_with("review", "Archon could not stop the agents", |run| {
        let mut review = settled(run, "review", StageKind::Review, StageState::Completed);
        review.verdict = Some(Verdict::Pass);
        vec![review]
    });
    assert_eq!(
        passed.failure,
        Some(FailureClass::Other),
        "a passed review failed nothing"
    );
}

#[test]
fn a_nested_review_is_read_as_the_gate_reads_it() {
    let claimed = |review: serde_json::Value| {
        move |run: &PipelineRun| {
            let mut work = settled(run, "implement", StageKind::Work, StageState::Completed);
            work.verdict = Some(Verdict::Pass);
            work.structured = Some(serde_json::json!({ "review": review.clone() }));
            vec![work]
        }
    };
    let failed = failed_with(
        "implement",
        "gate",
        claimed(serde_json::json!({"verdict": "FAIL"})),
    );
    assert_eq!(
        failed.failure,
        Some(FailureClass::Review),
        "the nested verdict failed"
    );
    let refused = serde_json::json!({"verdict": "pass", "unverified": "no reviewer ran"});
    let refused = failed_with("implement", "gate", claimed(refused));
    assert_eq!(
        refused.failure,
        Some(FailureClass::Review),
        "Archon refused the pass"
    );
    let passed = failed_with(
        "implement",
        "later",
        claimed(serde_json::json!({"verdict": "pass"})),
    );
    assert_eq!(
        passed.failure,
        Some(FailureClass::Other),
        "a passed review failed nothing"
    );
    let skip = serde_json::json!({"verdict": "skipped", "reason": "docs only"});
    let refused_skip = failed_with(
        "implement",
        "stage `implement` failed: the review this pipeline declares is not on record: \
         a skip needs reviewers here\n\nreport",
        claimed(skip.clone()),
    );
    assert_eq!(
        refused_skip.failure,
        Some(FailureClass::Review),
        "the gate refused the skip"
    );
    let accepted_skip = failed_with("implement", "stage `implement` failed: push", claimed(skip));
    assert_eq!(
        accepted_skip.failure,
        Some(FailureClass::Other),
        "the gate took the skip"
    );
    let legacy = failed_with("code-review", "later", |run| {
        let mut review = settled(run, "code-review", StageKind::Review, StageState::Completed);
        review.kind = None;
        review.verdict = Some(Verdict::Pass);
        vec![review]
    });
    assert_eq!(
        legacy.failure,
        Some(FailureClass::Other),
        "a kindless review that passed"
    );
    let legacy_fail = failed_with("code-review", "x", |run| {
        let mut review = review_fail(run);
        review.stage_id = "code-review".into();
        review.kind = None;
        vec![review]
    });
    assert_eq!(
        legacy_fail.failure,
        Some(FailureClass::Review),
        "a kindless review that failed"
    );
}

#[test]
fn a_stage_prefix_still_names_an_escalation_or_a_blocker() {
    let escalated = failed_with("implement", "ESCALATE: needs adaptive", |run| {
        vec![settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed {
                error: "ESCALATE: x".into(),
            },
        )]
    });
    assert_eq!(escalated.failure, Some(FailureClass::Escalated));
    let blocked = failed_with("review", "BLOCKED: needs a token", |run| {
        vec![review_fail(run)]
    });
    assert_eq!(blocked.failure, Some(FailureClass::Blocked));
}
