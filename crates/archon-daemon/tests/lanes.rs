//! Independent runs proceed in parallel: one item's slow Aether call does
//! not hold back the start of another (ARCHITECTURE §7.3).

mod common;

use std::time::Duration;

use archon_core::queue::QueueState;
use archon_daemon::{dispatcher, executor};
use common::Harness;

/// Whether an agent was asked for with `text` in its objective.
fn requested(h: &Harness, text: &str) -> bool {
    let requests = h.backend.requests();
    requests.iter().any(|r| r.objective.contains(text))
}

#[tokio::test]
async fn a_stalled_spawn_holds_back_no_other_run() {
    let h = Harness::new().await;
    let slow = h.enqueue("slow item").await;
    let fast = h.enqueue("fast item").await;
    let gate = h.backend.stall_spawns("brief for slow item");
    executor::coordination::reconcile(&h.ctx)
        .await
        .expect("coordination pass");
    let ctx = h.ctx.clone();
    let dispatch = tokio::spawn(async move { dispatcher::dispatch_once(&ctx).await });

    let fast_spawned = async {
        while !requested(&h, "brief for fast item") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), fast_spawned)
        .await
        .expect("the fast item's agent is asked for while the slow one's waits");
    assert!(
        !requested(&h, "brief for slow item"),
        "the slow spawn is still held"
    );

    gate.add_permits(1);
    dispatch
        .await
        .expect("dispatch task")
        .expect("dispatch pass");
    assert!(requested(&h, "brief for slow item"));
    for item in [&slow, &fast] {
        let state = h.state(&item.id).await;
        assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
    }
}
