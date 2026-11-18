//! The model evidence report's pipeline lines: only runs in its range, and a
//! run a stage handed on (BLOCKED, ESCALATE) is no loss.
mod common;
use archon_core::Timestamp;
use archon_core::catalog::StageKind;
use archon_core::cost::DayRange;
use archon_core::outcome::FailureClass;
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState, Verdict, review_tally_key};
use archon_queue::Store;
use common::*;

const DAY: i64 = 86_400;

fn day(at: Timestamp) -> chrono::NaiveDate {
    at.with_timezone(&chrono::Local).date_naive()
}

fn range(at: Timestamp) -> DayRange {
    DayRange {
        since: day(at),
        until: day(at),
    }
}

fn reviewing(pr: u64) -> QueueItem {
    reviewing_at(pr, 'a')
}

/// A review-only item of pull request `pr` frozen at a head of `head` digits.
fn reviewing_at(pr: u64, head: char) -> QueueItem {
    assert!(head.is_ascii_hexdigit(), "a head is hex");
    let mut item = item(None, &format!("review #{pr}"));
    item.pipeline = "review".into();
    item.onto = Some(Box::new(OntoPullRequest {
        target: OntoTarget::Pr(pr),
        pr: PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{pr}")).unwrap(),
        node_id: None,
        head_repository: "o/r".into(),
        head_ref: "feature".into(),
        base_ref: "main".into(),
        head_sha: head.to_string().repeat(40),
        review_only: true,
    }));
    item
}

/// Runs `item` once to `state` at `at` and captures its outcome. A run of
/// the review pipeline leaves the reviewer verdict such a run records.
fn finish(store: &mut Store, item: &QueueItem, state: RunState, at: Timestamp) {
    finish_with(store, item, state, at, item.pipeline == "review");
}

/// Like [`finish`]; `judged` says whether a reviewer's verdict is recorded.
fn finish_with(store: &mut Store, item: &QueueItem, state: RunState, at: Timestamp, judged: bool) {
    finish_saying(store, item, state, at, judged.then_some("pass"));
}

/// Like [`finish`]; `verdict` is the word the recorded review states, if any.
fn finish_saying(
    store: &mut Store,
    item: &QueueItem,
    state: RunState,
    at: Timestamp,
    verdict: Option<&str>,
) {
    let judged = verdict.is_some();
    let id = match store.item(&item.id) {
        Ok(stored) => {
            store
                .transition_item(&stored.id, QueueState::Queued, at)
                .unwrap();
            stored.id
        }
        Err(_) => store.insert_item(item, None).unwrap().id,
    };
    let run = start(store, &id);
    let lead = lead(item, &run, &state, at, verdict);
    store.upsert_stage_run(&lead).unwrap();
    if judged && matches!(state, RunState::Failed { .. }) {
        let tally = serde_json::json!({
            "fails": [{"agent": "agt_r", "head": "abc1234", "findings": ["a bug"]}],
            "read": ["agt_r"],
        });
        let key = review_tally_key(&lead.id);
        store.kv_set(&key, &tally.to_string()).unwrap();
    }
    let next = match &state {
        RunState::Succeeded { summary } => QueueState::Succeeded {
            run: run.id.clone(),
            summary: summary.clone(),
        },
        RunState::Failed { error, .. } => QueueState::Failed {
            run: Some(run.id.clone()),
            error: error.clone(),
        },
        other => panic!("{other:?} is not a finish"),
    };
    let finished = PipelineRun {
        state,
        finished_at: Some(at),
        ..run
    };
    store.finish_run(&finished, next, at).unwrap();
    store
        .capture_outcome(&store.run(&finished.id).unwrap(), None, true)
        .unwrap();
}

