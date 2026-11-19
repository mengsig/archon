//! The one restatement belongs to the stage attempt, not to one stage run:
//! a replacement of the attempt's stage run (its agent woken at a deadline,
//! a fresh agent after an infrastructure failure) inherits the ask and the
//! final answer, and every stage run that ends keeps the record.

use archon_aether::AetherEvent;
use archon_aether::events::EventKind;
use archon_core::protocol::Request;
use archon_core::queue::QueueState;
use archon_core::run::{CheckDecision, StageOutcome, StageRun, StageState, Verdict, VerdictNote};
use archon_core::steer::{Steer, Then};
use archon_daemon::{executor, pump};

use super::{
    FAIL, PASS, at_e2e, disputed, failing, failure, kept, key, marked, only_running, restate,
    result, setup_runs, stored, submit,
};
use crate::common::{self, Harness};

/// Aether stops `stage`'s agent at its deadline and Archon wakes it once;
/// returns the stage run the woken agent now runs.
async fn woken_at_deadline(h: &Harness, stage: &StageRun) -> StageRun {
    let deadline = archon_core::now();
    assert!(
        deadline > stage.started_at,
        "the deadline follows the start"
    );
    common::set_deadline(h, stage, deadline).await;
    let agent = stage.aether_agent.clone().expect("an agent");
    h.backend.fail_agent(&agent);
    let stopped = AetherEvent {
        seq: 1,
        at: deadline,
        agent: Some(agent.clone()),
        task: None,
        parent: None,
        run: None,
        kind: EventKind::AgentFailed {
            error: format!("deadline of {}", deadline.to_rfc3339()),
        },
    };
    pump::handle_fleet_event(&h.ctx, &stopped)
        .await
        .expect("handled");
    h.dispatch().await;
    let woken = only_running(h, &stage.stage_id).await;
    assert_ne!(woken.id, stage.id, "a new stage run");
    assert_eq!(
        (woken.attempt, woken.aether_agent.as_ref()),
        (stage.attempt, Some(&agent)),
        "the same attempt and agent"
    );
    woken
}

/// The woken agent ends its turn with `outcome`, as it reports after a wake.
async fn finish_woken(h: &Harness, woken: &StageRun, outcome: StageOutcome) {
    let agent = woken.aether_agent.as_ref().expect("an agent");
    h.backend.answer(agent, outcome);
    executor::turn_ended(&h.ctx, agent).await.expect("turn end");
    h.dispatch().await;
}

/// A fresh agent replaces `stage`'s after an infrastructure failure.
async fn replaced_cold(h: &Harness, stage: &StageRun) -> StageRun {
    let lost = |_: &StageRun| StageOutcome::Failed {
        error: "the engine died".into(),
        infra: true,
    };
    assert_eq!(h.settle_running(lost).await.len(), 1);
    h.dispatch().await;
    let fresh = only_running(h, &stage.stage_id).await;
    assert_eq!(fresh.attempt, stage.attempt, "the same attempt");
    assert_ne!(fresh.aether_agent, stage.aether_agent, "a fresh agent");
    fresh
}

#[tokio::test]
async fn a_pending_ask_follows_its_agent_through_a_deadline_wake() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let ask = kept(&h, &e2e).await.expect("the ask is kept");
    let woken = woken_at_deadline(&h, &e2e).await;
    assert_eq!(key(&woken), key(&e2e), "one attempt, one key");
    assert_eq!(
        kept(&h, &woken).await,
        Some(ask),
        "the ask outlives the stop"
    );
    let timed_out = stored(&h, &item, &e2e).await;
    assert_eq!(timed_out.state, StageState::TimedOut);
    assert_eq!(timed_out.restatement, Some(ask), "on the stopped run too");

    let again = submit(&h, &woken, PASS, disputed()).await;
    assert_eq!(
        again.decision,
        CheckDecision::Restate,
        "a repeat is no restatement"
    );
    let restated = restate(&h, &woken, PASS, disputed()).await;
    assert_eq!(
        restated.decision,
        CheckDecision::Final,
        "the one ask was spent"
    );
    assert!(
        !restated.message().contains("complete_task"),
        "{restated:?}"
    );
    assert!(
        restated
            .message()
            .contains("Finish now as your instructions say")
    );

    finish_woken(&h, &woken, result(PASS, marked())).await;
    let settled = stored(&h, &item, &woken).await;
    assert_eq!(
        (settled.verdict, settled.verdict_note),
        (Some(Verdict::Unknown), Some(VerdictNote::DisputedPass)),
        "the woken run is bound by its attempt's restatement"
    );
    assert!(failure(&h, &item).await.contains("`setup` was not re-run"));
    assert_eq!(setup_runs(&h, &item).await, 1);
    assert_eq!(kept(&h, &woken).await, None);
}

