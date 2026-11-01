//! A stage at its deadline: Aether's stop wakes the same agent once with a
//! short extension to commit and report, and the sweep warns it beforehand.

use crate::common;

use archon_aether::AetherEvent;
use archon_aether::events::EventKind;
use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_daemon::{dispatcher, executor, pump};
use common::Harness;

/// Aether's `agent.failed` for `stage`'s agent, stamped `at`.
fn agent_failed(stage: &StageRun, at: Timestamp) -> AetherEvent {
    AetherEvent {
        seq: 1,
        at,
        agent: stage.aether_agent.clone(),
        task: None,
        parent: None,
        run: None,
        kind: EventKind::AgentFailed {
            error: format!("deadline of {}", at.to_rfc3339()),
        },
    }
}

/// Aether stops `stage`'s agent at its deadline, holding it as failed so a
/// wake revives it when `wakeable`.
async fn stop_at_deadline(h: &Harness, stage: &StageRun, wakeable: bool) {
    let deadline = archon_core::now();
    assert!(
        deadline > stage.started_at,
        "the deadline follows the start"
    );
    common::set_deadline(h, stage, deadline).await;
    let agent = stage.aether_agent.as_ref().expect("an agent");
    if wakeable {
        h.backend.fail_agent(agent);
    }
    pump::handle_fleet_event(&h.ctx, &agent_failed(stage, deadline))
        .await
        .expect("handled");
    h.dispatch().await;
}

/// Run one item up to its first stage's agent stopped at its deadline and woken.
async fn woken_at_deadline(h: &Harness) -> (StageRun, StageRun) {
    h.enqueue("ran out of time").await;
    h.dispatch().await;
    let first = h.running().await.remove(0);
    stop_at_deadline(h, &first, true).await;
    let woken = h.running().await.remove(0);
    (first, woken)
}

#[tokio::test]
async fn an_agent_stopped_at_its_deadline_is_woken_once_to_commit_and_report() {
    let h = Harness::new().await;
    let (first, woken) = woken_at_deadline(&h).await;
    let agent = first.aether_agent.clone().expect("an agent");

    assert_eq!(h.backend.woken(), std::slice::from_ref(&agent));
    assert_eq!(h.backend.requests().len(), 1, "no fresh agent");
    assert_ne!(woken.id, first.id);
    assert_eq!(
        (woken.attempt, woken.aether_agent.as_ref()),
        (1, Some(&agent))
    );
    let stages = h.ctx.store.lock().await.stage_runs(&first.run).unwrap();
    let timed_out = stages.iter().find(|s| s.id == first.id).unwrap();
    assert_eq!(timed_out.state, StageState::TimedOut);
    let told = h.backend.resumed_with(&woken);
    assert!(told.contains("commit what works now"), "{told}");
    assert!(told.contains("Archon now stops you"), "{told}");
    let until = common::stored_deadline(&h, &woken)
        .await
        .expect("a deadline");
    let extension = until - woken.started_at;
    assert_eq!(
        extension,
        chrono::Duration::minutes(10),
        "a short extension"
    );
    let notices = h.notices(NotificationKind::Progress).await;
    assert!(
        notices.iter().any(|n| n.contains("hit its deadline")),
        "{notices:?}"
    );

    let done = StageOutcome::Completed {
        summary: "committed what works".into(),
        structured: None,
    };
    h.backend.answer(&agent, done);
    executor::turn_ended(&h.ctx, &agent)
        .await
        .expect("turn end");
    h.dispatch().await;
    let settled = h.ctx.store.lock().await.stage_run(&woken.id).unwrap();
    assert_eq!(settled.state, StageState::Completed);
}

