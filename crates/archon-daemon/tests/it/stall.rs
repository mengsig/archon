//! The stage watchdog (issue #251): a stage whose agent is left `running`
//! with no engine and no events is retried once, then failed with an
//! infrastructure reason, without waiting for the stage timeout.

use crate::common;

use archon_aether::events::EventKind;
use archon_core::aether_ids::AetherAgentId;
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
fn went_silent(h: &Harness, agent: &AetherAgentId, minutes: i64) {
    let then = archon_core::now() - chrono::Duration::minutes(minutes);
    h.ctx
        .activity
        .saw(agent, &EventKind::AgentStarted { turn: 1 }, then);
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
    went_silent(&h, &lead(&stage), 600);
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
