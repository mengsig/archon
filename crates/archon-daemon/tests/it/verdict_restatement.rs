//! `submit_verdict`: a stage agent has its verdict read while it still runs,
//! is asked once to restate one Archon cannot read, and the restated one is
//! final. A disputed pass costs one extra turn and never a setup re-run, a
//! restated fail still fails, the one ask survives a restart, and a stage
//! that never calls the tool settles as it did before.

use crate::common;

use std::collections::BTreeMap;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{
    CheckDecision, Restatement, StageOutcome, StageRun, Verdict, VerdictCheck, VerdictNote,
};
use archon_daemon::executor;
use common::{Harness, MODEL, choice, completed};
use serde_json::{Value, json};

const E2E: &str = "e2e-testing";
const PASS: &str = "Ran every scenario.\nVERDICT: PASS";
const FAIL: &str = "The environment never came up.\nVERDICT: FAIL";

fn e2e_plan() -> ModelPlan {
    let stages = ["setup", "e2e", "report"];
    let one = |stage: &str| (stage.to_owned(), vec![choice(MODEL, Effort::Medium)]);
    ModelPlan(stages.into_iter().map(one).collect::<BTreeMap<_, _>>())
}

/// An `e2e-testing` item whose setup is done and whose `e2e` agent runs.
async fn at_e2e(h: &Harness) -> (QueueItem, StageRun) {
    let item = h.enqueue_in(E2E, "journeys", e2e_plan()).await.unwrap();
    h.dispatch().await;
    let setup = h.settle_running(|_| completed("start.sh ready")).await;
    assert_eq!(setup[0].stage_id, "setup");
    h.dispatch().await;
    let e2e = only_running(h, "e2e").await;
    (item, e2e)
}

async fn only_running(h: &Harness, stage_id: &str) -> StageRun {
    let mut running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].stage_id, stage_id);
    running.remove(0)
}

/// A pass beside the product defect an agent forgot to mark non-blocking.
fn disputed() -> Value {
    json!({"verdict": "pass", "findings": [
        {"severity": "high", "title": "checkout: pay button does nothing"}
    ]})
}

/// The same result with the finding marked as the stage's rule asks.
fn marked() -> Value {
    json!({"verdict": "pass", "findings": [{
        "severity": "high", "blocking": false, "fault": "product",
        "title": "checkout: pay button does nothing"
    }]})
}

async fn submit_as(
    h: &Harness,
    caller: Option<AetherAgentId>,
    summary: &str,
    structured: Value,
) -> Result<VerdictCheck, ArchonError> {
    let request = Request::SubmitVerdict {
        summary: summary.into(),
        structured: Some(structured),
        caller,
    };
    match h.cli(request).await? {
        Reply::VerdictCheck(check) => Ok(check),
        other => panic!("submit_verdict replied {other:?}"),
    }
}

async fn submit(h: &Harness, stage: &StageRun, summary: &str, structured: Value) -> VerdictCheck {
    let caller = stage.aether_agent.clone();
    assert!(caller.is_some(), "a running stage has an agent");
    let answered = submit_as(h, caller, summary, structured).await;
    answered.expect("the stage's own agent may submit")
}

fn result(summary: &str, structured: Value) -> StageOutcome {
    StageOutcome::Completed {
        summary: summary.into(),
        structured: Some(structured),
    }
}

async fn settle(h: &Harness, summary: &'static str, structured: Value) {
    let settled = h
        .settle_running(|_| result(summary, structured.clone()))
        .await;
    assert_eq!(settled.len(), 1, "{settled:?}");
    h.dispatch().await;
}

async fn stored(h: &Harness, item: &QueueItem, stage: &StageRun) -> StageRun {
    let stages = h.stages(&item.id).await;
    let found = stages.into_iter().find(|s| s.id == stage.id);
    found.expect("the stage run is stored")
}

async fn setup_runs(h: &Harness, item: &QueueItem) -> usize {
    let stages = h.stages(&item.id).await;
    stages.iter().filter(|s| s.stage_id == "setup").count()
}

