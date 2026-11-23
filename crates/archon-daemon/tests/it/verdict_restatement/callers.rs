//! Who may call `submit_verdict` and with what: the running stage's own
//! agent, named by Aether, for a stage that holds a verdict, sending a
//! result `complete_task` would take. A refused call spends nothing.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::ClientKind;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{CheckDecision, StageRun, Verdict, VerdictCheck, VerdictNote};
use archon_daemon::handlers;
use serde_json::{Value, json};

use super::{
    E2E, PASS, at_e2e, call, disputed, e2e_plan, forwarded, kept, marked, only_running, restate,
    settle, setup_runs, stored, submit,
};
use crate::common::delivery::{pass_by, reviewed_by};
use crate::common::{Harness, MODEL, REVIEW_MODEL, choice, completed};

fn refused(answer: Result<VerdictCheck, ArchonError>, why: &str) {
    match answer {
        Err(ArchonError::Forbidden(text) | ArchonError::Invalid(text)) => {
            assert!(text.contains(why), "{text}");
        }
        other => panic!("expected a refusal naming {why:?}, got {other:?}"),
    }
}

async fn submit_as(h: &Harness, agent: &AetherAgentId) -> Result<VerdictCheck, ArchonError> {
    forwarded(h, agent, call(Some(agent), PASS, disputed(), false)).await
}

#[tokio::test]
async fn only_a_running_verdict_stage_s_own_agent_may_submit() {
    let h = Harness::new().await;
    let item = h.enqueue_in(E2E, "journeys", e2e_plan()).await.unwrap();
    h.dispatch().await;
    let setup = only_running(&h, "setup").await;
    let setup_agent = setup.aether_agent.clone().expect("an agent");
    refused(
        submit_as(&h, &setup_agent).await,
        "stage `setup` has no verdict or required records",
    );
    let stranger = AetherAgentId::parse("agt_stranger").unwrap();
    refused(submit_as(&h, &stranger).await, "has none on record");
    let unnamed = call(None, PASS, disputed(), false);
    let unnamed = handlers::handle(&h.ctx, ClientKind::ArchonMcp, unnamed).await;
    assert!(
        matches!(&unnamed, Err(ArchonError::Forbidden(text)) if text.contains("caller metadata")),
        "{unnamed:?}"
    );

    h.settle_running(|_| completed("start.sh ready")).await;
    h.dispatch().await;
    let e2e = only_running(&h, "e2e").await;
    let agent = e2e.aether_agent.clone().expect("an agent");
    let for_another = call(Some(&stranger), PASS, disputed(), false);
    refused(
        forwarded(&h, &agent, for_another).await,
        "but was forwarded for",
    );
    assert_eq!(kept(&h, &e2e).await, None, "a refused call asks nothing");

    settle(&h, PASS, marked()).await;
    refused(submit_as(&h, &agent).await, "while its stage runs");
    assert_eq!(kept(&h, &e2e).await, None, "a refused call keeps nothing");
    assert_eq!(setup_runs(&h, &item).await, 1);
}

/// `complete_task` takes an object, so no other `structured` is read as the
/// result the stage will finish with, and none spends the one ask.
#[tokio::test]
async fn a_structured_value_that_is_no_object_is_refused_before_anything_is_asked() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let agent = e2e.aether_agent.clone().expect("an agent");
    for (restating, odd) in [(false, json!("pass")), (true, json!(["pass"]))] {
        let request = call(Some(&agent), "I looked around.", odd, restating);
        refused(
            forwarded(&h, &agent, request).await,
            "`structured` must be an object",
        );
        assert_eq!(kept(&h, &e2e).await, None, "nothing was asked");
    }

    let asked = submit(&h, &e2e, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate, "the ask is unspent");
    let ask = kept(&h, &e2e).await;
    let odd = call(Some(&agent), PASS, json!(1), true);
    refused(
        forwarded(&h, &agent, odd).await,
        "`structured` must be an object",
    );
    assert_eq!(
        kept(&h, &e2e).await,
        ask,
        "no refused call is the restatement"
    );
    let restated = restate(&h, &e2e, PASS, marked()).await;
    assert_eq!(restated.reading.verdict, Verdict::Pass);
    settle(&h, PASS, marked()).await;
    assert_eq!(stored(&h, &item, &e2e).await.verdict, Some(Verdict::Pass));
}

