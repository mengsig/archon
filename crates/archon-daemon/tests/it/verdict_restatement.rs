//! `submit_verdict`: a stage agent has its verdict read while it still runs,
//! is asked once to restate one Archon cannot read, and the restated one is
//! final. A disputed pass costs one extra turn and never a setup re-run, a
//! restated fail still fails, the one ask survives a restart, and a stage
//! that never calls the tool settles as it did before.

use crate::common;
mod bindings;
mod callers;
mod cleanup;
mod records;
mod replaced;

use std::collections::BTreeMap;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::RequestId;
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{
    CheckDecision, Restatement, StageOutcome, StageRun, Verdict, VerdictCheck, VerdictNote,
};
use archon_daemon::{executor, handlers};
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

/// The `submit_verdict` call naming `caller`, as the tool builds it.
fn call(
    caller: Option<&AetherAgentId>,
    summary: &str,
    structured: Value,
    restating: bool,
) -> Request {
    Request::SubmitVerdict {
        summary: summary.into(),
        structured: Some(structured),
        restating,
        caller: caller.cloned(),
    }
}

/// `request` as `archon mcp` forwards a stage agent's tool call for `agent`.
async fn forwarded(
    h: &Harness,
    agent: &AetherAgentId,
    request: Request,
) -> Result<VerdictCheck, ArchonError> {
    match forwarded_reply(h, agent, request).await? {
        Reply::VerdictCheck(check) => Ok(check),
        other => panic!("submit_verdict replied {other:?}"),
    }
}

async fn forwarded_reply(
    h: &Harness,
    agent: &AetherAgentId,
    request: Request,
) -> Result<Reply, ArchonError> {
    let tracked = Request::Tracked {
        request_id: RequestId::new(),
        idempotency_key: None,
        request: Box::new(request),
    };
    let request = Request::ForAgent {
        caller: agent.clone(),
        request: Box::new(tracked),
    };
    Box::pin(handlers::handle(&h.ctx, ClientKind::ArchonMcp, request)).await
}

async fn sent(
    h: &Harness,
    stage: &StageRun,
    summary: &str,
    structured: Value,
    restating: bool,
) -> VerdictCheck {
    let agent = stage
        .aether_agent
        .as_ref()
        .expect("a running stage has an agent");
    let request = call(Some(agent), summary, structured, restating);
    let answered = forwarded(h, agent, request).await;
    answered.expect("the stage's own agent may submit")
}

/// The stage's agent has the result it is about to finish with read.
async fn submit(h: &Harness, stage: &StageRun, summary: &str, structured: Value) -> VerdictCheck {
    sent(h, stage, summary, structured, false).await
}

/// The stage's agent sends the restatement it was asked for.
async fn restate(h: &Harness, stage: &StageRun, summary: &str, structured: Value) -> VerdictCheck {
    sent(h, stage, summary, structured, true).await
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

/// The store key of the stage attempt `stage` is a run of.
fn key(stage: &StageRun) -> String {
    let (run, attempt, fan) = (&stage.run, stage.attempt, stage.fan_index);
    format!(
        "verdict_restatement:{run}:{attempt}:{fan}:{}",
        stage.stage_id
    )
}

async fn kept(h: &Harness, stage: &StageRun) -> Option<Restatement> {
    let raw = h.kv(&key(stage)).await?;
    Some(serde_json::from_str(&raw).expect("a stored restatement"))
}

async fn restarted(h: &mut Harness) {
    h.restart();
    executor::recover(&h.ctx).await.expect("recovery");
}

/// A fail whose one blocking finding names `fault` as at fault.
fn failing(fault: &str) -> Value {
    json!({"verdict": "fail", "findings": [
        {"severity": "high", "blocking": true, "fault": fault, "title": "the run stopped"}
    ]})
}

#[tokio::test]
async fn a_disputed_pass_is_restated_once_and_the_gate_decides_without_re_running_setup() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let asked = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
    assert_eq!(asked.reading.note, Some(VerdictNote::DisputedPass));
    assert!(asked.message().contains("\"blocking\": true"), "{asked:?}");
    assert!(asked.message().contains("\"restating\": true"), "{asked:?}");
    assert_eq!(only_running(&h, "e2e").await.id, e2e.id, "it still runs");

    let restated = restate(&h, &e2e, PASS, marked()).await;
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
        kept(&h, &e2e).await,
        None,
        "the ended attempt's ask is forgotten"
    );
}

