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
use archon_queue::store::keys;
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
    cancel_stops(harness, &agents);
    agents
}

/// `cancel` stops the agent in `agents`.
fn cancel_stops(harness: &Harness, agents: &Agents) {
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

/// The old generation stays while the refresh fails; the feed says so after
/// three tries, and the refresh still happens once Aether recovers.
#[tokio::test]
async fn a_failed_refresh_keeps_the_old_generation_and_finishes_once_aether_recovers() {
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
    for minutes in [0, 10, 20, 30] {
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
    let told = harness.system_notes().await;
    let told: Vec<_> = told
        .iter()
        .filter(|n| n.contains("failed 3 times"))
        .collect();
    assert_eq!(told.len(), 1, "the operator is told once: {told:?}");

    cancel_stops(&harness, &agents);
    script_successor(&harness, &agents);
    let later = start + TimeDelta::minutes(40);
    let recovered = supervisor::economize(&harness.ctx, later).await.unwrap();
    assert_eq!(recovered, Economy::Refreshed, "never given up for good");
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(current.unwrap().as_str(), ORCH_2);
}

/// archond stopped after Aether cancelled the outdated generation and before
/// the next one was started: it is finished fresh, never restarted from its
/// checkpoint with the outdated instructions.
#[tokio::test]
async fn a_refresh_stopped_after_its_cancel_starts_a_fresh_generation() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    harness.set_kv("orchestrator_refresh_unfinished", "1").await;
    set_state(&agents, ORCH, "cancelled");
    script_successor(&harness, &agents);

    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id.as_str(), ORCH_2, "a fresh generation");
    let restarts = harness.fake.received_named("restart");
    assert!(
        restarts.is_empty(),
        "restarted from the checkpoint: {restarts:?}"
    );
    assert_eq!(supervisor::generation(&harness.ctx).await.unwrap(), 2);
    let unfinished = harness.kv("orchestrator_refresh_unfinished").await;
    assert!(unfinished.is_none(), "adoption ended the refresh");
    set_state(&agents, ORCH_2, "waiting");
    let now = archon_core::now();
    let after = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(after, Economy::Nothing, "the fresh generation is current");
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
async fn a_refresh_that_cancelled_but_could_not_create_is_finished_next_check() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    harness
        .fake
        .script("create_agent", Scripted::refuse("internal", "busy"));
    let now = archon_core::now();
    supervisor::economize(&harness.ctx, now)
        .await
        .expect_err("the successor was not created");
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert!(
        current.is_none(),
        "the old generation was cancelled: {current:?}"
    );

    script_successor(&harness, &agents);
    let next = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(1));
    assert_eq!(next.await.unwrap(), Economy::Refreshed);
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(
        current.unwrap().as_str(),
        ORCH_2,
        "finished at the next check"
    );
}

/// The refresh's successor stopped before its briefing, which retired it
/// and advanced the generation once more: the next check still finishes it.
#[tokio::test]
async fn a_refresh_whose_successor_stopped_before_its_briefing_is_finished_next_check() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    script_successor(&harness, &agents);
    set_state(&agents, ORCH_2, "failed");
    harness.fake.script(
        "trigger",
        Scripted::refuse("invalid_argument", "agent stopped"),
    );
    let now = archon_core::now();
    supervisor::economize(&harness.ctx, now)
        .await
        .expect_err("the successor stopped before its briefing");
    assert!(harness.kv(keys::ORCHESTRATOR_AGENT).await.is_none());
    let generation = harness.kv(keys::ORCHESTRATOR_GENERATION).await;
    assert_eq!(generation.as_deref(), Some("3"), "N+2, nothing adopted");

    const ORCH_3: &str = "agt_0003";
    agents
        .lock()
        .unwrap()
        .insert(ORCH_3.into(), orchestrator(ORCH_3, "ready"));
    let third = Scripted::reply("agent", orchestrator(ORCH_3, "ready"));
    harness.fake.script("create_agent", third);
    let next = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(1));
    assert_eq!(next.await.unwrap(), Economy::Refreshed);
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(
        current.unwrap().as_str(),
        ORCH_3,
        "finished at the next check"
    );
    let unfinished = harness.kv("orchestrator_refresh_unfinished").await;
    assert!(
        unfinished.is_none(),
        "adoption ended the refresh: {unfinished:?}"
    );
}