const REVIEW_ONLY: &str = r#"[pipeline]
name = "review-only"
description = "review a change, ship nothing"
version = 1
workspace = "worktree"
requires_repo = true
[pipeline.review]
level = "standard"
stage = "review"
[[stage]]
id = "review"
kind = "work"
prompt = "implement.md"
allow_delegation = true
"#;

/// A review-only run whose lead runs.
async fn at_review_lead(h: &Harness, title: &str) -> (QueueItem, StageRun) {
    let plan = ModelPlan([("review".into(), vec![choice(MODEL, Effort::Low)])].into());
    let item = h.enqueue_in("review-only", title, plan).await.unwrap();
    h.dispatch().await;
    let lead = only_running(h, "review").await;
    assert_eq!(lead.only_reviews, Some(true), "a review-only lead");
    (item, lead)
}

/// A review record's verdict, the only one a silent lead's result gives.
fn recorded(verdict: &str) -> Value {
    json!({"review": {"verdict": verdict, "reviewers": ["agt_reviewer"]}})
}

/// The tool reads a review-only lead's result as settlement does: its review
/// record's verdict counts when the result states none of its own.
#[tokio::test]
async fn a_review_only_lead_s_recorded_verdict_is_read_as_settlement_reads_it() {
    let h = Harness::new().await;
    h.add_pipeline("review-only", REVIEW_ONLY);
    let (passing, lead) = at_review_lead(&h, "passes on its record").await;
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    reviewed_by(&h, &lead, "Reviewed.", std::slice::from_ref(&reviewer)).await;
    let record = pass_by(&[reviewer]);
    let accepted = submit(&h, &lead, "Reviewed.", record.clone()).await;
    assert_eq!(accepted.decision, CheckDecision::Accepted);
    assert_eq!(accepted.reading.verdict, Verdict::Pass);
    settle(&h, "Reviewed.", record).await;
    let settled = stored(&h, &passing, &lead).await;
    assert_eq!(
        (settled.verdict, settled.restatement),
        (Some(Verdict::Pass), None),
        "settlement read what the tool read"
    );

    let (failing, lead) = at_review_lead(&h, "restates a fail").await;
    let asked = submit(&h, &lead, "Reviewed.", json!({})).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
    assert_eq!(asked.reading.note, Some(VerdictNote::MissingVerdictLine));
    let mut found = recorded("fail");
    found["findings"] = json!([{"severity": "high", "title": "the migration drops a column"}]);
    let restated = restate(&h, &lead, "Reviewed: it loses data.", found).await;
    assert_eq!(restated.decision, CheckDecision::Final);
    assert_eq!(restated.reading.verdict, Verdict::Fail);
    settle(&h, "Reviewed, all fine.", recorded("pass")).await;
    let settled = stored(&h, &failing, &lead).await;
    assert_eq!(
        settled.verdict,
        Some(Verdict::Fail),
        "the restated fail binds"
    );
    let QueueState::Failed { error, .. } = h.state(&failing.id).await else {
        panic!("the restated fail fails the run");
    };
    // The failure tells what the restated fail said, ahead of the later pass.
    let said = error.find("Reviewed: it loses data.");
    assert!(
        said.is_some() && said < error.find("Reviewed, all fine."),
        "{error}"
    );
    assert!(error.contains("the migration drops a column"), "{error}");
}

#[tokio::test]
async fn a_refused_review_record_does_not_consume_the_verdict_restatement() {
    let h = Harness::new().await;
    h.add_pipeline("review-only", REVIEW_ONLY);
    let (item, lead) = at_review_lead(&h, "restates a pass with a checked review").await;
    let agent = lead.aether_agent.as_ref().unwrap();
    let asked = submit(&h, &lead, "Reviewed.", json!({})).await;
    assert_eq!(asked.decision, CheckDecision::Restate);
    let before = kept(&h, &lead).await.unwrap();
    assert!(before.restated.is_none());

    let missing = call(Some(agent), "Reviewed.", recorded("pass"), true);
    refused(
        forwarded(&h, agent, missing).await,
        "the review this pipeline declares is not on record",
    );
    assert_eq!(kept(&h, &lead).await, Some(before));

    let reviewer = h.backend.delegate(agent, REVIEW_MODEL);
    reviewed_by(&h, &lead, "Reviewed.", std::slice::from_ref(&reviewer)).await;
    let record = pass_by(&[reviewer]);
    let checked = restate(&h, &lead, "Reviewed.", record.clone()).await;
    assert_eq!(checked.decision, CheckDecision::Final);
    assert_eq!(checked.reading.verdict, Verdict::Pass);
    settle(&h, "Reviewed.", record).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}
