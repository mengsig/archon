//! The stage watchdog (issue #251): a stage whose agent is left `running`
//! with no engine and no events is retried once, then failed with an
//! infrastructure reason, without waiting for the stage timeout.

use crate::common;

use archon_aether::events::{AetherEvent, EventKind};
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ports::{OpenToolCall, TreePulse};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageRun, StageState};
use archon_daemon::dispatcher;
use common::Harness;

const DEAD: TreePulse = TreePulse {
    engine_alive: Some(false),
    last_event_at: None,
    held: false,
    held_for_cause: false,
    inspection: false,
    running: true,
};

/// The tree after an older aetherd restarted under a running command: the
/// lead is `blocked` for inspection, so nothing in it runs.
const INSPECTED: TreePulse = TreePulse {
    inspection: true,
    running: false,
    ..DEAD
};

fn lead(stage: &StageRun) -> AetherAgentId {
    stage
        .aether_agent
        .clone()
        .expect("a running stage has an agent")
}

/// The stuck state: the lead's turn opened `minutes` ago and nothing has
/// been heard since. The event is back-dated: archond's clock stays real.
/// Recording it also lets the next sweep read the tree's pulse again.
fn went_silent(h: &Harness, agent: &AetherAgentId, minutes: i64) {
    let then = archon_core::now() - chrono::Duration::minutes(minutes);
    h.ctx.activity.saw(agent, then);
}

async fn sweep(h: &Harness) {
    dispatcher::dispatch_once(&h.ctx)
        .await
        .expect("dispatch pass");
}

async fn the_only_running(h: &Harness) -> StageRun {
    let mut running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    running.remove(0)
}

#[tokio::test]
async fn a_stalled_stage_is_retried_once_then_failed_as_infrastructure() {
    let h = Harness::new().await;
    let item = h.enqueue("stuck after a database lock").await;
    h.dispatch().await;
    let first = the_only_running(&h).await;
    went_silent(&h, &lead(&first), 21);
    let recent = archon_core::now() - chrono::Duration::minutes(19);
    let heard = TreePulse {
        last_event_at: Some(recent),
        ..DEAD
    };
    h.backend.set_pulse(&lead(&first), heard);
    sweep(&h).await;
    assert_eq!(
        the_only_running(&h).await.id,
        first.id,
        "Aether heard from the tree 19 min ago: not yet stalled"
    );

    h.backend.set_pulse(&lead(&first), DEAD);
    went_silent(&h, &lead(&first), 21);
    sweep(&h).await;
    let retry = the_only_running(&h).await;
    assert_ne!(retry.id, first.id, "the stage was re-spawned");
    assert_eq!(retry.stage_id, first.stage_id);
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("the stalled stage fails: {:?}", failed.state);
    };
    assert!(
        error.starts_with("agent stalled: no engine and no events for 2"),
        "{error}"
    );
    assert!(failed.infrastructure_failure);
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress
            .iter()
            .any(|p| p.contains("agent stalled") && p.contains("retrying (1 of 1)")),
        "the feed shows the retry: {progress:?}"
    );

    h.backend.set_pulse(&lead(&retry), DEAD);
    went_silent(&h, &lead(&retry), 25);
    sweep(&h).await;
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("a second stall is final: {:?}", h.state(&item.id).await);
    };
    assert!(
        error.contains("agent stalled: no engine and no events for 25 min"),
        "{error}"
    );
    let Ok(Reply::ItemDetail(detail)) = h.cli(Request::GetItem { item: item.id }).await else {
        panic!("get item");
    };
    assert_eq!(detail.stages.len(), 2, "one retry, no more");
    assert!(detail.stages.iter().all(|s| matches!(
        &s.state,
        StageState::Failed { error } if error.starts_with("agent stalled")
    )));
}

