//! A failed run's class comes from its stage records, never its error's words.
mod common;
use archon_core::catalog::StageKind;
use archon_core::outcome::{ClassSource, FailureClass, Outcome};
use archon_core::queue::QueueState;
use archon_core::run::{
    BUDGET_STAGE, PipelineRun, RunState, StageOutcome, StageRun, StageState, Verdict,
    review_tally_key,
};
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
    failed_in(&item(None, "classed").pipeline, stage_id, error, stages)
}

/// The outcome of a run of `pipeline` that failed under `stage_id`.
fn failed_in(
    pipeline: &str,
    stage_id: &str,
    error: &str,
    stages: impl Fn(&PipelineRun) -> Vec<StageRun>,
) -> Outcome {
    captured(pipeline, stage_id, error, stages, false)
}

/// Like [`failed_in`]; `tallied` keeps, for each stage, the review tally
/// archond writes when one of its reviewers fails the work.
fn captured(
    pipeline: &str,
    stage_id: &str,
    error: &str,
    stages: impl Fn(&PipelineRun) -> Vec<StageRun>,
    tallied: bool,
) -> Outcome {
    let mut store = store();
    let mut classed = item(None, "classed");
    classed.pipeline = pipeline.into();
    let item = store.insert_item(&classed, None).unwrap();
    let run = start(&mut store, &item.id);
    let fail_tally = serde_json::json!({
        "fails": [{"agent": "agt_r", "head": "abc1234", "findings": ["a bug"]}],
        "read": ["agt_r"],
    });
    for stage in stages(&run) {
        store.upsert_stage_run(&stage).unwrap();
        if tallied {
            let key = review_tally_key(&stage.id);
            store.kv_set(&key, &fail_tally.to_string()).unwrap();
        }
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
        let error = "cost budget of $2: $2.0100 spent".into();
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
        let error = "Stopped by its budget (cost budget of $2: $2.0100 spent); its live \
                     subtree was cancelled with it, 0 descendant(s) were live at the stop. Details are \
                     in the exhaustion.failed event."
            .into();
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
    let spaced = failed_with(
        "implement",
        "later",
        claimed(serde_json::json!({"verdict": "PASS\n"})),
    );
    assert_eq!(
        spaced.failure,
        Some(FailureClass::Other),
        "the gate reads a verdict on one line"
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
    let unjudged = failed_with("review", "x", |run| {
        let error = "x".into();
        let mut old = settled(run, "review", StageKind::Work, StageState::Failed { error });
        old.kind = None;
        vec![old]
    });
    assert_eq!(
        unjudged.failure,
        Some(FailureClass::Other),
        "a kindless stage's id alone names no review"
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
fn a_task_that_quotes_a_cost_budget_is_no_budget_stop() {
    for quoted in [
        "the change exceeds the cost budget of $2.00 stated in the PR",
        "cost budget of $5 is too small for this job",
        "cost budget of $5: the plan needs $9 spent on reviewers alone",
        "cost budget of $5: the plan needs $9 spent",
        "cost budget of $5: we have insufficient test time and the remaining day is spent",
        "cost budget of $5: ≥ $9 of my time spent",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); this is my own explanation",
        "cost budget of $5: ≥ $9.0000 (for the smoke tests) spent",
        "cost budget of $5: not reported (by anyone) spent",
        "cost budget of $5: $9 spent",
        "cost budget of $2: $2.01 spent",
        "cost budget of $5.00: $5.0100 spent",
        "cost budget of $5: ≥ $9 (some unpriced) spent",
        "cost budget of $5: not reported (some unpriced) spent",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); no new subtree cancellation \
         was requested by the release plan",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); its live subtree was cancelled \
         with it, many descendant(s) were live at the stop.",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); no new subtree cancellation \
         was requested, 0 descendant(s) were live at the stop. This sentence describes a \
         fictional example from the review.",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); no new subtree cancellation \
         was requested, 0 descendant(s) were live at the stop.",
        "cost budget of $5: $5.0100 spent (notional cost: as the reviewer sees it)",
        "cost budget of $5: $5.0100 spent, which the plan allowed",
        "cost budget of $1e3: $5.0100 spent",
        "cost budget of $5 or so: $5.0100 spent",
        " cost budget of $5: $5.0100 spent",
        "Stopped by its budget (cost budget of $5: $5.0100 spent); no new subtree cancellation \
         was requested, 0 descendant(s) were live at the stop. Details are in the \
         exhaustion.failed event. (notional cost: Claude Code reports list-price dollars)",
    ] {
        quoted_budget(quoted);
    }
}

#[test]
fn aethers_cost_stop_is_read_in_each_shape_it_takes() {
    let message = "Stopped by its budget (cost budget of $3: $3.0001 spent); no new subtree \
                   cancellation was requested, 2 descendant(s) were live at the stop. Details \
                   are in the exhaustion.failed event.";
    let label = StageOutcome::NOTIONAL_COST_LABEL;
    for stop in [
        "cost budget of $1: ≥ $1.0200 (some unpriced) spent".to_owned(),
        "cost budget of $0.5: not reported spent".to_owned(),
        "cost budget of $0.5: not reported (usage not fully reported) spent".to_owned(),
        "cost budget of $0.5: ≥ $0.6000 (interrupted run, cost unknown) spent".to_owned(),
        message.to_owned(),
        format!("{message}{label}"),
        format!("cost budget of $2: $2.0100 spent{label}"),
    ] {
        let capped = failed_with("implement", "stage failed", |run| {
            let error = stop.clone();
            vec![settled(
                run,
                "implement",
                StageKind::Work,
                StageState::Failed { error },
            )]
        });
        assert_eq!(capped.failure, Some(FailureClass::Budget), "{stop}");
    }
}

fn quoted_budget(quoted: &'static str) {
    let review = failed_with("review", quoted, |run| {
        let mut lead = settled(
            run,
            "review",
            StageKind::Work,
            StageState::Failed {
                error: quoted.into(),
            },
        );
        lead.only_reviews = Some(true);
        vec![lead]
    });
    assert_eq!(review.failure, Some(FailureClass::Review), "{quoted}");
    let work = failed_with("implement", quoted, |run| {
        let error = quoted.into();
        vec![settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed { error },
        )]
    });
    assert_eq!(work.failure, Some(FailureClass::Other), "{quoted}");
    assert!(work.is_scored(), "the task judged its own work");
}

