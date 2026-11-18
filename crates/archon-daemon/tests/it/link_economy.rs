//! The orchestrator's context economy against a fake aetherd: the
//! compaction threshold sent only to an Aether that serves it, one refresh of
//! an outdated generation at an idle moment, and one idle compaction.

use crate::link_common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::Timestamp;
use archon_core::id::QueueItemId;
use archon_daemon::supervisor::{self, Economy};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_queue::store::keys;
use chrono::TimeDelta;
use link_common::Harness;
use serde_json::{Value, json};

pub(crate) const ORCH: &str = "agt_0001";
pub(crate) const ORCH_2: &str = "agt_0002";
/// `kv`: what the newest generation was created with.
const ORIGIN_KEY: &str = "orchestrator_origin";

pub(crate) type Agents = Arc<Mutex<HashMap<String, Value>>>;

pub(crate) fn orchestrator(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// `get_agent` answers from a table the test edits; `cancel` stops the agent.
pub(crate) fn agent_table(harness: &Harness) -> Agents {
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
pub(crate) fn cancel_stops(harness: &Harness, agents: &Agents) {
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

pub(crate) async fn create(harness: &Harness, agents: &Agents, id: &str) {
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
pub(crate) fn set_state(agents: &Agents, id: &str, state: &str) {
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

pub(crate) fn script_successor(harness: &Harness, agents: &Agents) {
    agents
        .lock()
        .unwrap()
        .insert(ORCH_2.into(), orchestrator(ORCH_2, "ready"));
    let successor = Scripted::reply("agent", orchestrator(ORCH_2, "ready"));
    harness.fake.script("create_agent", successor);
}

pub(crate) fn set_stats(agents: &Agents, id: &str, tokens: u64, turn_ended_at: Timestamp) {
    let mut table = agents.lock().unwrap();
    let agent = table.get_mut(id).expect("a known agent");
    agent["stats"] = json!({ "context_tokens": tokens, "last_turn_at": turn_ended_at });
}

pub(crate) async fn as_if_created_by_an_older_build(harness: &Harness) {
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
    let line = status_line(&harness).await;
    assert!(
        line.contains("upgrade refresh (prompt) delayed: 4 attempts failed, the last with:"),
        "status says the upgrade is delayed: {line}"
    );
    assert!(line.contains("busy"), "and why: {line}");
    assert!(!line.contains("prompt outdated"), "{line}");

    cancel_stops(&harness, &agents);
    script_successor(&harness, &agents);
    let later = start + TimeDelta::minutes(40);
    let recovered = supervisor::economize(&harness.ctx, later).await.unwrap();
    assert_eq!(recovered, Economy::Refreshed, "never given up for good");
    let current = supervisor::current_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(current.unwrap().as_str(), ORCH_2);
    let line = status_line(&harness).await;
    assert!(
        line.ends_with("; up to date"),
        "nothing is owed now: {line}"
    );
}

/// How `archon status` describes the orchestrator's generation.
async fn status_line(harness: &Harness) -> String {
    let info = supervisor::orchestrator_info(&harness.ctx).await.unwrap();
    info.expect("an orchestrator").describe_freshness()
}

/// An orchestrator that is never idle delays the refresh without a failure:
/// status says so from the first moment, never a bare "prompt outdated".
#[tokio::test]
async fn status_says_why_an_owed_refresh_waits_and_keeps_the_old_words_when_it_is_off() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "running");
    let now = archon_core::now();
    let busy = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(busy, Economy::Nothing);
    let line = status_line(&harness).await;
    let waits = "upgrade refresh (prompt) delayed: it waits until the orchestrator is idle";
    assert!(line.contains(waits), "{line}");
    assert!(!line.contains("prompt outdated"), "{line}");

    let mut config = archon_core::config::Config::default();
    config.orchestrator.refresh_on_upgrade = false;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    let line = status_line(&harness).await;
    assert!(
        line.contains("; prompt outdated"),
        "no refresh comes: {line}"
    );
    assert!(!line.contains("delayed"), "{line}");
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

/// Aether 1.4.1 was sent no threshold: that absence is frozen with the
/// creation key before the request, and a retry repeats it though the
/// Aether connected by then would take one.
#[tokio::test]
async fn a_retried_creation_that_first_sent_no_threshold_sends_none_again() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    harness.fake.script(
        "create_agent",
        Scripted::refuse("unavailable", "reply lost"),
    );
    supervisor::ensure_orchestrator(&harness.ctx)
        .await
        .expect_err("the reply was lost");
    let first = &harness.fake.received_named("create_agent")[0];
    let raw = harness.kv("orchestrator_compaction_tokens").await;
    let frozen: Value = serde_json::from_str(&raw.expect("frozen before the request")).unwrap();
    assert_eq!(frozen["key"], first["idempotency_key"], "with its key");
    assert!(frozen["tokens"].is_null(), "an explicit absence: {frozen}");

    harness
        .fake
        .set_features(vec!["compaction_tokens".to_owned()]);
    harness.connect().await;
    create(&harness, &agents, ORCH).await;
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 2, "one retry");
    assert_eq!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    let resent = &creates[1]["config"]["compaction_tokens"];
    assert!(resent.is_null(), "the retry changed the request: {resent}");
}

/// A threshold frozen for another creation is never applied to this one.
#[tokio::test]
async fn a_threshold_frozen_under_another_key_is_not_reused() {
    let harness = Harness::new().await;
    harness
        .fake
        .set_features(vec!["compaction_tokens".to_owned()]);
    harness.connect().await;
    let agents = agent_table(&harness);
    let other = json!({ "key": "archon-orchestrator-other-9", "tokens": 120_000 });
    harness
        .set_kv("orchestrator_compaction_tokens", &other.to_string())
        .await;
    create(&harness, &agents, ORCH).await;
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates[0]["config"]["compaction_tokens"], json!(160_000));
    let raw = harness.kv("orchestrator_compaction_tokens").await.unwrap();
    let frozen: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(frozen["key"], creates[0]["idempotency_key"]);
}

/// The idle compaction takes the switch guard a Say or trigger holds while
/// it starts a turn, and reads the orchestrator again once it has it.
#[tokio::test]
async fn an_idle_compaction_waits_out_a_message_being_sent_and_then_sees_its_turn() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let now = archon_core::now();
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, now - TimeDelta::minutes(50));

    let sending = supervisor::hold_switch(&harness.ctx).await;
    let looked = harness.fake.received_named("get_agent").len();
    let ctx = harness.ctx.clone();
    let check = tokio::spawn(async move { supervisor::economize(&ctx, now).await });
    harness
        .eventually("the first look at the idle orchestrator", || async {
            harness.fake.received_named("get_agent").len() > looked
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let early = harness.fake.received_named("compact");
    assert!(early.is_empty(), "compacted beside the send: {early:?}");

    set_state(&agents, ORCH, "running");
    drop(sending);
    let economy = check.await.unwrap().unwrap();
    assert_eq!(economy, Economy::Nothing, "the message started a turn");
    let compacts = harness.fake.received_named("compact");
    assert!(compacts.is_empty(), "folded mid-turn: {compacts:?}");
}

/// News due to wake the orchestrator is about to start a turn: the idle
/// compaction, checked again under the switch guard, leaves it alone.
#[tokio::test]
async fn news_due_stops_the_idle_compaction() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let now = archon_core::now() + TimeDelta::minutes(1);
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, now - TimeDelta::minutes(50));
    let news = TriggerEvent::ItemBlocked {
        item: QueueItemId::new(),
        reason: "stuck".into(),
    };
    triggers::push(&harness.ctx, news).await.unwrap();
    let due = supervisor::economize(&harness.ctx, now).await.unwrap();
    assert_eq!(due, Economy::Nothing, "the news goes first");
    let compacts = harness.fake.received_named("compact");
    assert!(compacts.is_empty(), "folded as a wake began: {compacts:?}");
}