#[tokio::test]
async fn a_held_or_live_tree_never_stalls() {
    let h = Harness::new().await;
    h.enqueue("waiting on an approval").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    for pulse in [
        TreePulse { held: true, ..DEAD },
        TreePulse {
            engine_alive: Some(true),
            ..DEAD
        },
        TreePulse {
            last_event_at: Some(archon_core::now()),
            ..DEAD
        },
    ] {
        h.backend.set_pulse(&lead(&stage), pulse);
        went_silent(&h, &lead(&stage), 600);
        sweep(&h).await;
        assert_eq!(the_only_running(&h).await.id, stage.id, "{pulse:?}");
    }
}

#[tokio::test]
async fn without_the_engine_report_a_running_tree_stalls_after_the_fallback() {
    let h = Harness::new().await;
    let item = h.enqueue("an older Aether").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    went_silent(&h, &lead(&stage), 46);
    let waiting = TreePulse {
        engine_alive: None,
        running: false,
        ..DEAD
    };
    h.backend.set_pulse(&lead(&stage), waiting);
    sweep(&h).await;
    assert_eq!(
        the_only_running(&h).await.id,
        stage.id,
        "a tree waiting on its wake is left alone"
    );

    let running = TreePulse {
        running: true,
        ..waiting
    };
    h.backend.set_pulse(&lead(&stage), running);
    went_silent(&h, &lead(&stage), 46);
    sweep(&h).await;
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("silence alone stalls it: {:?}", failed.state);
    };
    assert!(
        error.starts_with("agent stalled: no events for 46 min"),
        "{error}"
    );
}

/// Issue AR4: an agent Aether blocked for inspection waits for a person
/// Archon never asks; the watchdog resumes it with a note instead.
#[tokio::test]
async fn a_stage_blocked_for_inspection_is_resumed_with_a_note() {
    let h = Harness::new().await;
    let item = h.enqueue("docker compose up when aetherd restarted").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    h.backend.set_pulse(&lead(&stage), INSPECTED);
    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;

    let cleared = h.backend.inspections_cleared();
    assert_eq!(cleared.len(), 1, "{cleared:?}");
    assert_eq!(cleared[0].0, lead(&stage));
    assert!(
        cleared[0].1.contains("outcome is unknown"),
        "{}",
        cleared[0].1
    );
    assert_eq!(the_only_running(&h).await.id, stage.id, "it works on");
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress
            .iter()
            .any(|p| p.contains("blocked for inspection")),
        "the feed says so: {progress:?}"
    );
    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;
    assert_eq!(h.backend.inspections_cleared().len(), 1, "resumed once");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// Moving a held deadline is an event in Aether's log, so its clock is fresh;
/// only archond's own stream, which never sees the move, measures the silence.
#[tokio::test]
async fn a_moved_deadline_does_not_put_off_an_inspection_resume() {
    let h = Harness::new().await;
    h.enqueue("blocked near its deadline").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let moved_just_now = TreePulse {
        last_event_at: Some(archon_core::now()),
        ..INSPECTED
    };
    h.backend.set_pulse(&lead(&stage), moved_just_now);
    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;
    assert_eq!(h.backend.inspections_cleared().len(), 1);
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

#[tokio::test]
async fn a_refused_inspection_resume_fails_the_stage_as_infrastructure() {
    let h = Harness::new().await;
    let item = h.enqueue("Aether will not resume it").await;
    h.dispatch().await;
    let first = the_only_running(&h).await;
    h.backend.set_pulse(&lead(&first), INSPECTED);
    h.backend.fail_next_inspection(ArchonError::Aether(
        "invalid argument: agent is not under inspection".into(),
    ));
    went_silent(&h, &lead(&first), 21);
    sweep(&h).await;

    let retry = the_only_running(&h).await;
    assert_ne!(retry.id, first.id, "the infra retry replaces it at once");
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("the refused stage fails: {:?}", failed.state);
    };
    assert!(error.contains("blocked it for inspection"), "{error}");
    assert!(error.contains("not under inspection"), "{error}");
    assert!(failed.infrastructure_failure);
}

