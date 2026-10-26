//! The link workers under archond's own supervision: a worker that fails is
//! restarted under the worker policy, and the rest keep running meanwhile.

use crate::link_common;

use std::sync::Arc;

use archon_aether::fake::{self, Scripted};
use archon_queue::Store;
use archon_queue::store::keys;
use link_common::{Harness, spawn_link};
use serde_json::json;

const ORCH: &str = "agt_0001";

/// A store with the current schema that refuses every write.
fn frozen_store(harness: &Harness) -> Store {
    let path = harness.dir.path().join("frozen").join("store.db");
    drop(Store::open(&path).expect("a store to freeze"));
    let frozen = Store::open_read_only(&path).expect("opens read-only");
    assert!(frozen.kv_get(keys::ORCHESTRATOR_AGENT).unwrap().is_none());
    frozen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_event_pump_is_restarted_until_it_follows_aether_again() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.fake.set_responder(
        "get_agent",
        Arc::new(|cmd| {
            let mut agent = fake::agent(cmd["id"].as_str().unwrap_or_default(), None);
            agent["status"] = json!({ "state": "running" });
            Scripted::reply("agent", agent)
        }),
    );
    let frozen = frozen_store(&harness);
    let writable = std::mem::replace(&mut *harness.ctx.store.lock().await, frozen);

    // Each connection fails as the pump records Aether's instance.
    let pump = spawn_link(&harness, harness.fake.endpoint(), None);
    harness
        .eventually("the failed pump was restarted twice", || async {
            harness.fake.connections_accepted() >= 3
        })
        .await;
    assert!(!pump.is_finished(), "a store failure stops no worker set");
    assert!(!harness.ctx.aether.is_connected());

    *harness.ctx.store.lock().await = writable;
    harness
        .eventually("the restarted pump follows both streams", || async {
            harness.ctx.aether.is_connected() && harness.fake.live_subscriptions() == 2
        })
        .await;
    assert!(!pump.is_finished());
    pump.abort();
}
