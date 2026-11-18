//! The orchestrator's context economy against a fake aetherd: the
//! compaction threshold sent only to an Aether that serves it, one refresh of
//! an outdated generation at an idle moment, and one idle compaction.

use crate::link_common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use archon_aether::fake::{self, Scripted};
use archon_core::Timestamp;
use archon_core::id::QueueItemId;
use archon_daemon::supervisor::{self, Economy};
use archon_daemon::triggers::{self, TriggerEvent};
use chrono::TimeDelta;
use link_common::Harness;
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";
/// `kv`: what the newest generation was created with.
const ORIGIN_KEY: &str = "orchestrator_origin";

type Agents = Arc<Mutex<HashMap<String, Value>>>;

fn orchestrator(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// `get_agent` answers from a table the test edits; `cancel` stops the agent.
fn agent_table(harness: &Harness) -> Agents {
    let agents: Agents = Arc::default();
    let table = agents.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            match table.lock().unwrap().get(id) {
                Some(agent) => Scripted::reply("agent", agent.clone()),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    let table = agents.clone();
    harness.fake.set_responder(
        "cancel",
        Arc::new(move |cmd| {
            let id = cmd["agent"].as_str().unwrap_or_default();
            if let Some(agent) = table.lock().unwrap().get_mut(id) {
                agent["status"] = json!({ "state": "cancelled" });
            }
            Scripted::ok()
        }),
    );
    agents
}

async fn create(harness: &Harness, agents: &Agents, id: &str) {
    agents
        .lock()
        .unwrap()
        .insert(id.into(), orchestrator(id, "ready"));
    let created = Scripted::reply("agent", orchestrator(id, "ready"));
    harness.fake.script("create_agent", created);
    let adopted = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(adopted.as_str(), id);
}

/// A `waiting` agent waits for its inbox and logged its last event ten
/// minutes ago; any other state is set as given.
fn set_state(agents: &Agents, id: &str, state: &str) {
    let mut table = agents.lock().unwrap();
    let agent = table.get_mut(id).expect("a known agent");
    agent["status"] = match state {
        "waiting" => json!({ "state": state, "condition": { "on": "inbox" } }),
        _ => json!({ "state": state }),
    };
    let last_event = archon_core::now() - TimeDelta::minutes(10);
    agent["last_event_at"] = json!(last_event);
}

fn set_paused(agents: &Agents, id: &str, paused: bool) {
    let mut table = agents.lock().unwrap();
    table.get_mut(id).expect("a known agent")["paused"] = json!(paused);
}

fn script_successor(harness: &Harness, agents: &Agents) {
    agents
        .lock()
        .unwrap()
        .insert(ORCH_2.into(), orchestrator(ORCH_2, "ready"));
    let successor = Scripted::reply("agent", orchestrator(ORCH_2, "ready"));
    harness.fake.script("create_agent", successor);
}

fn set_stats(agents: &Agents, id: &str, tokens: u64, turn_ended_at: Timestamp) {
    let mut table = agents.lock().unwrap();
    let agent = table.get_mut(id).expect("a known agent");
    agent["stats"] = json!({ "context_tokens": tokens, "last_turn_at": turn_ended_at });
}

async fn as_if_created_by_an_older_build(harness: &Harness) {
    let raw = harness
        .kv(ORIGIN_KEY)
        .await
        .expect("the creation was recorded");
    let mut origin: Value = serde_json::from_str(&raw).unwrap();
    origin["archon_version"] = json!("0.0.1");
    origin["prompt"] = json!("0000000000000000");
    harness.set_kv(ORIGIN_KEY, &origin.to_string()).await;
}

#[tokio::test]
async fn the_compaction_threshold_reaches_an_aether_that_serves_it_only() {
    for served in [true, false] {
        let harness = Harness::new().await;
        let features = match served {
            true => vec!["compaction_tokens".to_owned()],
            false => Vec::new(),
        };
        harness.fake.set_features(features);
        harness.connect().await;
        let agents = agent_table(&harness);
        create(&harness, &agents, ORCH).await;
        let creates = harness.fake.received_named("create_agent");
        assert_eq!(creates.len(), 1, "one orchestrator created");
        let sent = &creates[0]["config"]["compaction_tokens"];
        match served {
            true => assert_eq!(sent, &json!(160_000), "the configured threshold"),
            false => assert!(sent.is_null(), "Aether 1.4.1 is sent no threshold: {sent}"),
        }
    }
}

#[tokio::test]
async fn a_zero_threshold_is_never_sent() {
    let mut config = archon_core::config::Config::default();
    config.orchestrator.compaction_tokens = 0;
    let harness = Harness::with(config, None).await;
    harness
        .fake
        .set_features(vec!["compaction_tokens".to_owned()]);
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    let creates = harness.fake.received_named("create_agent");
    assert!(creates[0]["config"]["compaction_tokens"].is_null());
}

#[tokio::test]
async fn a_changed_fingerprint_schedules_one_refresh_and_only_when_no_turn_runs() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    set_state(&agents, ORCH, "waiting");
    let now = archon_core::now();
    assert_eq!(
        supervisor::economize(&harness.ctx, now).await.unwrap(),
        Economy::Nothing
    );

    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "running");
    let during = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(
        during,
        Economy::Nothing,
        "a running turn is never cut short"
    );
    assert!(harness.fake.received_named("cancel").is_empty());

    set_state(&agents, ORCH, "waiting");
    script_successor(&harness, &agents);
    let idle = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(idle, Economy::Refreshed, "refreshed at the idle moment");
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(current.unwrap().as_str(), ORCH_2);
    assert_eq!(supervisor::generation(&harness.ctx).await.unwrap(), 2);

    set_state(&agents, ORCH_2, "waiting");
    let after = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(
        after,
        Economy::Nothing,
        "the fresh generation is up to date"
    );
    assert_eq!(harness.fake.received_named("create_agent").len(), 2);
    assert_eq!(
        harness.fake.received_named("cancel").len(),
        1,
        "exactly one refresh"
    );
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_old_generation_and_retries_three_times_at_most() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    harness
        .fake
        .set_responder("cancel", Arc::new(|_| Scripted::refuse("internal", "busy")));
    let start = archon_core::now();
    for minutes in [0, 10, 20] {
        let now = start + TimeDelta::minutes(minutes);
        supervisor::economize(&harness.ctx, now)
            .await
            .expect_err("the refresh failed");
        let soon = now + TimeDelta::minutes(5);
        let early = supervisor::economize(&harness.ctx, soon).await.unwrap();
        assert_eq!(early, Economy::Nothing, "a retry waits ten minutes");
    }
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(current.unwrap().as_str(), ORCH, "the old generation stays");
    let later = start + TimeDelta::minutes(60);
    let spent = supervisor::economize(&harness.ctx, later).await.unwrap();
    assert_eq!(spent, Economy::Nothing, "given up after three tries");
    assert_eq!(harness.fake.received_named("cancel").len(), 3);
}