/// A run whose work-kind `stage_id` failed its task, as the bundled review
/// pipeline's lead does; `only_reviews` is `None` on a 1.6.3 record.
fn failed_lead(
    pipeline: &str,
    only_reviews: Option<bool>,
    stage_id: &'static str,
    tallied: bool,
) -> Outcome {
    let stages = move |run: &PipelineRun| {
        let error = MISLEADING.into();
        let mut lead = settled(run, stage_id, StageKind::Work, StageState::Failed { error });
        lead.only_reviews = only_reviews;
        vec![lead]
    };
    captured(pipeline, stage_id, MISLEADING, stages, tallied)
}

#[test]
fn a_review_only_lead_that_fails_its_task_is_a_review_failure() {
    let review = failed_lead("review", Some(true), "review", false);
    assert_eq!(review.failure, Some(FailureClass::Review));
    assert_eq!(review.failure_source, Some(ClassSource::Structure));
    let named = failed_lead("review", Some(false), "review", false);
    assert_eq!(
        named.failure,
        Some(FailureClass::Other),
        "a recorded duty outranks the id"
    );
    let implement = failed_lead("direct", Some(false), "implement", false);
    assert_eq!(implement.failure, Some(FailureClass::Other));
}

#[test]
fn a_recorded_work_kind_wins_over_a_stage_id_that_says_review() {
    for (pipeline, stage_id) in [
        ("review", "review"),
        ("direct", "review"),
        ("review", "review-input"),
    ] {
        let unrelated = failed_lead(pipeline, None, stage_id, false);
        assert_eq!(
            unrelated.failure,
            Some(FailureClass::Other),
            "a 1.6.3 work stage is no review by its name: {pipeline} / {stage_id}"
        );
    }
    let tallied = failed_lead("review", None, "review", true);
    assert_eq!(
        tallied.failure,
        Some(FailureClass::Review),
        "a reviewer's recorded fail is what makes a 1.6.3 lead's failure a review's"
    );
    let holder = failed_lead("direct", Some(false), "implement", true);
    assert_eq!(
        holder.failure,
        Some(FailureClass::Review),
        "a stage whose reviewer failed its work, failing through fail_task"
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

/// The daemon's budget stop fails the run itself, under a stage it did not
/// let run; a stage that ends its own task in those words blocked.
#[test]
fn a_stage_that_blocks_in_the_budget_stops_words_is_blocked() {
    let said = "BLOCKED: the item's budget is used up in the customer sandbox";
    let own = failed_with("implement", said, |run| {
        let error = said.into();
        let mut stage = settled(
            run,
            "implement",
            StageKind::Work,
            StageState::Failed { error },
        );
        stage.aether_agent = Some(agent(7));
        vec![stage]
    });
    assert_eq!(own.failure, Some(FailureClass::Blocked));
    let stop = "BLOCKED: the item's budget is used up: $1.00 spent of its $1.00 cap ($0.00 over)";
    let before_a_retry = failed_with("review", stop, |run| vec![review_fail(run)]);
    assert_eq!(
        before_a_retry.failure,
        Some(FailureClass::Budget),
        "the daemon stopped the next attempt; the last one did not say so"
    );
}

/// A spawn the daemon gave up on left no agent. An agent that fails its task
/// in the daemon's words judged its own work.
#[test]
fn a_spawn_failure_is_the_daemons_only_where_no_agent_ran() {
    for gave_up in [
        "create_agent failed on a full disk 5 times (No space left on device)",
        "create_agent kept failing: aether: internal: storage temporarily unavailable",
    ] {
        let spawn = |attached: bool| {
            failed_with("implement", "stage `implement` failed", move |run| {
                let error = gave_up.into();
                let mut stage = settled(
                    run,
                    "implement",
                    StageKind::Work,
                    StageState::Failed { error },
                );
                stage.aether_agent = attached.then(|| agent(7));
                vec![stage]
            })
        };
        let unspawned = spawn(false);
        assert_eq!(
            unspawned.failure,
            Some(FailureClass::Infrastructure),
            "{gave_up}"
        );
        assert!(!unspawned.is_scored(), "no agent judged the work");
        let quoted = spawn(true);
        assert_eq!(quoted.failure, Some(FailureClass::Other), "{gave_up}");
        assert!(quoted.is_scored(), "the agent failed its own task");
    }
}

/// Recovery fails a run it cannot resume after dropping the stage rows it
/// could not read, so the rows that remain do not say what stopped it.
#[test]
fn a_run_recovery_could_not_resume_is_an_infrastructure_failure() {
    let error = format!(
        "{}corrupt: decode stage run. `archon queue retry qi_x` runs it again once fixed",
        archon_core::run::UNRESUMABLE_RUN
    );
    let dropped = failed_with("review", &error, |run| vec![review_fail(run)]);
    assert_eq!(dropped.failure, Some(FailureClass::Infrastructure));
    assert!(!dropped.is_scored(), "no reviewer failed the work");
    let rowless = failed_with("review", &error, |_| Vec::new());
    assert_eq!(rowless.failure, Some(FailureClass::Infrastructure));
}

/// Only a tally that names the reviewer of each fail is a reviewer's verdict.
#[test]
fn a_tally_that_names_no_reviewer_is_no_review() {
    let lead_with = |tally: serde_json::Value| {
        let mut store = store();
        let item = store.insert_item(&item(None, "classed"), None).unwrap();
        let run = start(&mut store, &item.id);
        let error = MISLEADING.into();
        let lead = settled(
            &run,
            "review",
            StageKind::Work,
            StageState::Failed { error },
        );
        store.upsert_stage_run(&lead).unwrap();
        let key = review_tally_key(&lead.id);
        store.kv_set(&key, &tally.to_string()).unwrap();
        fail(&mut store, &run, "review", MISLEADING);
        let captured = store.capture_outcome(&store.run(&run.id).unwrap(), None, true);
        captured.map(|()| store.outcome(&run.id).unwrap().unwrap().failure)
    };
    let unnamed = lead_with(serde_json::json!({"fails": [null]}));
    assert!(
        matches!(unnamed, Err(archon_core::error::ArchonError::Corrupt(_))),
        "{unnamed:?}"
    );
    let blank = lead_with(serde_json::json!({"fails": [{"agent": " "}]}));
    assert_eq!(blank.unwrap(), Some(FailureClass::Other));
    let empty = lead_with(serde_json::json!({"fails": []}));
    assert_eq!(empty.unwrap(), Some(FailureClass::Other));
    let other = lead_with(serde_json::json!({"fails": [], "other_fails": [["agt_r", "abc1234"]]}));
    assert_eq!(other.unwrap(), Some(FailureClass::Review));
}
