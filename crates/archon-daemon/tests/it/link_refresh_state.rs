//! The refresh's stored state (the generation counter, the unfinished
//! marker, the stored agent and the frozen creation settings) changes in one
//! transaction per transition. Each transition is stopped before its commit,
//! by a store write that fails halfway, and after it, by the Aether call
//! that follows; recovery then starts the fresh generation, never a restart
//! of the outdated checkpoint.

use crate::link_common;
use crate::link_economy::{
    ORCH, ORCH_2, agent_table, as_if_created_by_an_older_build, cancel_stops, create,
    script_successor, set_state,
};

use std::sync::Arc;

use archon_aether::fake::Scripted;
use archon_daemon::supervisor::{self, Economy};
use archon_queue::Store;
use archon_queue::store::keys;
use chrono::TimeDelta;
use link_common::Harness;
use serde_json::{Value, json};

const MARKER: &str = "orchestrator_refresh_unfinished";
const TRIED: &str = "orchestrator_refresh_tried";
const THRESHOLD: &str = "orchestrator_compaction_tokens";

/// The four values that change together, as stored now.
#[derive(Debug, PartialEq, Eq)]
struct Stored {
    generation: Option<String>,
    marker: Option<String>,
    agent: Option<String>,
    threshold_key: Option<String>,
}

async fn stored(harness: &Harness) -> Stored {
    let threshold = harness.kv(THRESHOLD).await;
    let threshold_key = threshold.map(|raw| {
        let frozen: Value = serde_json::from_str(&raw).expect("a frozen threshold");
        frozen["key"].as_str().expect("its creation key").to_owned()
    });
    Stored {
        generation: harness.kv(keys::ORCHESTRATOR_GENERATION).await,
        marker: harness.kv(MARKER).await,
        agent: harness.kv(keys::ORCHESTRATOR_AGENT).await,
        threshold_key,
    }
}

/// An outdated idle generation 1 (`ORCH`) on a store kept on disk, so a
/// second connection can make one of its writes fail.
async fn outdated_on_disk() -> (Harness, crate::link_economy::Agents) {
    let harness = Harness::new().await;
    let on_disk = Store::open(&harness.ctx.paths.store).expect("a store on disk");
    *harness.ctx.store.lock().await = on_disk;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    (harness, agents)
}

/// How a write of one `kv` key is made to fail.
#[derive(Clone, Copy)]
enum Fails {
    Writing,
    Removing,
}

/// Make the store fail on `key` as `fails` says, as a stop in the middle of
/// the transaction that touches it would: SQLite rolls that transaction back.
fn stop_at(harness: &Harness, key: &str, fails: Fails) -> rusqlite::Connection {
    assert!(
        key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
        "{key}"
    );
    let db = rusqlite::Connection::open(&harness.ctx.paths.store).expect("a second connection");
    let abort = "BEGIN SELECT RAISE(ABORT, 'simulated stop'); END";
    let triggers = match fails {
        Fails::Writing => format!(
            "CREATE TRIGGER stop_insert BEFORE INSERT ON kv WHEN NEW.key = '{key}' {abort};
             CREATE TRIGGER stop_update BEFORE UPDATE ON kv WHEN NEW.key = '{key}' {abort};"
        ),
        Fails::Removing => format!(
            "CREATE TRIGGER stop_delete BEFORE DELETE ON kv WHEN OLD.key = '{key}' {abort};"
        ),
    };
    db.execute_batch(&triggers).expect("triggers installed");
    db
}

fn resume(db: &rusqlite::Connection) {
    let drop = "DROP TRIGGER IF EXISTS stop_insert; DROP TRIGGER IF EXISTS stop_update; \
                DROP TRIGGER IF EXISTS stop_delete;";
    db.execute_batch(drop).expect("triggers removed");
}

fn no_checkpoint_restart(harness: &Harness) {
    let restarts = harness.fake.received_named("restart");
    assert!(
        restarts.is_empty(),
        "the outdated checkpoint was restarted: {restarts:?}"
    );
}

/// `cancel` stops the agent, but its reply is lost.
fn cancel_stops_without_a_reply(harness: &Harness, agents: &crate::link_economy::Agents) {
    let table = agents.clone();
    harness.fake.set_responder(
        "cancel",
        Arc::new(move |cmd| {
            let id = cmd["agent"].as_str().unwrap_or_default();
            if let Some(agent) = table.lock().unwrap().get_mut(id) {
                agent["status"] = json!({ "state": "cancelled" });
            }
            Scripted::refuse("unavailable", "reply lost")
        }),
    );
}