#[tokio::test]
async fn a_refresh_waits_for_due_news_a_pause_and_an_actual_upgrade() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    set_state(&agents, ORCH, "waiting");
    let now = archon_core::now() + TimeDelta::minutes(1);

    let raw = harness
        .kv(ORIGIN_KEY)
        .await
        .expect("the creation was recorded");
    let mut origin: Value = serde_json::from_str(&raw).unwrap();
    origin["prompt"] = json!("0000000000000000");
    harness.set_kv(ORIGIN_KEY, &origin.to_string()).await;
    let edited = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(
        edited,
        Economy::Nothing,
        "an edit on the same build waits for a restart"
    );

    as_if_created_by_an_older_build(&harness).await;
    set_paused(&agents, ORCH, true);
    let paused = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(paused, Economy::Nothing, "an operator pause is kept");
    set_paused(&agents, ORCH, false);

    let news = TriggerEvent::ItemBlocked {
        item: QueueItemId::new(),
        reason: "stuck".into(),
    };
    triggers::push(&harness.ctx, news).await.unwrap();
    let due = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(
        due,
        Economy::Nothing,
        "due news reaches the running generation first"
    );
    assert!(harness.fake.received_named("cancel").is_empty());
}

#[tokio::test]
async fn an_idle_large_orchestrator_is_compacted_once_before_its_cache_expires() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let ended = archon_core::now();
    set_stats(&agents, ORCH, 150_000, ended);
    set_state(&agents, ORCH, "waiting");
    let economize = |minutes: i64| {
        let ctx = harness.ctx.clone();
        async move {
            let now = ended + TimeDelta::minutes(minutes);
            supervisor::economize(&ctx, now).await.unwrap()
        }
    };
    assert_eq!(
        economize(49).await,
        Economy::Nothing,
        "not idle long enough"
    );
    set_state(&agents, ORCH, "running");
    assert_eq!(economize(55).await, Economy::Nothing, "a turn is running");
    set_state(&agents, ORCH, "waiting");
    assert_eq!(economize(50).await, Economy::Compacted);
    assert_eq!(
        economize(51).await,
        Economy::Nothing,
        "once per idle stretch"
    );
    let compacts = harness.fake.received_named("compact");
    assert_eq!(compacts.len(), 1);
    assert_eq!(compacts[0]["agent"], json!(ORCH));
}