/// The stage as the bundled pipelines record it: the review pipeline's lead
/// is a work stage that holds the review. A pass carries the review in its
/// result; a fail goes through `fail_task`, and only archond's tally keeps it.
fn lead(
    item: &QueueItem,
    run: &PipelineRun,
    state: &RunState,
    at: Timestamp,
    verdict: Option<&str>,
) -> StageRun {
    let mut ran = stage(run, 0);
    ran.stage_id = "review".into();
    ran.kind = Some(StageKind::Work);
    ran.only_reviews = Some(item.pipeline == "review");
    ran.state = match state {
        RunState::Failed { error, .. } => StageState::Failed {
            error: error.clone(),
        },
        _ => StageState::Completed,
    };
    if ran.state == StageState::Completed {
        ran.verdict = Some(Verdict::Pass);
        ran.structured = verdict.map(|said| serde_json::json!({"review": {"verdict": said}}));
    }
    ran.finished_at = Some(at);
    ran
}

fn passed() -> RunState {
    RunState::Succeeded {
        summary: "VERDICT: PASS".into(),
    }
}

fn failed(error: &str) -> RunState {
    RunState::Failed {
        stage_id: "review".into(),
        error: error.into(),
    }
}

#[test]
fn a_pipeline_with_no_run_in_the_range_has_no_line() {
    let mut store = store();
    let now = t(DAY * 10);
    finish(&mut store, &item(None, "old"), passed(), t(50));
    finish(&mut store, &reviewing(1), passed(), now);
    let lines = store.pipeline_lines(None, None, &range(now)).unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!((lines[0].pipeline.as_str(), lines[0].runs), ("review", 1));
    let earlier = store.pipeline_lines(None, None, &range(t(50))).unwrap();
    assert_eq!(
        earlier
            .iter()
            .map(|l| l.pipeline.as_str())
            .collect::<Vec<_>>(),
        ["direct"]
    );
    let filtered = store
        .pipeline_lines(None, Some("direct"), &range(now))
        .unwrap();
    assert!(filtered.is_empty());
    assert_eq!(
        store.learning_stats(None).unwrap().pipelines.len(),
        2,
        "lessons keep the lifetime"
    );
}

#[test]
fn blocked_and_escalated_runs_are_no_loss_and_have_their_own_columns() {
    let mut store = store();
    let at = t(DAY);
    finish(
        &mut store,
        &reviewing(1),
        failed("BLOCKED: needs a token"),
        at,
    );
    finish(
        &mut store,
        &reviewing(2),
        failed("ESCALATE: not a review"),
        at,
    );
    finish(&mut store, &reviewing(3), passed(), at);
    finish(&mut store, &reviewing(4), failed("the review failed"), at);
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    let line = &lines[0];
    assert_eq!(
        (line.runs, line.scored, line.blocked, line.escalated),
        (4, 2, 1, 1)
    );
    assert_eq!(line.pass_rate, 0.5);
    let text = archon_core::model_evidence::ModelEvidenceReport {
        pipelines: lines,
        ..serde_json::from_value(serde_json::json!({
            "range": range(at), "estimates_counted": true, "rows": [], "omitted": 0
        }))
        .unwrap()
    }
    .text();
    assert!(
        text.contains("scored=2 pass=50% blocked=1 escalated=1"),
        "{text}"
    );
}

#[test]
fn a_pull_request_reviewed_at_five_heads_counts_once() {
    let mut store = store();
    let at = t(DAY);
    let first = reviewing_at(42, '1');
    finish(&mut store, &first, failed("the review failed"), at);
    for head in ['2', '3', '4'] {
        let retry = reviewing_at(42, head);
        finish(&mut store, &retry, failed("the review failed"), at);
    }
    finish(&mut store, &reviewing_at(42, '5'), passed(), at);
    finish(&mut store, &reviewing(7), passed(), at);
    finish(&mut store, &reviewing(9), failed("the review failed"), at);
    let failed_round = store.outcome(&store.runs_for_item(&first.id).unwrap()[0].id);
    let failed_round = failed_round.unwrap().unwrap();
    assert_eq!(failed_round.failure, Some(FailureClass::Review));
    assert!(
        failed_round.review_pr.is_some(),
        "a failed round reviewed it"
    );
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    let prs = lines[0].review_prs.as_ref().expect("a review pipeline");
    assert_eq!((prs.reviewed, prs.passed), (3, 2));
    assert_eq!(
        prs.median_rounds_to_pass,
        Some(3.0),
        "the median of 5 and 1"
    );
    let build = store
        .pipeline_lines(None, Some("direct"), &range(at))
        .unwrap();
    assert!(build.is_empty());

    let pruned = store
        .prune_history(t(DAY * 400), 100, |_, _| false)
        .unwrap();
    assert!(pruned.total() > 0, "{pruned:?}");
    assert!(store.item(&first.id).is_err(), "the item is pruned");
    let kept = store.pipeline_lines(None, None, &range(at)).unwrap();
    assert_eq!(
        kept[0].review_prs, lines[0].review_prs,
        "pruning keeps the evidence"
    );
}

