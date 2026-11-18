//! The model evidence report's pipeline lines: only runs in its range, and a
//! run a stage handed on (BLOCKED, ESCALATE) is no loss.
mod common;
use archon_core::Timestamp;
use archon_core::cost::DayRange;
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
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
    let mut item = item(None, &format!("review #{pr}"));
    item.pipeline = "review".into();
    item.onto = Some(Box::new(OntoPullRequest {
        target: OntoTarget::Pr(pr),
        pr: PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{pr}")).unwrap(),
        node_id: None,
        head_repository: "o/r".into(),
        head_ref: "feature".into(),
        base_ref: "main".into(),
        head_sha: "a".repeat(40),
        review_only: true,
    }));
    item
}

/// Runs `item` once to `state` at `at` and captures its outcome.
fn finish(store: &mut Store, item: &QueueItem, state: RunState, at: Timestamp) {
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
    let many = reviewing(42);
    for _ in 0..4 {
        finish(&mut store, &many, failed("the review failed"), at);
    }
    finish(&mut store, &many, passed(), at);
    let once = reviewing(7);
    finish(&mut store, &once, passed(), at);
    finish(&mut store, &reviewing(9), failed("the review failed"), at);
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
}