#[tokio::test]
async fn a_restated_fail_binds_the_woken_agent_s_later_pass() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    restate(&h, &e2e, FAIL, failing("product")).await;
    let woken = woken_at_deadline(&h, &e2e).await;

    let later = submit(&h, &woken, PASS, marked()).await;
    assert_eq!(later.decision, CheckDecision::Final);
    assert_eq!(later.reading.verdict, Verdict::Fail, "the fail stands");
    assert!(!later.message().contains("complete_task"), "{later:?}");
    finish_woken(&h, &woken, result(PASS, marked())).await;

    let settled = stored(&h, &item, &woken).await;
    assert_eq!(settled.verdict, Some(Verdict::Fail));
    let error = failure(&h, &item).await;
    assert!(error.contains("stage `e2e` failed"), "{error}");
    assert_eq!(setup_runs(&h, &item).await, 1);
}

/// A woken agent's task already ended, so no answer names `complete_task`.
#[tokio::test]
async fn a_woken_agent_s_readable_verdict_is_accepted_and_settles() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    let woken = woken_at_deadline(&h, &e2e).await;
    let accepted = submit(&h, &woken, PASS, marked()).await;
    assert_eq!(accepted.decision, CheckDecision::Accepted);
    assert!(
        !accepted.message().contains("complete_task"),
        "{accepted:?}"
    );
    assert!(
        accepted
            .message()
            .contains("Finish now as your instructions say")
    );
    finish_woken(&h, &woken, result(PASS, marked())).await;
    assert_eq!(stored(&h, &item, &woken).await.verdict, Some(Verdict::Pass));
    only_running(&h, "report").await;
}

#[tokio::test]
async fn a_fresh_agent_replacing_the_attempt_s_inherits_its_restatement() {
    let h = Harness::new().await;
    let (pending, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    let fresh = replaced_cold(&h, &e2e).await;
    let failed = stored(&h, &pending, &e2e).await;
    assert!(matches!(failed.state, StageState::Failed { .. }));
    assert_eq!(failed.restatement.map(|r| r.restated), Some(None));
    let restated = restate(&h, &fresh, PASS, disputed()).await;
    assert_eq!(restated.decision, CheckDecision::Final, "no second ask");
    assert_eq!(h.settle_running(|_| result(PASS, marked())).await.len(), 1);
    h.dispatch().await;
    assert_eq!(
        stored(&h, &pending, &fresh).await.verdict,
        Some(Verdict::Unknown)
    );
    assert_eq!(setup_runs(&h, &pending).await, 1);

    let (bound, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    restate(&h, &e2e, FAIL, failing("product")).await;
    let fresh = replaced_cold(&h, &e2e).await;
    let later = submit(&h, &fresh, PASS, marked()).await;
    assert_eq!(later.decision, CheckDecision::Final);
    assert_eq!(later.reading.verdict, Verdict::Fail);
    assert_eq!(h.settle_running(|_| result(PASS, marked())).await.len(), 1);
    h.dispatch().await;
    assert_eq!(
        stored(&h, &bound, &fresh).await.verdict,
        Some(Verdict::Fail)
    );
    assert_eq!(setup_runs(&h, &bound).await, 1);
    assert_eq!(kept(&h, &fresh).await, None);
}

/// An operator's abort ends the attempt: its stage run keeps the record, and
/// the retry is a new attempt that is asked on its own account.
#[tokio::test]
async fn an_aborted_attempt_keeps_its_restatement_on_record() {
    let h = Harness::new().await;
    let (item, e2e) = at_e2e(&h).await;
    submit(&h, &e2e, PASS, disputed()).await;
    restate(&h, &e2e, FAIL, failing("product")).await;
    let abort = Request::SteerItem {
        item: item.id.clone(),
        steer: Steer {
            abort: Some(Then::Retry),
            ..Steer::default()
        },
    };
    h.cli(abort).await.expect("aborted");
    h.dispatch().await;

    let aborted = stored(&h, &item, &e2e).await;
    assert_eq!(aborted.state, StageState::Cancelled);
    let record = aborted.restatement.expect("the record survives the abort");
    assert_eq!(record.asked, VerdictNote::DisputedPass);
    assert_eq!(record.restated.map(|r| r.verdict), Some(Verdict::Fail));
    assert_eq!(
        kept(&h, &e2e).await,
        None,
        "the ended attempt's is forgotten"
    );

    let retry = only_running(&h, "e2e").await;
    assert_eq!(retry.attempt, e2e.attempt + 1, "a new attempt");
    let asked = submit(&h, &retry, PASS, disputed()).await;
    assert_eq!(asked.decision, CheckDecision::Restate, "its own one ask");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}