#[tokio::test]
async fn a_restated_fail_fails_even_if_the_stage_then_completes_with_a_pass() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let restated = restate(&h, &e2e, FAIL, failing("product")).await;
    assert_eq!(restated.decision, CheckDecision::Final);
    assert_eq!(restated.reading.verdict, Verdict::Fail);
    assert!(restated.message().contains("a fail"), "{restated:?}");

    for restating in [false, true] {
        let later = sent(&h, &e2e, PASS, marked(), restating).await;
        assert_eq!(later.decision, CheckDecision::Final);
        assert_eq!(
            later.reading.verdict,
            Verdict::Fail,
            "a later call changes nothing"
        );
    }
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
    let again = restate(&h, &e2e, PASS, disputed()).await;
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
    let ask = kept(&h, &e2e).await.expect("the ask is kept");
    assert_eq!((ask.asked, ask.restated), (VerdictNote::DisputedPass, None));

    restarted(&mut h).await;
    let again = restate(&h, &e2e, PASS, disputed()).await;
    assert_eq!(again.decision, CheckDecision::Final, "never asked twice");
    assert_eq!(again.reading.note, Some(VerdictNote::DisputedPass));

    restarted(&mut h).await;
    let third = restate(&h, &e2e, PASS, marked()).await;
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

/// The reply to the first submission is lost with archond after the ask was
/// stored: the agent repeats what it sent, which is no restatement.
#[tokio::test]
async fn an_ask_whose_answer_was_lost_is_given_again_and_spends_nothing() {
    let mut h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let ask = kept(&h, &e2e).await.expect("the ask is kept");

    restarted(&mut h).await;
    let repeated = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(repeated.decision, CheckDecision::Restate, "asked again");
    assert_eq!(repeated.reading.note, Some(VerdictNote::DisputedPass));
    assert_eq!(
        kept(&h, &e2e).await,
        Some(ask),
        "the repeat restated nothing"
    );

    let restated = restate(&h, &e2e, PASS, marked()).await;
    assert_eq!(restated.decision, CheckDecision::Final);
    assert_eq!(restated.reading.verdict, Verdict::Pass);
    settle(&h, PASS, marked()).await;
    assert_eq!(stored(&h, &item, &e2e).await.verdict, Some(Verdict::Pass));
    only_running(&h, "report").await;
    assert_eq!(setup_runs(&h, &item).await, 1);
}

#[tokio::test]
async fn a_readable_submission_is_accepted_and_binds_nothing() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    for restating in [false, true] {
        let fail = json!({"verdict": "fail", "findings": []});
        let accepted = sent(&h, &e2e, FAIL, fail, restating).await;
        assert_eq!(accepted.decision, CheckDecision::Accepted);
        assert!(
            !accepted.message().contains("complete_task"),
            "{accepted:?}"
        );
        assert_eq!(kept(&h, &e2e).await, None, "no ask, nothing kept");
    }
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

/// The verdict that settles and the findings that route it are one
/// submission's: the final restatement's, not the result's.
#[tokio::test]
async fn a_restated_verdict_is_routed_by_the_findings_restated_with_it() {
    let mut h = Harness::new().await;
    let (product, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    restate(&h, &e2e, FAIL, failing("product")).await;
    let noted = json!({"verdict": "pass", "findings": [
        {"severity": "low", "blocking": false, "fault": "environment", "title": "slow start"}
    ]});
    settle(&h, PASS, noted).await;
    let error = failure(&h, &product).await;
    assert!(error.contains("`setup` was not re-run"), "{error}");
    // The failure tells what the restated fail said, ahead of the later pass.
    let (said, finished) = (error.find("the run stopped"), error.find("slow start"));
    assert!(said.is_some() && said < finished, "{error}");
    assert_eq!(setup_runs(&h, &product).await, 1);

    let (environment, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    restate(&h, &e2e, FAIL, failing("environment")).await;
    settle(&h, PASS, json!({"verdict": "pass", "findings": []})).await;
    let settled = stored(&h, &environment, &e2e).await;
    assert_eq!(settled.verdict, Some(Verdict::Fail));
    assert_eq!(settled.restatement.map(|r| r.environment), Some(true));
    let again = only_running(&h, "setup").await;
    assert_eq!(
        again.attempt, 2,
        "the restated environment fail re-runs setup"
    );
    // Setup is told the restated finding, which the result it settled with lost.
    let told = h.backend.resumed_with(&again);
    let (said, finished) = (told.find(FAIL), told.find(PASS));
    assert!(said.is_some() && said < finished, "{told}");
    assert!(told.contains("the run stopped"), "{told}");

    restarted(&mut h).await;
    h.dispatch().await;
    assert_eq!(
        only_running(&h, "setup").await.id,
        again.id,
        "replayed alike"
    );
    assert_eq!(setup_runs(&h, &environment).await, 2);
    assert!(matches!(
        h.state(&product.id).await,
        QueueState::Failed { .. }
    ));

    // The next attempt of `e2e` is asked on its own account.
    h.settle_running(|_| completed("start.sh ready")).await;
    h.dispatch().await;
    let second = only_running(&h, "e2e").await;
    assert_eq!(second.attempt, 2);
    let asked = submit(&h, &second, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
}
