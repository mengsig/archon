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

/// Aether's `agent.created` for `child`, delegated by `stage`'s lead.
fn child_created(stage: &StageRun, child: &archon_core::aether_ids::AetherAgentId) -> AetherEvent {
    AetherEvent {
        seq: 2,
        at: archon_core::now(),
        agent: Some(child.clone()),
        task: None,
        parent: stage.aether_agent.clone(),
        run: None,
        kind: EventKind::AgentCreated {
            model: common::REVIEW_MODEL.parse().expect("model"),
            name: "reviewer".into(),
        },
    }
}

/// Run one item to its first stage and extend its deadline, as a steer does:
/// the stage, the deadline it had and the one it has now.
async fn extended(h: &Harness) -> (StageRun, Timestamp, Timestamp) {
    h.enqueue("needs more time").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let old = common::stored_deadline(h, &stage)
        .await
        .expect("a deadline");
    let later = old + chrono::Duration::hours(2);
    common::set_deadline(h, &stage, later).await;
    (stage, old, later)
}

#[tokio::test]
async fn a_child_created_after_an_extension_gets_the_new_deadline() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let (stage, old, later) = extended(&h).await;
    let lead = stage.aether_agent.clone().expect("a lead");
    let child = h.backend.delegate(&lead, common::REVIEW_MODEL);
    // Aether copied the lead's row as it stood before the move landed.
    h.backend.limit_delegated(&child, old);
    pump::handle_fleet_event(&h.ctx, &child_created(&stage, &child))
        .await
        .expect("handled");
    let raised: Vec<_> = h
        .backend
        .updates()
        .into_iter()
        .filter(|(agent, _)| *agent == child)
        .map(|(_, update)| update.deadline)
        .collect();
    assert_eq!(raised, [Some(later)], "the child gets the stage's deadline");
}

/// A child that delegated before its creation was handled passed the old
/// deadline on; handling the child raises the grandchild too.
#[tokio::test]
async fn a_grandchild_delegated_before_its_parent_was_handled_gets_the_new_deadline() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let (stage, old, later) = extended(&h).await;
    let lead = stage.aether_agent.clone().expect("a lead");
    let child = h.backend.delegate(&lead, common::REVIEW_MODEL);
    h.backend.limit_delegated(&child, old);
    let grandchild = h.backend.delegate(&child, common::REVIEW_MODEL);
    h.backend.limit_delegated(&grandchild, old);
    pump::handle_fleet_event(&h.ctx, &child_created(&stage, &child))
        .await
        .expect("handled");
    let updates = h.backend.updates();
    for agent in [&child, &grandchild] {
        let raised: Vec<_> = (updates.iter())
            .filter(|(to, _)| to == agent)
            .map(|(_, update)| update.deadline)
            .collect();
        assert_eq!(raised, [Some(later)], "{agent} gets the stage's deadline");
    }
}

#[tokio::test]
async fn a_child_created_with_the_stage_deadline_or_none_is_left_alone() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let (stage, _, later) = extended(&h).await;
    let lead = stage.aether_agent.clone().expect("a lead");
    let current = h.backend.delegate(&lead, common::REVIEW_MODEL);
    h.backend.limit_delegated(&current, later);
    let unlimited = h.backend.delegate(&lead, common::REVIEW_MODEL);
    for child in [&current, &unlimited] {
        pump::handle_fleet_event(&h.ctx, &child_created(&stage, child))
            .await
            .expect("handled");
    }
    let touched = h
        .backend
        .updates()
        .into_iter()
        .filter(|(agent, _)| *agent == current || *agent == unlimited);
    assert_eq!(touched.count(), 0, "no child's deadline is changed");
}

/// Aether's `agent.created` for `agent`, delegated by `parent`.
fn delegated_by(
    parent: &archon_core::aether_ids::AetherAgentId,
    agent: &archon_core::aether_ids::AetherAgentId,
) -> AetherEvent {
    AetherEvent {
        seq: 3,
        at: archon_core::now(),
        agent: Some(agent.clone()),
        task: None,
        parent: Some(parent.clone()),
        run: None,
        kind: EventKind::AgentCreated {
            model: common::REVIEW_MODEL.parse().expect("model"),
            name: "worker".into(),
        },
    }
}

/// The deadlines sent to `agent`, in order.
fn deadlines_sent(h: &Harness, agent: &archon_core::aether_ids::AetherAgentId) -> Vec<Timestamp> {
    let updates = h.backend.updates().into_iter();
    updates
        .filter(|(to, _)| to == agent)
        .filter_map(|(_, update)| update.deadline)
        .collect()
}

