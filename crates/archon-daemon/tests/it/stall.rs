//! The stage watchdog (issue #251): a stage whose agent is left `running`
//! with no engine and no events is retried once, then failed with an
//! infrastructure reason, without waiting for the stage timeout.

use crate::common;

use archon_aether::events::{AetherEvent, EventKind};
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ports::TreePulse;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageRun, StageState};
use archon_daemon::dispatcher;
use common::Harness;

const DEAD: TreePulse = TreePulse {
    engine_alive: Some(false),
    last_event_at: None,
    held: false,
    running: true,
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