/// The woken agent stopped at its own short deadline times out for good.
#[tokio::test]
async fn a_woken_agent_stopped_at_its_deadline_again_times_out() {
    let h = Harness::new().await;
    let (_, woken) = woken_at_deadline(&h).await;
    stop_at_deadline(&h, &woken, true).await;

    let settled = h.ctx.store.lock().await.stage_run(&woken.id).unwrap();
    assert_eq!(settled.state, StageState::TimedOut);
    assert!(h.running().await.is_empty(), "no third run");
    assert_eq!(h.backend.woken().len(), 1, "woken once");
    assert_eq!(h.backend.requests().len(), 1, "no fresh agent");
    let item = h.ctx.store.lock().await.run(&woken.run).unwrap().item;
    assert!(matches!(h.state(&item).await, QueueState::Failed { .. }));
}

/// Any failure of the woken run is final: no infrastructure retry gives it
/// a whole stage's time again.
#[tokio::test]
async fn a_woken_agent_that_fails_is_not_retried() {
    let h = Harness::new().await;
    let (_, woken) = woken_at_deadline(&h).await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;

    let settled = h.ctx.store.lock().await.stage_run(&woken.id).unwrap();
    assert!(matches!(settled.state, StageState::Failed { .. }));
    assert!(h.running().await.is_empty(), "no retry");
    assert_eq!(h.backend.requests().len(), 1, "no fresh agent");
}

/// An agent Aether cannot wake leaves its stage timed out: a fresh agent
/// with a few minutes could not finish the work either.
#[tokio::test]
async fn a_refused_deadline_wake_times_out_without_a_fresh_agent() {
    let h = Harness::new().await;
    let item = h.enqueue("ran out of time, gone").await;
    h.dispatch().await;
    let first = h.running().await.remove(0);
    stop_at_deadline(&h, &first, false).await;

    assert!(h.backend.woken().is_empty());
    assert_eq!(h.backend.requests().len(), 1, "no fresh agent");
    assert!(h.running().await.is_empty());
    let stages = h.stages(&item.id).await;
    assert!(
        stages.iter().all(|s| s.state == StageState::TimedOut),
        "{stages:?}"
    );
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
}

async fn sweep(h: &Harness) {
    dispatcher::dispatch_once(&h.ctx)
        .await
        .expect("dispatch pass");
}

fn warnings(h: &Harness, stage: &StageRun) -> Vec<String> {
    let key = format!("{}:deadline-warning", stage.id);
    let calls = h.backend.message_calls();
    calls.into_iter().filter(|k| *k == key).collect()
}

/// The sweep warns a stage's agent once, with a fifth of its time left.
#[tokio::test]
async fn a_running_stage_is_warned_once_as_its_deadline_nears() {
    let h = Harness::new().await;
    h.enqueue("long work").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let agent = stage.aether_agent.clone().expect("an agent");
    common::set_deadline(&h, &stage, archon_core::now() + chrono::Duration::days(1)).await;
    sweep(&h).await;
    assert!(warnings(&h, &stage).is_empty(), "not yet");

    common::set_deadline(
        &h,
        &stage,
        archon_core::now() + chrono::Duration::minutes(1),
    )
    .await;
    sweep(&h).await;
    sweep(&h).await;
    assert_eq!(warnings(&h, &stage).len(), 1, "warned once");
    let messages = h.backend.messages();
    let (to, text) = messages
        .iter()
        .find(|(_, text)| text.contains("stage deadline"))
        .expect("a warning");
    assert_eq!(to, &agent);
    assert!(text.contains("commit what works"), "{text}");
}

/// A warning Aether did not answer is sent again by the next sweep.
#[tokio::test]
async fn an_unanswered_deadline_warning_is_sent_again() {
    let h = Harness::new().await;
    h.enqueue("long work, flaky link").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    common::set_deadline(
        &h,
        &stage,
        archon_core::now() + chrono::Duration::minutes(1),
    )
    .await;
    h.backend
        .fail_next_message(ArchonError::Unavailable("aether: link down".into()));
    sweep(&h).await;
    sweep(&h).await;
    sweep(&h).await;

    assert_eq!(warnings(&h, &stage).len(), 2, "tried, then sent once");
    let delivered = h.backend.messages();
    let warned = delivered
        .iter()
        .filter(|(_, t)| t.contains("stage deadline"));
    assert_eq!(warned.count(), 1);
}