/// Someone resumed the agent between Archon's read and its resume: the
/// refusal that follows is no stall, and the stage keeps its agent.
#[tokio::test]
async fn a_refusal_for_an_agent_resumed_meanwhile_keeps_the_stage() {
    let h = Harness::new().await;
    let item = h.enqueue("the operator resumed it first").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    h.backend.set_pulse(&lead(&stage), INSPECTED);
    h.backend
        .resume_elsewhere_before_next_inspection(ArchonError::Aether(
            "invalid argument: agent is not under inspection".into(),
        ));
    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;

    assert_eq!(the_only_running(&h).await.id, stage.id, "still at work");
    assert_eq!(h.stages(&item.id).await.len(), 1, "no retry was needed");
}

#[tokio::test]
async fn an_unanswered_inspection_resume_is_tried_again() {
    let h = Harness::new().await;
    h.enqueue("Aether is busy").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    h.backend.set_pulse(&lead(&stage), INSPECTED);
    h.backend
        .fail_next_inspection(ArchonError::Unavailable("reply lost".into()));
    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;
    assert!(h.backend.inspections_cleared().is_empty());
    assert_eq!(the_only_running(&h).await.id, stage.id);

    went_silent(&h, &lead(&stage), 21);
    sweep(&h).await;
    assert_eq!(h.backend.inspections_cleared().len(), 1);
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

/// A hold with no cause Aether names (a dependency, a resource lease, a
/// reason Archon cannot read) is told to the orchestrator once; never killed.
#[tokio::test]
async fn a_long_hold_with_no_named_cause_is_escalated_once() {
    let h = Harness::new().await;
    h.enqueue("blocked on a lease nobody frees").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let held = TreePulse {
        held: true,
        running: false,
        ..DEAD
    };
    h.backend.set_pulse(&lead(&stage), held);
    let told = |notices: Vec<String>| {
        notices
            .into_iter()
            .filter(|n| n.contains("held by Aether"))
            .count()
    };
    for _ in 0..2 {
        went_silent(&h, &lead(&stage), 46);
        sweep(&h).await;
    }
    assert_eq!(told(h.notices(NotificationKind::Blocker).await), 1, "once");
    assert_eq!(the_only_running(&h).await.id, stage.id, "not stopped");

    let usage = TreePulse {
        held_for_cause: true,
        ..held
    };
    let h2 = Harness::new().await;
    h2.enqueue("blocked on a usage limit").await;
    h2.dispatch().await;
    let waiting = the_only_running(&h2).await;
    h2.backend.set_pulse(&lead(&waiting), usage);
    went_silent(&h2, &lead(&waiting), 600);
    sweep(&h2).await;
    assert_eq!(told(h2.notices(NotificationKind::Blocker).await), 0);
}

/// Aether's own `task.cancelled` for the stage, as the fleet stream hears it.
async fn cancelled_event(h: &Harness, stage: &StageRun) {
    let event = AetherEvent {
        seq: 1,
        at: archon_core::now(),
        agent: stage.aether_agent.clone(),
        task: stage.aether_task.clone(),
        parent: None,
        run: None,
        kind: EventKind::TaskCancelled {
            reason: Some("agent stalled".into()),
        },
    };
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .expect("fleet event");
}

#[tokio::test]
async fn the_cancels_own_event_settling_first_still_retries_the_stall() {
    let h = Harness::new().await;
    let item = h.enqueue("cancel event before its reply").await;
    h.dispatch().await;
    let first = the_only_running(&h).await;
    went_silent(&h, &lead(&first), 21);
    h.backend.set_pulse(&lead(&first), DEAD);
    h.backend
        .fail_next_cancel(ArchonError::Unavailable("reply lost".into()));
    sweep(&h).await;
    assert_eq!(
        the_only_running(&h).await.id,
        first.id,
        "an unanswered cancel leaves the stage to the next sweep"
    );

    cancelled_event(&h, &first).await;
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("the watchdog's cancel is its stall: {:?}", failed.state);
    };
    assert!(error.starts_with("agent stalled: no engine"), "{error}");
    assert!(failed.infrastructure_failure);
    sweep(&h).await;
    let retry = the_only_running(&h).await;
    assert_ne!(retry.id, first.id, "the stall is retried, not cancelled");
}