/// Hold one piece of news that wakes the orchestrator from `due_at` on.
async fn hold_news_until(harness: &Harness, due_at: Timestamp) {
    let news = TriggerEvent::ItemBlocked {
        item: QueueItemId::new(),
        reason: "stuck".into(),
    };
    triggers::push_quiet(&harness.ctx, news).await.unwrap();
    let raw = harness.kv("trigger_outbox").await.expect("held news");
    let mut outbox: Value = serde_json::from_str(&raw).unwrap();
    let held = outbox["pending"][0].as_object_mut().expect("the news");
    held.remove("quiet");
    held.insert("defer_until".into(), json!(due_at));
    harness.set_kv("trigger_outbox", &outbox.to_string()).await;
}

/// Answer `get_agent` from `agents`, keeping the `nth` lookup from now
/// waiting until the returned sender is used. For a multi-thread runtime.
fn hold_lookup(harness: &Harness, agents: &Agents, nth: usize) -> std::sync::mpsc::Sender<()> {
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = Mutex::new(held);
    let table = agents.clone();
    let lookups = AtomicUsize::new(0);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            if lookups.fetch_add(1, Ordering::SeqCst) + 1 == nth {
                let wait = || held.lock().unwrap().recv_timeout(Duration::from_secs(10));
                tokio::task::block_in_place(wait).expect("the test releases the lookup");
            }
            let id = cmd["id"].as_str().unwrap_or_default();
            match table.lock().unwrap().get(id) {
                Some(agent) => Scripted::reply("agent", agent.clone()),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    release
}

/// One check at `now` whose recheck under the switch guard has looked the
/// orchestrator up before news is due at `due_at`, and then waits for the
/// store until after it.
async fn check_as_news_comes_due(
    harness: &Harness,
    agents: &Agents,
    now: Timestamp,
    due_at: Timestamp,
) -> Economy {
    hold_news_until(harness, due_at).await;
    let looked = harness.fake.received_named("get_agent").len();
    // The check's first look, then the recheck's under the guard.
    let release = hold_lookup(harness, agents, 2);
    let ctx = harness.ctx.clone();
    let check = tokio::spawn(async move { supervisor::economize(&ctx, now).await });
    harness
        .eventually("the recheck looks the orchestrator up", || async {
            harness.fake.received_named("get_agent").len() >= looked + 2
        })
        .await;
    let seen = tokio::time::Instant::now();
    let store = harness.ctx.store.lock().await;
    release.send(()).expect("the lookup is waiting");
    wait_past(seen, now, due_at).await;
    drop(store);
    check.await.unwrap().unwrap()
}

/// Sleep until a check that began at `now`, and whose clock was running by
/// `seen`, counts the time as past `due_at`, however late it started.
async fn wait_past(seen: tokio::time::Instant, now: Timestamp, due_at: Timestamp) {
    let until_due = (due_at - now).to_std().expect("due after the check began");
    tokio::time::sleep_until(seen + until_due + Duration::from_millis(200)).await;
}

/// News that comes due after the refresh's recheck looked the orchestrator
/// up, while it waits for the store: the news is checked at the time it is
/// once the store is held, where the attempt is recorded, so it is seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn news_that_comes_due_while_the_recheck_waits_for_the_store_stops_the_refresh() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    let now = archon_core::now();
    let due_at = now + TimeDelta::milliseconds(1500);

    let economy = check_as_news_comes_due(&harness, &agents, now, due_at).await;
    assert_eq!(economy, Economy::Nothing, "the news came due meanwhile");
    let cancels = harness.fake.received_named("cancel");
    assert!(cancels.is_empty(), "refreshed as a wake began: {cancels:?}");
    let begun = harness.kv("orchestrator_refresh_unfinished").await;
    assert_eq!(begun, None, "no refresh began");
}