/// Aether restarted the idle orchestrator into a running successor after
/// the refresh rechecked it: the successor's turn is not cancelled.
#[tokio::test]
async fn a_refresh_never_cancels_a_successor_started_after_its_idle_check() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    let idle = agents.lock().unwrap()[ORCH].clone();
    // The first look and the recheck under the switch guard see it idle.
    for _ in 0..2 {
        let seen = Scripted::reply("agent", idle.clone());
        harness.fake.script("get_agent", seen);
    }
    {
        let mut table = agents.lock().unwrap();
        table.get_mut(ORCH).unwrap()["restarted_as"] = json!(ORCH_2);
        table.insert(ORCH_2.into(), orchestrator(ORCH_2, "running"));
    }

    let now = archon_core::now();
    supervisor::economize(&harness.ctx, now)
        .await
        .expect_err("the agent checked is no longer the one in charge");
    let cancels = harness.fake.received_named("cancel");
    assert!(
        cancels.is_empty(),
        "a running turn was cut off: {cancels:?}"
    );
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 1, "no fresh generation was started");
}

/// The recheck under the switch guard saw a turn start: neither rule acts
/// on the stale first answer.
#[tokio::test]
async fn a_turn_seen_by_the_refresh_recheck_also_stops_the_compaction() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    harness.fake.set_default("compact", Scripted::ok());
    let ended = archon_core::now() - TimeDelta::minutes(50);
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, ended);
    let table = agents.clone();
    let asked = Arc::new(Mutex::new(0));
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |_| {
            let mut asked = asked.lock().unwrap();
            *asked += 1;
            let mut agent = table.lock().unwrap()[ORCH].clone();
            if *asked > 1 {
                agent["status"] = json!({ "state": "running" });
            }
            Scripted::reply("agent", agent)
        }),
    );
    let now = archon_core::now();
    let economy = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(economy, Economy::Nothing);
    assert!(
        harness.fake.received_named("compact").is_empty(),
        "no fold mid-turn"
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

/// An unfinished refresh marker, whatever generation it names, means the
/// agent still stored is the outdated one: adoption clears it in the same
/// transaction that stores a successor. So it is finished fresh too.
#[tokio::test]
async fn a_refresh_stopped_halfway_through_its_generation_change_starts_a_fresh_one() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    harness.set_kv("orchestrator_refresh_unfinished", "1").await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "2").await;
    set_state(&agents, ORCH, "cancelled");
    script_successor(&harness, &agents);

    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id.as_str(), ORCH_2, "a fresh generation");
    let restarts = harness.fake.received_named("restart");
    assert!(
        restarts.is_empty(),
        "restarted from the checkpoint: {restarts:?}"
    );
    let unfinished = harness.kv("orchestrator_refresh_unfinished").await;
    assert!(unfinished.is_none(), "adoption ended the refresh");
}

/// No refresh is owed: a turn an operator message or trigger started after
/// the first look is seen under the switch guard, and nothing is compacted.
#[tokio::test]
async fn a_turn_started_after_the_first_look_stops_the_idle_compaction() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let ended = archon_core::now() - TimeDelta::minutes(50);
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, ended);
    let table = agents.clone();
    let asked = Arc::new(Mutex::new(0));
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |_| {
            let mut asked = asked.lock().unwrap();
            *asked += 1;
            let mut agent = table.lock().unwrap()[ORCH].clone();
            if *asked > 1 {
                agent["status"] = json!({ "state": "running" });
            }
            Scripted::reply("agent", agent)
        }),
    );
    let now = archon_core::now();
    let economy = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(economy, Economy::Nothing);
    let compacts = harness.fake.received_named("compact");
    assert!(compacts.is_empty(), "folded mid-turn: {compacts:?}");
}

/// A creation whose reply was lost is retried under its key with the
/// threshold it first sent, though the config changed meanwhile.
#[tokio::test]
async fn a_retried_creation_sends_the_threshold_it_first_sent() {
    let harness = Harness::new().await;
    harness
        .fake
        .set_features(vec!["compaction_tokens".to_owned()]);
    harness.connect().await;
    let agents = agent_table(&harness);
    harness.fake.script(
        "create_agent",
        Scripted::refuse("unavailable", "reply lost"),
    );
    supervisor::ensure_orchestrator(&harness.ctx)
        .await
        .expect_err("the reply was lost");
    harness.fake.set_features(Vec::new());
    harness.connect().await;
    create(&harness, &agents, ORCH).await;
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 2, "one retry");
    assert_eq!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    assert!(
        creates[0]["idempotency_key"].is_string(),
        "a keyed creation"
    );
    assert_eq!(
        creates[0]["config"], creates[1]["config"],
        "the same request under the same key"
    );
}