#[tokio::test]
async fn zero_stall_minutes_leaves_a_dead_tree_to_its_timeout() {
    let mut config = archon_core::config::Config::default();
    config.review.auto_repair = false;
    config.limits.stall_minutes = 0;
    let h = Harness::with_config(config).await;
    h.enqueue("watchdog off").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    h.backend.set_pulse(&lead(&stage), DEAD);
    went_silent(&h, &lead(&stage), 600);
    sweep(&h).await;
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

/// An agent Aether holds (`blocked` on a usage limit or credentials) is
/// suspended, not failed: its deadline moves on in Aether and in the store.
#[tokio::test]
async fn a_held_stage_near_its_deadline_has_it_moved() {
    let h = Harness::new().await;
    h.enqueue("blocked on a usage limit").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let soon = archon_core::now() + chrono::Duration::minutes(5);
    common::set_deadline(&h, &stage, soon).await;
    h.backend
        .set_pulse(&lead(&stage), TreePulse { held: true, ..DEAD });
    sweep(&h).await;

    let moved = common::stored_deadline(&h, &stage)
        .await
        .expect("a deadline");
    assert!(
        moved > soon + chrono::Duration::minutes(20),
        "{moved} after {soon}"
    );
    let updates = h.backend.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].0, lead(&stage));
    assert_eq!(updates[0].1.deadline, Some(moved));
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

/// An Aether that cannot update a subtree has each live helper whose deadline
/// is lower raised with the lead: the one it holds may be a helper.
#[tokio::test]
async fn a_held_stage_has_its_helpers_deadlines_moved_one_by_one() {
    let h = Harness::new().await;
    h.enqueue("a helper waits on an approval").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let soon = archon_core::now() + chrono::Duration::minutes(5);
    common::set_deadline(&h, &stage, soon).await;
    let held = h.backend.delegate(&lead(&stage), common::MODEL);
    h.backend.limit_delegated(&held, soon);
    let unlimited = h.backend.delegate(&lead(&stage), common::MODEL);
    h.backend
        .set_pulse(&lead(&stage), TreePulse { held: true, ..DEAD });
    sweep(&h).await;

    let moved = common::stored_deadline(&h, &stage)
        .await
        .expect("a deadline");
    let raised: Vec<_> = h
        .backend
        .updates()
        .into_iter()
        .map(|u| (u.0, u.1.deadline))
        .collect();
    assert_eq!(
        raised,
        [(lead(&stage), Some(moved)), (held, Some(moved))],
        "{unlimited} keeps no deadline"
    );
}

#[tokio::test]
async fn a_stage_near_its_deadline_that_is_not_held_keeps_it() {
    let h = Harness::new().await;
    h.enqueue("working to the end").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let soon = archon_core::now() + chrono::Duration::minutes(5);
    common::set_deadline(&h, &stage, soon).await;
    sweep(&h).await;

    assert_eq!(common::stored_deadline(&h, &stage).await, Some(soon));
    assert!(h.backend.updates().is_empty());
}

