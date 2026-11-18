//! V24 freezes a reviewed pull request into its outcome only on a recorded
//! reviewer verdict, and reads a stage's kind before its id.

use super::outcome_classes::*;
use super::*;
use archon_core::catalog::StageKind;
use archon_core::outcome::{FailureClass, Outcome};
use archon_core::run::{PipelineRun, StageRun, StageState, Verdict};

const PR: &str = "https://github.com/o/r/pull/7";

/// Makes the item of each run review [`PR`] and push nothing.
fn target(conn: &Connection, runs: &[&PipelineRun], item: serde_json::Value) {
    for run in runs {
        conn.execute(
            "UPDATE items SET data = ?2 WHERE id = ?1",
            [run.item.as_str(), &item.to_string()],
        )
        .unwrap();
    }
}

fn review_only() -> serde_json::Value {
    serde_json::json!({"onto": {"review_only": true, "pr": PR}})
}

fn frozen(conn: &Connection, run: &PipelineRun) -> Option<String> {
    let data: String = conn
        .query_row(
            "SELECT data FROM outcomes WHERE run = ?1",
            [run.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    let outcome: Outcome = serde_json::from_str(&data).expect("the outcome stays readable");
    outcome.review_pr
}

fn class(conn: &Connection, run: &PipelineRun) -> Option<String> {
    columns(conn, run).0
}

/// The bundled review pipeline's lead as 1.6.3 recorded a failed round: a
/// work stage that failed its task, with no duty kept.
fn failed_lead(run: &PipelineRun) -> StageRun {
    let lead = failed_stage(run, StageKind::Work, "blocking issues", false);
    assert!(lead.only_reviews.is_none(), "a 1.6.3 record");
    lead
}

#[test]
fn v24_freezes_a_pull_request_only_on_a_recorded_reviewer_verdict() {
    let mut conn = v23_store();
    let judged = failed_run("review", "x");
    let mut review = stage(&judged, "review", StageKind::Review, StageState::Completed);
    review.verdict = Some(Verdict::Fail);
    seed(&conn, &judged, &[review], Some(FailureClass::Review), false);
    let tallied = failed_run("review", "the review found blocking issues");
    let lead = failed_lead(&tallied);
    tally_a_fail(&conn, &lead);
    seed(&conn, &tallied, &[lead], Some(FailureClass::Other), false);
    let assigned = failed_run("review", "the checkout failed");
    let lead = failed_lead(&assigned);
    seed(&conn, &assigned, &[lead], Some(FailureClass::Other), false);
    let carried = succeeded_run();
    let mut lead = stage(&carried, "review", StageKind::Work, StageState::Completed);
    lead.verdict = Some(Verdict::Pass);
    lead.structured = Some(serde_json::json!({"review": {"verdict": "pass"}}));
    seed(&conn, &carried, &[lead], None, false);
    let unverified = succeeded_run();
    let mut lead = stage(
        &unverified,
        "review",
        StageKind::Work,
        StageState::Completed,
    );
    lead.verdict = Some(Verdict::Pass);
    lead.structured =
        Some(serde_json::json!({"review": {"verdict": "pass", "unverified": "no reviewer ran"}}));
    seed(&conn, &unverified, &[lead], None, false);
    // A level-none read of the pull request: it succeeds with no review.
    let unreviewed = succeeded_run();
    let mut read = stage(
        &unreviewed,
        "review",
        StageKind::Work,
        StageState::Completed,
    );
    read.verdict = Some(Verdict::Pass);
    seed(&conn, &unreviewed, &[read], None, false);
    let every = [&judged, &tallied, &assigned, &carried, &unverified];
    target(&conn, &every, review_only());
    let mut none = review_only();
    none["autonomy"] = serde_json::json!({"review": "none"});
    target(&conn, &[&unreviewed], none);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(frozen(&conn, &judged), some(PR), "a review stage's verdict");
    assert_eq!(
        frozen(&conn, &tallied),
        some(PR),
        "a reviewer's tallied fail"
    );
    assert_eq!(class(&conn, &tallied), some("review"));
    assert_eq!(
        frozen(&conn, &assigned),
        None,
        "a lead that failed with no reviewer verdict reviewed nothing"
    );
    assert_eq!(class(&conn, &assigned), some("other"), "its kind is work");
    assert_eq!(frozen(&conn, &carried), some(PR), "the review it carries");
    assert_eq!(
        frozen(&conn, &unverified),
        None,
        "Archon refused that review"
    );
    assert_eq!(
        frozen(&conn, &unreviewed),
        None,
        "level none records no review"
    );
}

#[test]
fn v24_reads_a_recorded_kind_before_the_stage_id() {
    let mut conn = v23_store();
    let mut input = failed_run("review-input", "the input was malformed");
    input.pipeline = "read".into();
    let work = failed_stage(&input, StageKind::Work, "the input was malformed", false);
    seed(&conn, &input, &[work], Some(FailureClass::Review), false);
    let kindless = |stage_id: &str, verdict: Option<Verdict>| {
        let run = failed_run(stage_id, "x");
        let state = match verdict {
            Some(_) => StageState::Completed,
            None => StageState::Failed { error: "x".into() },
        };
        let mut old = stage(&run, stage_id, StageKind::Review, state);
        old.kind = None;
        old.verdict = verdict;
        seed(&conn, &run, &[old], Some(FailureClass::Other), false);
        run
    };
    let judged = kindless("code-review", Some(Verdict::Fail));
    let unjudged = kindless("review-input", None);
    let hedged = kindless("review", Some(Verdict::Unknown));
    target(&conn, &[&input, &judged, &unjudged, &hedged], review_only());

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        class(&conn, &input),
        some("other"),
        "a work stage stays work"
    );
    assert_eq!(frozen(&conn, &input), None);
    assert_eq!(
        class(&conn, &judged),
        some("review"),
        "a kindless review with its verdict"
    );
    assert_eq!(frozen(&conn, &judged), some(PR));
    for run in [&unjudged, &hedged] {
        assert_eq!(
            class(&conn, run),
            some("other"),
            "an id alone names no review: {}",
            run_stage(run)
        );
        assert_eq!(frozen(&conn, run), None, "{}", run_stage(run));
    }
}

#[test]
fn v24_skips_a_pull_request_target_it_cannot_read() {
    let mut conn = v23_store();
    let reviewed = failed_run("review", "x");
    let mut review = stage(
        &reviewed,
        "review",
        StageKind::Review,
        StageState::Completed,
    );
    review.verdict = Some(Verdict::Fail);
    seed(
        &conn,
        &reviewed,
        &[review],
        Some(FailureClass::Review),
        false,
    );
    let odd = serde_json::json!({"onto": {"review_only": true, "pr": 42}});
    target(&conn, &[&reviewed], odd);
    let torn = failed_run("review", "x");
    let mut review = stage(&torn, "review", StageKind::Review, StageState::Completed);
    review.verdict = Some(Verdict::Fail);
    seed(&conn, &torn, &[review], Some(FailureClass::Review), false);
    conn.execute(
        "UPDATE stage_runs SET data = '{\"torn\": true}' WHERE run = ?1",
        [torn.id.as_str()],
    )
    .unwrap();
    target(&conn, &[&torn], review_only());

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(frozen(&conn, &reviewed), None);
    assert_eq!(frozen(&conn, &torn), None, "its stage runs cannot be read");
}