/// A grandchild created from its parent's row before that parent was raised
/// is raised on its own event, whose parent is no stage lead.
#[tokio::test]
async fn a_nested_child_created_after_an_extension_gets_the_new_deadline() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let (stage, old, later) = extended(&h).await;
    let lead = stage.aether_agent.clone().expect("a lead");
    let child = h.backend.delegate(&lead, common::REVIEW_MODEL);
    h.backend.limit_delegated(&child, old);
    pump::handle_fleet_event(&h.ctx, &child_created(&stage, &child))
        .await
        .expect("handled");
    assert_eq!(deadlines_sent(&h, &child), [later]);
    // Delegated from the child's row as it stood before its raise landed.
    let grandchild = h.backend.delegate(&child, common::REVIEW_MODEL);
    h.backend.limit_delegated(&grandchild, old);
    pump::handle_fleet_event(&h.ctx, &delegated_by(&child, &grandchild))
        .await
        .expect("handled");
    assert_eq!(
        deadlines_sent(&h, &grandchild),
        [later],
        "the nested child is raised"
    );
}

/// A steer extending the deadline while a late child is raised is never
/// lowered by that raise: the read and the raise are one under the lock.
#[tokio::test]
async fn a_late_child_raise_never_lowers_a_concurrent_extension() {
    use archon_core::protocol::{Reply, Request};
    use std::time::Duration;
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let (stage, old, _) = extended(&h).await;
    let lead = stage.aether_agent.clone().expect("a lead");
    let child = h.backend.delegate(&lead, common::REVIEW_MODEL);
    h.backend.limit_delegated(&child, old);
    let checks_before = h.backend.descendant_checks().len();
    let gate = h.backend.hold_descendants_after(0);
    let (ctx, event) = (h.ctx.clone(), child_created(&stage, &child));
    let lifting = tokio::spawn(async move { pump::handle_fleet_event(&ctx, &event).await });
    let held = async {
        while h.backend.descendant_checks().len() <= checks_before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), held)
        .await
        .expect("the raise reads the lead's tree");
    let item = h.ctx.store.lock().await.run(&stage.run).unwrap().item;
    let request = Request::SteerItem {
        item,
        steer: archon_core::steer::Steer {
            extend_minutes: Some(45),
            ..Default::default()
        },
    };
    let release = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        gate.add_permits(1);
    };
    let (steered, ()) = tokio::join!(h.cli(request), release);
    assert!(matches!(steered, Ok(Reply::Item(_))), "{steered:?}");
    tokio::time::timeout(Duration::from_secs(10), lifting)
        .await
        .expect("the raise ends")
        .unwrap()
        .expect("handled");
    let stored = common::stored_deadline(&h, &stage)
        .await
        .expect("a deadline");
    let sent = deadlines_sent(&h, &child);
    assert_eq!(
        sent.last(),
        Some(&stored),
        "the child ends at the extension: {sent:?}"
    );
}

/// A refused wake's timed-out stage records the effort it ran at, and its
/// failure reaches the orchestrator as structural, naming that effort.
#[tokio::test]
async fn a_refused_deadline_wake_reports_the_effort_it_ran_at() {
    use archon_daemon::triggers::{self, TriggerEvent};

    let h = Harness::new().await;
    let item = h.enqueue("ran out of time, advised").await;
    h.dispatch().await;
    let first = h.running().await.remove(0);
    stop_at_deadline(&h, &first, false).await;

    let ran = h.backend.requests()[0].choice.clone();
    let stages = h.stages(&item.id).await;
    assert!(!stages.is_empty());
    for stage in &stages {
        assert_eq!(stage.state, StageState::TimedOut, "{stage:?}");
        assert_eq!(stage.choice, ran, "the record holds what ran: {stage:?}");
    }
    let held = triggers::held(&h.ctx).await.unwrap();
    let summary = held
        .iter()
        .find_map(|event| match event {
            TriggerEvent::ItemFinished {
                item: id, summary, ..
            } if id == &item.id => Some(summary.clone()),
            _ => None,
        })
        .expect("the failure is reported");
    assert!(summary.contains("Failure class: structural"), "{summary}");
    let step = format!("one effort step up from {} on {}", ran.effort, ran.model);
    assert!(summary.contains(&step), "{summary}");
}
