//! Stage agents the pipeline moved past are archived in Aether, with what
//! they delegated: a finished run's failed attempts, an attempt a fresh
//! agent replaced, and an escalation once its item moves on. An unresolved
//! escalation or blocker stays in view under the one notice its run posts.

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ports::DescendantState;
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use archon_core::steer::Steer;
use common::{Harness, MODEL, PIPELINE};

fn failed(error: &str) -> StageOutcome {
    StageOutcome::Failed {
        error: error.into(),
        infra: false,
    }
}

fn engine_died() -> StageOutcome {
    StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    }
}

fn lead_agent(stage: &StageRun) -> AetherAgentId {
    stage
        .aether_agent
        .clone()
        .expect("a running stage has an agent")
}

fn retry(item: &QueueItem) -> Request {
    Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    }
}

/// The implementer, which delegated a worker that ended, fails its run.
#[tokio::test]
async fn a_finished_run_archives_its_failed_stage_agents_and_their_delegates() {
    let h = Harness::new().await;
    let item = h.enqueue("failing build").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    assert!(h.backend.archived().is_empty(), "nothing moved past yet");

    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_eq!(h.backend.archived(), [worker, agent.clone()]);
    let stage = h.stages(&item.id).await.remove(0);
    assert_eq!(h.kv(&format!("archive:{}", stage.id)).await, None);

    h.dispatch().await;
    assert_eq!(h.backend.archived().len(), 2, "archived once");
}

/// A woken agent is never archived while its slot can still use it; the
/// cold retry's fresh agent moves past it, so it is archived then.
#[tokio::test]
async fn a_retried_attempt_archives_the_agent_it_replaced() {
    let h = Harness::new().await;
    let item = h.enqueue("flaky engine").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    assert_eq!(h.backend.woken(), std::slice::from_ref(&agent));
    assert!(h.backend.archived().is_empty(), "the woken agent works on");

    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    let cold = lead_agent(&h.running().await[0]);
    assert_ne!(cold, agent, "the cold retry is a fresh agent");
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));

    h.settle_running(|_| completed_implement()).await;
    h.dispatch().await;
    assert_eq!(
        h.backend.archived(),
        std::slice::from_ref(&agent),
        "a completed attempt stays"
    );
}

fn completed_implement() -> StageOutcome {
    common::completed("implemented")
}

/// Fail the only stage of a new item's run with `error`, which ends the run,
/// and check its agent is kept, with the one notice of `kind` the finished
/// run posts; returns the item and the kept agent.
async fn kept_failure(
    h: &Harness,
    error: &str,
    kind: NotificationKind,
) -> (QueueItem, AetherAgentId) {
    let item = h.enqueue("needs the operator").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.settle_running(|_| failed(error)).await;
    h.dispatch().await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(h.backend.archived().is_empty(), "kept for the operator");
    let notices = h.notices(kind).await;
    assert_eq!(notices.len(), 1, "one notice: {notices:?}");
    let other = match kind {
        NotificationKind::Blocker => NotificationKind::Outcome,
        _ => NotificationKind::Blocker,
    };
    assert!(h.notices(other).await.is_empty(), "and no other");
    (item, agent)
}

/// A `BLOCKED:` failure waits under its one blocker; retrying the item
/// moves past it, and its agent is archived.
#[tokio::test]
async fn an_unresolved_blocker_stays_in_view_until_its_item_is_retried() {
    let h = Harness::new().await;
    let blocked = "BLOCKED: the deploy key is missing";
    let (item, agent) = kept_failure(&h, blocked, NotificationKind::Blocker).await;
    assert!(h.notices(NotificationKind::Blocker).await[0].contains("deploy key"));

    h.cli(retry(&item)).await.expect("retry");
    h.dispatch().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert_eq!(
        h.notices(NotificationKind::Blocker).await.len(),
        1,
        "no second notice"
    );
}

/// An `ESCALATE:` failure hands the item back under the run's one notice;
/// a steer that revives the item moves past it, and its agent is archived.
#[tokio::test]
async fn an_unresolved_escalation_stays_in_view_until_its_item_is_steered() {
    let h = Harness::new().await;
    let escalated = "ESCALATE: this needs a schema migration first";
    let (item, agent) = kept_failure(&h, escalated, NotificationKind::Outcome).await;

    let switch = Steer {
        switch_pipeline: Some(PIPELINE.into()),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: switch,
    };
    h.orchestrator(steer).await.expect("revived");
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
}

/// A routine failure gets no blocker, and its agent is archived at once.
#[tokio::test]
async fn a_routine_failure_is_archived_without_a_blocker() {
    let h = Harness::new().await;
    let item = h.enqueue("routine failure").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    assert_eq!(h.backend.archived(), [agent]);
}

/// An archive Aether did not answer is owed, and the sweep makes it; the
/// run is not held up by it.
#[tokio::test]
async fn an_unanswered_archive_is_retried_by_the_sweep() {
    let h = Harness::new().await;
    let item = h.enqueue("aether away").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Unavailable("aether away".into()));
    h.settle_running(|_| failed("tests failed")).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(
        h.backend.archived().is_empty(),
        "the archive went unanswered"
    );
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    h.dispatch().await;
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None, "done and forgotten");
}

/// A corrupt archive mark is dropped by the sweep, which still archives
/// the valid owed agent and does the rest of its work.
#[tokio::test]
async fn a_corrupt_archive_mark_does_not_stop_the_sweep() {
    let h = Harness::new().await;
    let item = h.enqueue("aether away").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Unavailable("aether away".into()));
    h.settle_running(|_| failed("tests failed")).await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    let bad_key = "archive:not-a-stage-run";
    let bad_value = format!("archive:{}", archon_core::id::StageRunId::new());
    {
        let mut store = h.ctx.store.lock().await;
        store.kv_set(bad_key, "owed").unwrap();
        store.kv_set(&bad_value, "garbled").unwrap();
    }
    let next = h.enqueue("after the corrupt marks").await;

    h.dispatch().await;
    assert_eq!(h.kv(bad_key).await, None, "corrupt key dropped");
    assert_eq!(h.kv(&bad_value).await, None, "corrupt value dropped");
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None, "valid mark done and forgotten");
    assert!(matches!(
        h.state(&next.id).await,
        QueueState::Running { .. }
    ));
}