/// The same wait in the idle compaction: nothing is marked done or folded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn news_that_comes_due_while_the_recheck_waits_for_the_store_stops_the_compaction() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let now = archon_core::now();
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, now - TimeDelta::minutes(50));
    let due_at = now + TimeDelta::milliseconds(1500);

    let economy = check_as_news_comes_due(&harness, &agents, now, due_at).await;
    assert_eq!(economy, Economy::Nothing, "the news came due meanwhile");
    let compacts = harness.fake.received_named("compact");
    assert!(compacts.is_empty(), "folded as a wake began: {compacts:?}");
    let marked = harness.kv("orchestrator_idle_compaction").await;
    assert_eq!(marked, None, "the idle stretch is still owed its fold");
}

/// News that comes due while the idle compaction waits for the switch guard
/// is seen by its recheck, which reads the clock again under the guard.
#[tokio::test]
async fn news_that_comes_due_while_the_compaction_waits_stops_it() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    harness.fake.set_default("compact", Scripted::ok());
    let now = archon_core::now();
    set_state(&agents, ORCH, "waiting");
    set_stats(&agents, ORCH, 150_000, now - TimeDelta::minutes(50));
    let due_at = now + TimeDelta::milliseconds(400);
    hold_news_until(&harness, due_at).await;

    let sending = supervisor::hold_switch(&harness.ctx).await;
    let looked = harness.fake.received_named("get_agent").len();
    let ctx = harness.ctx.clone();
    let check = tokio::spawn(async move { supervisor::economize(&ctx, now).await });
    harness
        .eventually("the first look, before the news is due", || async {
            harness.fake.received_named("get_agent").len() > looked
        })
        .await;
    wait_past(tokio::time::Instant::now(), now, due_at).await;
    drop(sending);
    let economy = check.await.unwrap().unwrap();
    assert_eq!(economy, Economy::Nothing, "the news came due meanwhile");
    let compacts = harness.fake.received_named("compact");
    assert!(compacts.is_empty(), "folded as a wake began: {compacts:?}");
}

