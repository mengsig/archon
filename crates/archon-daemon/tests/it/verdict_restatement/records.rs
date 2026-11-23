use archon_core::error::ArchonError;
use archon_core::protocol::Reply;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{RunState, StageOutcome, StageRun};
use serde_json::{Value, json};

use super::{call, failure, forwarded_reply, only_running, settle};
use crate::common::delivery::{change, pass_by, reviewed_by, shipped_by, verdict_on};
use crate::common::{Harness, REVIEW_MODEL};
use crate::executor::adaptive_item;

const SUMMARY: &str = "Built and reviewed.";

async fn precheck(h: &Harness, lead: &StageRun, record: Value) -> Result<Reply, ArchonError> {
    let agent = lead
        .aether_agent
        .as_ref()
        .expect("a running lead has an agent");
    let request = call(Some(agent), SUMMARY, record, false);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        forwarded_reply(h, agent, request),
    )
    .await
    .expect("precheck returns without waiting on a daemon lock")
}

fn record(outcome: StageOutcome) -> Value {
    let StageOutcome::Completed {
        structured: Some(record),
        ..
    } = outcome
    else {
        panic!("expected a completion with a record, got {outcome:?}");
    };
    assert!(record.is_object());
    record
}

fn refusal(reply: Result<Reply, ArchonError>) -> String {
    match reply {
        Err(ArchonError::Invalid(why)) => why,
        other => panic!("expected a record refusal, got {other:?}"),
    }
}

async fn assert_settlement_refuses(h: &Harness, item: &QueueItem, proposed: Value, why: &str) {
    settle(h, SUMMARY, proposed).await;
    let RunState::Failed { stage_id, error } = h.run(&item.id).await.state else {
        panic!("the run should fail with {why}");
    };
    assert_eq!(stage_id, "build");
    assert_eq!(
        archon_pipeline::record_refusal(&error),
        Some(why),
        "{error}"
    );
}

#[tokio::test]
async fn a_delta_only_review_has_the_settlement_refusal_and_can_be_corrected_in_stage() {
    for correct in [false, true] {
        let h = Harness::new().await;
        let item = adaptive_item(&h).await;
        let lead = only_running(&h, "build").await;
        let earlier = record(shipped_by(&h, &lead, SUMMARY, json!({})).await);
        let reviewer = h
            .backend
            .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
        let built = shipped_by(&h, &lead, SUMMARY, pass_by(std::slice::from_ref(&reviewer))).await;
        let mut delta = verdict_on(&built, "pass");
        delta["base"] = earlier["commit"].clone();
        h.backend.complete_delegated(&reviewer, delta);
        let proposed = record(built);
        let why = refusal(precheck(&h, &lead, proposed.clone()).await);
        assert!(why.contains("read only from"), "{why}");
        assert_eq!(only_running(&h, "build").await.id, lead.id);
        assert_eq!(
            h.ctx.store.lock().await.run_review(&lead.run).unwrap(),
            None
        );

        if correct {
            let whole = h
                .backend
                .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
            h.backend.complete_delegated(
                &whole,
                json!({"verdict": "pass", "head": proposed["commit"]}),
            );
            let mut proposed = proposed;
            proposed["review"] = pass_by(&[whole])["review"].clone();
            assert!(matches!(
                precheck(&h, &lead, proposed.clone()).await,
                Ok(Reply::Ok)
            ));
            assert_eq!(
                h.ctx.store.lock().await.run_review(&lead.run).unwrap(),
                None
            );
            settle(&h, SUMMARY, proposed).await;
            assert!(matches!(
                h.state(&item.id).await,
                QueueState::Succeeded { .. }
            ));
        } else {
            assert_settlement_refuses(&h, &item, proposed, &why).await;
        }
    }
}

#[tokio::test]
async fn head_moving_after_precheck_is_refused_at_settlement() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = only_running(&h, "build").await;
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let proposed = record(reviewed_by(&h, &lead, SUMMARY, &[reviewer]).await);
    assert!(matches!(
        precheck(&h, &lead, proposed.clone()).await,
        Ok(Reply::Ok)
    ));
    let moved = change(&h.run(&item.id).await.workspace);
    assert_ne!(moved["commit"], proposed["commit"]);
    assert_ne!(moved["tree"], proposed["tree"]);

    settle(&h, SUMMARY, proposed).await;
    let error = failure(&h, &item).await;
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    assert!(error.contains("ship record"), "{error}");
}

#[tokio::test]
async fn an_empty_pr_body_has_the_settlement_refusal_in_stage() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = only_running(&h, "build").await;
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let mut proposed = record(reviewed_by(&h, &lead, SUMMARY, &[reviewer]).await);
    proposed["pr"]["body"] = " \n ".into();
    let why = refusal(precheck(&h, &lead, proposed.clone()).await);
    assert!(why.contains("`pr.body` is empty"), "{why}");
    assert_eq!(only_running(&h, "build").await.id, lead.id);

    assert_settlement_refuses(&h, &item, proposed, &why).await;
}