async fn failure(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("expected a failed item, got {other:?}"),
    }
}

fn key(stage: &StageRun) -> String {
    format!("verdict_restatement:{}", stage.id)
}

#[tokio::test]
async fn a_disputed_pass_is_restated_once_and_the_gate_decides_without_re_running_setup() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let asked = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
    assert_eq!(asked.reading.note, Some(VerdictNote::DisputedPass));
    assert!(asked.message().contains("\"blocking\": true"), "{asked:?}");
    assert_eq!(only_running(&h, "e2e").await.id, e2e.id, "it still runs");

    let restated = submit(&h, &e2e, PASS, marked()).await;
    assert_eq!(restated.decision, CheckDecision::Final);
    assert_eq!(restated.reading.verdict, Verdict::Pass);
    settle(&h, PASS, marked()).await;

    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(
        (settled.verdict, settled.verdict_note),
        (Some(Verdict::Pass), None)
    );
    let record = settled.restatement.expect("the restatement is on record");
    assert_eq!(record.asked, VerdictNote::DisputedPass);
    assert_eq!(record.restated.map(|r| r.verdict), Some(Verdict::Pass));
    only_running(&h, "report").await;
    assert_eq!(setup_runs(&h, &item).await, 1, "setup is not re-run");
    assert_eq!(
        h.kv(&key(&e2e)).await,
        None,
        "the settled stage's ask is forgotten"
    );
}

#[tokio::test]
async fn a_restated_fail_fails_even_if_the_stage_then_completes_with_a_pass() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let product = json!({"verdict": "fail", "findings": [
        {"severity": "high", "blocking": true, "fault": "product", "title": "pay button"}
    ]});
    let restated = submit(&h, &e2e, FAIL, product).await;
    assert_eq!(restated.decision, CheckDecision::Final);
    assert_eq!(restated.reading.verdict, Verdict::Fail);
    assert!(restated.message().contains("a fail"), "{restated:?}");

    let later = submit(&h, &e2e, PASS, marked()).await;
    assert_eq!(later.decision, CheckDecision::Final);
    assert_eq!(
        later.reading.verdict,
        Verdict::Fail,
        "a third call changes nothing"
    );
    settle(&h, PASS, marked()).await;

    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(settled.verdict, Some(Verdict::Fail));
    let error = failure(&h, &item).await;
    assert!(error.contains("stage `e2e` failed"), "{error}");
    assert!(error.contains("`setup` was not re-run"), "{error}");
    assert!(
        error.contains("fail, as it restated before completing (final)"),
        "{error}"
    );
    assert_eq!(setup_runs(&h, &item).await, 1);
}

#[tokio::test]
async fn a_restatement_still_disputed_settles_as_a_disputed_pass_does() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let again = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(again.decision, CheckDecision::Final);
    assert_eq!(again.reading.note, Some(VerdictNote::DisputedPass));
    assert!(again.message().contains("counts as a fail"), "{again:?}");
    settle(&h, PASS, marked()).await;

    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(
        (settled.verdict, settled.verdict_note),
        (Some(Verdict::Unknown), Some(VerdictNote::DisputedPass)),
        "a readable result after a final unreadable restatement does not pass"
    );
    assert!(failure(&h, &item).await.contains("`setup` was not re-run"));
    assert_eq!(setup_runs(&h, &item).await, 1);
}