/// The release before froze nothing: a creation it sent and never adopted
/// is retried under its key with no threshold, as it was first sent.
#[tokio::test]
async fn a_creation_an_older_build_began_is_retried_without_a_threshold() {
    let harness = Harness::new().await;
    harness
        .fake
        .set_features(vec!["compaction_tokens".to_owned()]);
    harness.connect().await;
    let agents = agent_table(&harness);
    let begun = json!({
        "generation": 1,
        "archon_version": "1.6.3",
        "prompt": "",
        "tools": "0000000000000000",
        "lessons": [],
    });
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    harness.set_kv(ORIGIN_KEY, &begun.to_string()).await;
    assert_eq!(harness.kv("orchestrator_compaction_tokens").await, None);

    create(&harness, &agents, ORCH).await;
    let creates = harness.fake.received_named("create_agent");
    let sent = &creates[0]["config"]["compaction_tokens"];
    assert!(sent.is_null(), "the retry added a threshold: {sent}");
    let raw = harness.kv("orchestrator_compaction_tokens").await.unwrap();
    let frozen: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(frozen["key"], creates[0]["idempotency_key"]);
    assert!(frozen["tokens"].is_null(), "an explicit absence: {frozen}");
}

/// What `status` answers the CLI about the orchestrator.
pub(crate) async fn cli_status(harness: &Harness) -> archon_core::protocol::DaemonStatus {
    use archon_core::protocol::{ClientKind, Reply, Request};
    let request = Request::Status(Default::default());
    match archon_daemon::handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await {
        Ok(Reply::Status(status)) => *status,
        other => panic!("status replied {other:?}"),
    }
}

/// The refresh stopped the old generation and every creation fails: no
/// orchestrator is recorded, and status still shows the delayed upgrade with
/// its latest failure until the successor starts.
#[tokio::test]
async fn status_shows_a_refresh_whose_successor_cannot_start_and_why() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    assert_eq!(
        cli_status(&harness).await.orchestrator_refresh_delayed,
        None
    );
    as_if_created_by_an_older_build(&harness).await;
    set_state(&agents, ORCH, "waiting");
    let busy = Scripted::refuse("internal", "busy");
    harness.fake.set_default("create_agent", busy);
    let now = archon_core::now();
    let failed = supervisor::economize(&harness.ctx, now).await;
    failed.expect_err("the successor was not created");
    let status = cli_status(&harness).await;
    assert!(status.orchestrator.is_none(), "the old one was stopped");
    let delayed = status.orchestrator_refresh_delayed.expect("the delay");
    assert!(delayed.contains("busy"), "{delayed}");

    let down = Scripted::refuse("internal", "still down");
    harness.fake.set_default("create_agent", down);
    let again = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(1)).await;
    again.expect_err("the next check failed too");
    let delayed = cli_status(&harness).await.orchestrator_refresh_delayed;
    let delayed = delayed.expect("still delayed");
    assert!(delayed.contains("still down"), "the latest: {delayed}");

    agents
        .lock()
        .unwrap()
        .insert(ORCH_2.into(), orchestrator(ORCH_2, "ready"));
    let successor = Scripted::reply("agent", orchestrator(ORCH_2, "ready"));
    harness.fake.set_default("create_agent", successor);
    let next = supervisor::economize(&harness.ctx, now + TimeDelta::minutes(2));
    assert_eq!(next.await.unwrap(), Economy::Refreshed);
    let status = cli_status(&harness).await;
    assert_eq!(
        status.orchestrator.map(|info| info.agent.to_string()),
        Some(ORCH_2.to_owned())
    );
    assert_eq!(status.orchestrator_refresh_delayed, None);
}

/// The counter names a generation the stored agent was not created as: its
/// creation is not recorded under it, so `status` has no freshness to carry
/// the owed refresh and says beside the orchestrator why it waits.
#[tokio::test]
async fn status_shows_a_refresh_owed_by_records_that_disagree() {
    let harness = Harness::new().await;
    harness.connect().await;
    let agents = agent_table(&harness);
    create(&harness, &agents, ORCH).await;
    set_state(&agents, ORCH, "running");
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "2").await;

    let status = cli_status(&harness).await;
    let info = status.orchestrator.expect("the orchestrator still runs");
    assert_eq!(info.agent.to_string(), ORCH);
    assert_eq!(info.freshness, None, "generation 2 was never recorded");
    let delayed = status.orchestrator_refresh_delayed.expect("the delay");
    assert!(delayed.contains("idle"), "what it waits for: {delayed}");
}