#[tokio::test]
async fn a_stop_while_a_refresh_starts_records_its_attempt_and_marker_together_or_not_at_all() {
    let (harness, agents) = outdated_on_disk().await;
    let before = stored(&harness).await;
    assert_eq!(before.marker, None);
    assert_eq!(before.agent.as_deref(), Some(ORCH));

    let db = stop_at(&harness, MARKER, Fails::Writing);
    let now = archon_core::now();
    let stopped = supervisor::economize(&harness.ctx, now).await;
    let error = stopped.expect_err("the store stopped mid-transaction");
    assert!(error.is_store(), "{error}");
    assert_eq!(stored(&harness).await, before, "nothing changed");
    let tried = harness.kv(TRIED).await;
    assert!(tried.is_none(), "an attempt without its marker: {tried:?}");
    assert!(harness.fake.received_named("cancel").is_empty());

    resume(&db);
    script_successor(&harness, &agents);
    let again = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(again, Economy::Refreshed, "the refresh is still owed");
    assert_eq!(harness.fake.received_named("cancel").len(), 1);
    no_checkpoint_restart(&harness);
}

/// Stopped after the start committed and Aether cancelled the outdated
/// agent, before the generation advanced: all four values still describe
/// generation 1, with its marker, and recovery finishes it fresh.
#[tokio::test]
async fn a_stop_after_a_refresh_started_finishes_it_with_a_fresh_generation() {
    let (harness, agents) = outdated_on_disk().await;
    let before = stored(&harness).await;
    cancel_stops_without_a_reply(&harness, &agents);
    let now = archon_core::now();
    supervisor::economize(&harness.ctx, now)
        .await
        .expect_err("the cancel's reply was lost");
    let started = Stored {
        marker: Some("1".into()),
        ..before
    };
    assert_eq!(stored(&harness).await, started, "only the marker was added");
    assert!(harness.kv(TRIED).await.is_some(), "with its attempt");

    cancel_stops(&harness, &agents);
    script_successor(&harness, &agents);
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id.as_str(), ORCH_2, "a fresh generation");
    no_checkpoint_restart(&harness);
    let after = stored(&harness).await;
    assert_eq!(after.generation.as_deref(), Some("2"));
    assert_eq!(after.marker, None, "adoption ended the refresh");
    assert_eq!(after.agent.as_deref(), Some(ORCH_2));
}

#[tokio::test]
async fn a_stop_while_the_generation_advances_changes_all_four_values_or_none() {
    let (harness, agents) = outdated_on_disk().await;
    let before = stored(&harness).await;
    assert!(
        before.threshold_key.is_some(),
        "generation 1 froze its settings"
    );

    // The last write of the transition: the counter and agent precede it.
    let db = stop_at(&harness, THRESHOLD, Fails::Removing);
    let now = archon_core::now();
    let stopped = supervisor::economize(&harness.ctx, now).await;
    let error = stopped.expect_err("the store stopped mid-transaction");
    assert!(error.is_store(), "{error}");
    let halfway = Stored {
        marker: Some("1".into()),
        ..before
    };
    assert_eq!(
        stored(&harness).await,
        halfway,
        "the counter moved without the agent and its settings"
    );

    resume(&db);
    script_successor(&harness, &agents);
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id.as_str(), ORCH_2, "a fresh generation");
    no_checkpoint_restart(&harness);
    let after = stored(&harness).await;
    assert_eq!(after.generation.as_deref(), Some("2"));
    assert_eq!(after.marker, None);
    assert_ne!(after.threshold_key, halfway.threshold_key, "frozen anew");
}

/// Stopped after the advance committed, before the next generation was
/// created: no agent and no settings are stored under generation 2, the
/// marker still stands, and the next check creates that generation.
#[tokio::test]
async fn a_stop_after_the_generation_advanced_creates_that_generation() {
    let (harness, agents) = outdated_on_disk().await;
    harness.fake.script(
        "create_agent",
        Scripted::refuse("unavailable", "reply lost"),
    );
    let now = archon_core::now();
    supervisor::economize(&harness.ctx, now)
        .await
        .expect_err("the successor was not created");
    let advanced = stored(&harness).await;
    assert_eq!(advanced.generation.as_deref(), Some("2"));
    assert_eq!(
        advanced.agent, None,
        "the outdated agent left with its generation"
    );
    assert_eq!(advanced.marker.as_deref(), Some("1"), "the refresh is owed");
    let failed = harness.kv(TRIED).await.expect("the attempt");
    assert!(
        failed.contains("reply lost"),
        "status can say why: {failed}"
    );

    script_successor(&harness, &agents);
    let next = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(1));
    assert_eq!(next.await.unwrap(), Economy::Refreshed);
    no_checkpoint_restart(&harness);
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 3, "generation 1, then generation 2 twice");
    assert_eq!(
        creates[1]["idempotency_key"], creates[2]["idempotency_key"],
        "the retry is the same creation"
    );
    let after = stored(&harness).await;
    assert_eq!(after.threshold_key, advanced.threshold_key);
    assert_eq!(after.marker, None);
    assert_eq!(after.agent.as_deref(), Some(ORCH_2));
}