#[tokio::test]
async fn the_one_ask_survives_a_restart() {
    let mut h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let asked = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
    let kept: Restatement = serde_json::from_str(&h.kv(&key(&e2e)).await.unwrap()).unwrap();
    assert_eq!(
        (kept.asked, kept.restated),
        (VerdictNote::DisputedPass, None)
    );

    h.restart();
    executor::recover(&h.ctx).await.expect("recovery");
    let again = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(again.decision, CheckDecision::Final, "never asked twice");
    assert_eq!(again.reading.note, Some(VerdictNote::DisputedPass));

    h.restart();
    executor::recover(&h.ctx).await.expect("recovery");
    let third = submit(&h, &e2e, PASS, marked()).await;
    assert_eq!(third.decision, CheckDecision::Final);
    assert_eq!(
        third.reading.verdict,
        Verdict::Unknown,
        "the final one stands"
    );
    settle(&h, PASS, marked()).await;
    assert_eq!(
        stored(&h, &item, &e2e).await.verdict,
        Some(Verdict::Unknown)
    );
    assert_eq!(setup_runs(&h, &item).await, 1);
}

#[tokio::test]
async fn a_readable_submission_is_accepted_and_binds_nothing() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let accepted = submit(&h, &e2e, FAIL, json!({"verdict": "fail", "findings": []})).await;
    assert_eq!(accepted.decision, CheckDecision::Accepted);
    assert_eq!(h.kv(&key(&e2e)).await, None, "no ask, nothing kept");
    settle(&h, PASS, marked()).await;
    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(
        settled.verdict,
        Some(Verdict::Pass),
        "only its result is read"
    );
    assert_eq!(settled.restatement, None);
    only_running(&h, "report").await;
}

#[tokio::test]
async fn an_ask_never_answered_leaves_the_result_as_read() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, "I looked around.", json!({})).await;
    settle(&h, PASS, marked()).await;
    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(settled.verdict, Some(Verdict::Pass));
    let record = settled.restatement.expect("the ask is on record");
    assert_eq!(
        (record.asked, record.restated),
        (VerdictNote::MissingVerdictLine, None)
    );
    only_running(&h, "report").await;
}

#[tokio::test]
async fn a_stage_that_never_submits_settles_as_before() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    settle(&h, PASS, disputed()).await;
    let settled = stored(&h, &item, &e2e).await;
    assert_eq!(
        (settled.verdict, settled.verdict_note, settled.restatement),
        (
            Some(Verdict::Unknown),
            Some(VerdictNote::DisputedPass),
            None
        )
    );
    let error = failure(&h, &item).await;
    assert!(error.contains("`setup` was not re-run"), "{error}");
    assert_eq!(setup_runs(&h, &item).await, 1);

    // A pipeline whose loop has no `when` still loops on any failed gate.
    let item = h.enqueue("review without the tool").await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|_| result(PASS, disputed())).await;
    h.dispatch().await;
    let again = only_running(&h, "implement").await;
    assert_eq!(again.attempt, 2, "the fix loop runs as it did");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn only_a_running_verdict_stage_s_own_agent_may_submit() {
    let h = Harness::new().await;
    let item = h.enqueue_in(E2E, "journeys", e2e_plan()).await.unwrap();
    h.dispatch().await;
    let setup = only_running(&h, "setup").await;
    let refused = |answer: Result<VerdictCheck, ArchonError>, why: &str| match answer {
        Err(ArchonError::Forbidden(text)) => assert!(text.contains(why), "{text}"),
        other => panic!("expected a refusal naming {why:?}, got {other:?}"),
    };
    let from_setup = submit_as(&h, setup.aether_agent.clone(), PASS, marked()).await;
    refused(from_setup, "stage `setup` has not");
    refused(submit_as(&h, None, PASS, marked()).await, "caller metadata");
    let stranger = AetherAgentId::parse("agt_stranger").unwrap();
    refused(
        submit_as(&h, Some(stranger), PASS, marked()).await,
        "has none on record",
    );

    h.settle_running(|_| completed("start.sh ready")).await;
    h.dispatch().await;
    let e2e = only_running(&h, "e2e").await;
    settle(&h, PASS, marked()).await;
    let finished = submit_as(&h, e2e.aether_agent.clone(), PASS, disputed()).await;
    refused(finished, "while its stage runs");
    assert_eq!(h.kv(&key(&e2e)).await, None, "a refused call keeps nothing");
    assert_eq!(setup_runs(&h, &item).await, 1);
}