#[test]
fn a_pipeline_that_reviews_nothing_reviews_no_pull_request() {
    let mut store = store();
    let at = t(DAY);
    let mut read = reviewing(5);
    read.pipeline = "read".into();
    finish(&mut store, &read, passed(), at);
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    assert_eq!((lines[0].pipeline.as_str(), lines[0].runs), ("read", 1));
    assert_eq!(lines[0].review_prs, None, "its target alone is no review");
}

#[test]
fn a_level_none_read_that_records_no_review_reviews_no_pull_request() {
    let mut store = store();
    let at = t(DAY);
    let mut unreviewed = reviewing(11);
    unreviewed.autonomy.review = archon_core::autonomy::ReviewLevel::None;
    finish_with(&mut store, &unreviewed, passed(), at, false);
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    assert_eq!((lines[0].runs, lines[0].scored), (1, 1), "{lines:?}");
    assert_eq!(
        lines[0].review_prs, None,
        "the run succeeded, but no reviewer judged the pull request"
    );
    finish(&mut store, &reviewing(12), passed(), at);
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    let prs = lines[0].review_prs.as_ref().expect("one was reviewed");
    assert_eq!((prs.reviewed, prs.passed), (1, 1));
}

#[test]
fn being_assigned_a_review_is_no_reviewed_pull_request() {
    let mut store = store();
    let at = t(DAY);
    let unreached = reviewing(21);
    let failed_before = failed("the checkout failed");
    finish_with(&mut store, &unreached, failed_before, at, false);
    finish_with(&mut store, &reviewing(22), passed(), at, false);
    let run = &store.runs_for_item(&unreached.id).unwrap()[0];
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(
        outcome.failure,
        Some(FailureClass::Review),
        "the review-only lead failed"
    );
    assert_eq!(outcome.review_pr, None, "no reviewer verdict is on record");
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    assert_eq!((lines[0].runs, lines[0].scored), (2, 2));
    assert_eq!(lines[0].review_prs, None);
}

/// The gate reads a nested verdict on one line, so a pass it took with a
/// line break after it reviewed the pull request.
#[test]
fn a_verdict_the_gate_took_across_a_line_break_reviewed_its_pull_request() {
    let mut store = store();
    let at = t(DAY);
    let spaced = reviewing(31);
    finish_saying(&mut store, &spaced, passed(), at, Some("PASS\n"));
    let run = &store.runs_for_item(&spaced.id).unwrap()[0];
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert!(outcome.review_pr.is_some(), "{outcome:?}");
    finish_saying(&mut store, &reviewing(32), passed(), at, Some("not sure"));
    // The gate reads a verdict's first 400 characters: these say pass.
    let capped = reviewing(33);
    let long_pass = format!("PASS{}x", "-".repeat(396));
    finish_saying(&mut store, &capped, passed(), at, Some(&long_pass));
    let run = &store.runs_for_item(&capped.id).unwrap()[0];
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert!(outcome.review_pr.is_some(), "{outcome:?}");
    let overlong = format!("PASS{}x", "-".repeat(395));
    finish_saying(&mut store, &reviewing(34), passed(), at, Some(&overlong));
    let lines = store.pipeline_lines(None, None, &range(at)).unwrap();
    let prs = lines[0].review_prs.as_ref().expect("two were reviewed");
    assert_eq!(
        (prs.reviewed, prs.passed),
        (2, 2),
        "an unread word is no verdict"
    );
}