#[tokio::test]
async fn a_stop_while_a_successor_is_adopted_stores_it_and_ends_the_refresh_together() {
    let (harness, agents) = outdated_on_disk().await;
    script_successor(&harness, &agents);
    // Adoption stores the agent, then removes the marker: stop at the marker.
    let db = stop_at(&harness, MARKER, Fails::Removing);
    let now = archon_core::now();
    let stopped = supervisor::economize(&harness.ctx, now).await;
    let error = stopped.expect_err("the store stopped mid-transaction");
    assert!(error.is_store(), "{error}");
    let unadopted = stored(&harness).await;
    assert_eq!(unadopted.generation.as_deref(), Some("2"));
    assert_eq!(unadopted.marker.as_deref(), Some("1"));
    assert_eq!(
        unadopted.agent, None,
        "a successor stored beside the marker of the refresh it ends"
    );

    resume(&db);
    script_successor(&harness, &agents);
    let next = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(1));
    assert_eq!(next.await.unwrap(), Economy::Refreshed);
    no_checkpoint_restart(&harness);
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(
        creates[1]["idempotency_key"], creates[2]["idempotency_key"],
        "the same creation, adopted this time"
    );
    let after = stored(&harness).await;
    assert_eq!(after.marker, None);
    assert_eq!(after.agent.as_deref(), Some(ORCH_2));
    assert_eq!(after.generation.as_deref(), Some("2"));
}

/// Stopped after adoption committed: the refresh is over, so the fresh
/// generation that later stops is restarted from its own checkpoint.
#[tokio::test]
async fn a_stop_after_adoption_leaves_a_current_generation_that_restarts_from_its_checkpoint() {
    let (harness, agents) = outdated_on_disk().await;
    script_successor(&harness, &agents);
    let now = archon_core::now();
    let refreshed = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(refreshed, Economy::Refreshed);
    let adopted = stored(&harness).await;
    assert_eq!(adopted.marker, None, "cleared with the adoption");
    assert_eq!(adopted.agent.as_deref(), Some(ORCH_2));
    assert_eq!(adopted.generation.as_deref(), Some("2"));

    set_state(&agents, ORCH_2, "cancelled");
    const ORCH_3: &str = "agt_0003";
    let successor = crate::link_economy::orchestrator(ORCH_3, "ready");
    agents
        .lock()
        .unwrap()
        .insert(ORCH_3.into(), successor.clone());
    harness
        .fake
        .script("restart", Scripted::reply("agent", successor));
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id.as_str(), ORCH_3);
    let restarts = harness.fake.received_named("restart");
    assert_eq!(restarts.len(), 1, "its conversation is kept: {restarts:?}");
    assert_eq!(harness.fake.received_named("create_agent").len(), 2);
}

/// A marker that names no generation that was (a store another build left)
/// is a refresh owed all the same: recovery finishes it fresh, no panic.
#[tokio::test]
async fn a_marker_that_matches_no_generation_is_a_refresh_owed() {
    for marker in ["0", "7"] {
        let (harness, agents) = outdated_on_disk().await;
        harness.set_kv(MARKER, marker).await;
        set_state(&agents, ORCH, "cancelled");
        script_successor(&harness, &agents);
        let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
        assert_eq!(id.as_str(), ORCH_2, "marker {marker}: a fresh generation");
        no_checkpoint_restart(&harness);
        assert_eq!(stored(&harness).await.marker, None);
    }
}

/// A refresh that began and whose outdated agent still runs stays owed
/// though the generation no longer looks outdated: it ends with a successor.
#[tokio::test]
async fn a_leftover_marker_beside_a_live_agent_is_refreshed_at_the_next_idle_moment() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    set_state(&agents, ORCH, "waiting");
    let now = archon_core::now();
    let current = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(current, Economy::Nothing, "up to date, nothing unfinished");

    harness.set_kv(MARKER, "1").await;
    script_successor(&harness, &agents);
    let owed = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(owed, Economy::Refreshed);
    assert_eq!(stored(&harness).await.marker, None);
    assert_eq!(stored(&harness).await.agent.as_deref(), Some(ORCH_2));
}