#[tokio::test]
async fn a_held_stage_past_its_deadline_is_not_timed_out() {
    let h = Harness::new().await;
    let item = h.enqueue("held while the daemon was down").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    common::expire_deadline(&h, &stage).await;
    h.backend
        .set_pulse(&lead(&stage), TreePulse { held: true, ..DEAD });
    sweep(&h).await;

    assert_eq!(the_only_running(&h).await.id, stage.id, "not timed out");
    assert!(h.backend.cancelled().is_empty());
    let moved = common::stored_deadline(&h, &stage)
        .await
        .expect("a deadline");
    assert!(moved > archon_core::now() + chrono::Duration::minutes(20));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// An inspection holds the deadline only while the watchdog is there to
/// resume it; with the watchdog off it can never wait forever.
#[tokio::test]
async fn an_inspection_holds_the_deadline_only_while_the_watchdog_is_on() {
    let h = Harness::new().await;
    h.enqueue("blocked for inspection past its deadline").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    common::expire_deadline(&h, &stage).await;
    h.backend.set_pulse(&lead(&stage), INSPECTED);
    sweep(&h).await;
    assert_eq!(the_only_running(&h).await.id, stage.id, "not timed out");
    assert!(h.backend.cancelled().is_empty());

    let mut config = archon_core::config::Config::default();
    config.review.auto_repair = false;
    config.limits.stall_minutes = 0;
    let off = Harness::with_config(config).await;
    let item = off.enqueue("no watchdog to resume it").await;
    off.dispatch().await;
    let unwatched = the_only_running(&off).await;
    common::expire_deadline(&off, &unwatched).await;
    off.backend.set_pulse(&lead(&unwatched), INSPECTED);
    sweep(&off).await;
    let settled = off.stages(&item.id).await.remove(0);
    assert_eq!(settled.state, StageState::TimedOut, "{:?}", settled.state);
}

/// A stage past its deadline whose tree Aether did not answer for is not
/// timed out: Aether may hold it. The next sweep that reads it held moves it.
#[tokio::test]
async fn an_expired_stage_whose_pulse_is_unanswered_waits_for_the_next_read() {
    let h = Harness::new().await;
    h.enqueue("held, and Aether slow to say so").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    common::expire_deadline(&h, &stage).await;
    h.backend
        .set_pulse(&lead(&stage), TreePulse { held: true, ..DEAD });
    h.backend
        .fail_next_pulse(ArchonError::Unavailable("aetherd busy".into()));
    sweep(&h).await;

    assert_eq!(the_only_running(&h).await.id, stage.id, "not timed out");
    assert!(h.backend.cancelled().is_empty());
    sweep(&h).await;
    let moved = common::stored_deadline(&h, &stage)
        .await
        .expect("a deadline");
    assert!(moved > archon_core::now() + chrono::Duration::minutes(20));
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

/// The deadline check of a stage near its deadline reads the tree too; it
/// must not take the watchdog's read, or a stall would wait for the timeout.
#[tokio::test]
async fn a_stalled_stage_near_its_deadline_still_stalls() {
    let h = Harness::new().await;
    let item = h.enqueue("stuck just before its deadline").await;
    h.dispatch().await;
    let first = the_only_running(&h).await;
    let soon = archon_core::now() + chrono::Duration::minutes(5);
    common::set_deadline(&h, &first, soon).await;
    h.backend.set_pulse(&lead(&first), DEAD);
    went_silent(&h, &lead(&first), 21);
    sweep(&h).await;

    let failed = h.stages(&item.id).await.remove(0);
    assert_eq!(failed.id, first.id);
    let StageState::Failed { error } = &failed.state else {
        panic!("the stalled stage fails: {:?}", failed.state);
    };
    assert!(error.starts_with("agent stalled"), "{error}");
    assert_ne!(the_only_running(&h).await.id, first.id, "re-spawned");
}

/// A steer that extends the deadline while the held stage's own update is in
/// flight keeps its later deadline, in the store and in Aether.
#[tokio::test]
async fn a_held_stage_keeps_a_later_deadline_a_steer_stored_meanwhile() {
    let h = Harness::new().await;
    h.enqueue("blocked while the operator extends it").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    common::set_deadline(
        &h,
        &stage,
        archon_core::now() + chrono::Duration::minutes(5),
    )
    .await;
    h.backend
        .set_pulse(&lead(&stage), TreePulse { held: true, ..DEAD });
    let steered = archon_core::now() + chrono::Duration::hours(2);
    let (ctx, id) = (h.ctx.clone(), stage.id.clone());
    h.backend.on_next_update(move || {
        let mut store = ctx
            .store
            .try_lock()
            .expect("the store is free during an update");
        common::write_deadline(&mut store, &id, steered);
    });
    sweep(&h).await;

    assert_eq!(common::stored_deadline(&h, &stage).await, Some(steered));
    let deadlines: Vec<_> = h
        .backend
        .updates()
        .into_iter()
        .map(|(_, u)| u.deadline)
        .collect();
    assert_eq!(deadlines.len(), 2, "{deadlines:?}");
    assert!(deadlines[0] < Some(steered));
    assert_eq!(
        deadlines[1],
        Some(steered),
        "Aether ends with the later one"
    );
}

/// A turn in flight whose lease Aether renews every minute: its own clock
/// never goes quiet.
fn renewed() -> TreePulse {
    TreePulse {
        engine_alive: Some(true),
        last_event_at: Some(archon_core::now()),
        ..DEAD
    }
}

/// A Bash call `command` the lead opened `minutes` ago, with nothing since.
fn open_call(id: &str, command: &str, minutes: i64) -> OpenToolCall {
    let then = archon_core::now() - chrono::Duration::minutes(minutes);
    OpenToolCall {
        id: id.into(),
        name: "Bash".into(),
        command: Some(command.into()),
        requested_at: then,
        last_event_at: then,
    }
}

const STUCK: &str = "git status --short; git rev-parse HEAD HEAD^{tree}; kill 1227080";

/// 2026-11-01: a stage agent sat 85 min in one Bash call while its engine
/// lived and Aether renewed its lease; the watchdog never fired.
#[tokio::test]
async fn a_lead_hung_in_a_tool_call_is_woken_once_then_its_stage_retried() {
    let h = Harness::new().await;
    let item = h.enqueue("hung in a bash call").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let agent = lead(&stage);
    h.backend.set_pulse(&agent, renewed());
    h.backend
        .set_open_call(&agent, open_call("call_1", STUCK, 21));
    went_silent(&h, &agent, 21);
    let mail = AetherEvent {
        seq: 7,
        at: archon_core::now(),
        agent: Some(agent.clone()),
        task: None,
        parent: None,
        run: None,
        kind: EventKind::InboxDelivered {
            seq: 3,
            from_operator: false,
            message: None,
        },
    };
    archon_daemon::pump::handle_fleet_event(&h.ctx, &mail)
        .await
        .expect("mail delivered");
    sweep(&h).await;
    assert_eq!(
        h.backend.interrupts(),
        vec![agent.clone()],
        "mail it never read is no progress; the lead alone is interrupted"
    );
    let notes: Vec<String> = h
        .backend
        .messages()
        .into_iter()
        .filter(|(to, text)| {
            *to == agent && text.contains("hung for 21 min and was interrupted; continue")
        })
        .map(|(_, text)| text)
        .collect();
    assert_eq!(notes.len(), 1, "woken once with a note: {notes:?}");
    assert!(notes[0].contains("kill 1227080"), "{}", notes[0]);
    assert_eq!(
        the_only_running(&h).await.id,
        stage.id,
        "the same agent goes on"
    );
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress.iter().any(|p| p.contains("interrupted the turn")),
        "the orchestrator is told: {progress:?}"
    );

    h.backend
        .set_open_call(&agent, open_call("call_2", "sleep 99999", 21));
    went_silent(&h, &agent, 21);
    sweep(&h).await;
    assert_eq!(h.backend.interrupts().len(), 1, "never a second interrupt");
    assert_eq!(
        the_only_running(&h).await.id,
        stage.id,
        "it has time to recover"
    );

    let long_ago = (archon_core::now() - chrono::Duration::minutes(21)).to_rfc3339();
    let mark = format!("hung_tool:{}", stage.id);
    h.ctx.store.lock().await.kv_set(&mark, &long_ago).unwrap();
    went_silent(&h, &agent, 21);
    sweep(&h).await;
    assert_eq!(h.backend.interrupts().len(), 1, "a stop, not another wake");
    let retry = the_only_running(&h).await;
    assert_ne!(retry.id, stage.id, "the stage is retried");
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("the hung stage fails: {:?}", failed.state);
    };
    assert!(
        error.starts_with("agent stalled: tool call Bash"),
        "{error}"
    );
    assert!(failed.infrastructure_failure, "no model failure");
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress.iter().any(|p| p.contains("hung again")),
        "{progress:?}"
    );
}

#[tokio::test]
async fn a_long_foreground_check_is_not_cut_before_its_own_limit() {
    let h = Harness::new().await;
    h.enqueue("running make ci in the foreground").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let agent = lead(&stage);
    h.backend.set_pulse(&agent, renewed());
    h.backend
        .set_open_call(&agent, open_call("call_ci", "make ci", 45));
    went_silent(&h, &agent, 45);
    sweep(&h).await;
    assert!(
        h.backend.interrupts().is_empty(),
        "45 min of make ci is healthy"
    );

    h.backend
        .set_open_call(&agent, open_call("call_ci", "make ci", 61));
    went_silent(&h, &agent, 61);
    sweep(&h).await;
    assert_eq!(h.backend.interrupts(), vec![agent], "past the check limit");
    assert_eq!(the_only_running(&h).await.id, stage.id);
}

/// A bad mark in the store belongs to its stage run alone: the sweep goes on,
/// and the run counts as woken once, so this hang stops it.
#[tokio::test]
async fn a_corrupt_hung_call_mark_stops_only_its_own_stage() {
    let h = Harness::new().await;
    let item = h.enqueue("hung with a bad mark").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let agent = lead(&stage);
    h.backend.set_pulse(&agent, renewed());
    h.backend
        .set_open_call(&agent, open_call("call_1", STUCK, 21));
    went_silent(&h, &agent, 21);
    let mark = format!("hung_tool:{}", stage.id);
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&mark, "not a time")
        .unwrap();
    sweep(&h).await;

    assert!(h.backend.interrupts().is_empty(), "never woken again");
    let failed = h.stages(&item.id).await.remove(0);
    let StageState::Failed { error } = &failed.state else {
        panic!("the stage fails: {:?}", failed.state);
    };
    assert!(error.contains("hung again"), "{error}");
    assert!(failed.infrastructure_failure, "no model failure");
    assert_ne!(the_only_running(&h).await.id, stage.id, "retried");
}

/// The wake follows the mark: were the mark's write to fail, nothing woke.
#[tokio::test]
async fn the_hung_call_mark_is_written_before_the_agent_is_interrupted() {
    let h = Harness::new().await;
    h.enqueue("hung in a bash call").await;
    h.dispatch().await;
    let stage = the_only_running(&h).await;
    let agent = lead(&stage);
    h.backend.set_pulse(&agent, renewed());
    h.backend
        .set_open_call(&agent, open_call("call_1", STUCK, 21));
    went_silent(&h, &agent, 21);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (ctx, key, slot) = (
        h.ctx.clone(),
        format!("hung_tool:{}", stage.id),
        seen.clone(),
    );
    h.backend.on_next_interrupt(move || {
        let store = ctx.store.try_lock().expect("the store is free");
        *slot.lock().unwrap() = Some(store.kv_get(&key).unwrap());
    });
    sweep(&h).await;

    assert_eq!(h.backend.interrupts(), vec![agent]);
    let at_interrupt = seen.lock().unwrap().take().expect("interrupted");
    assert!(at_interrupt.is_some(), "the mark was there before the wake");
}
