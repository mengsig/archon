//! Healing the `archon` MCP server against the fake aetherd: a failed
//! server is refreshed and the orchestrator restarted, a healthy one is
//! left alone, failed refreshes and restarts back off and reach the
//! operator, and `Status` reports the server's health.

mod link_common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use archon_aether::fake::{self, Scripted};
use archon_core::Timestamp;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, McpHealth, Reply, Request};
use archon_daemon::{handlers, pump, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, agent_id};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";
const ORCH_3: &str = "agt_0003";
const TOOLS: usize = 22;

fn ready() -> Value {
    let tools = vec![json!({"name": "enqueue"}); TOOLS];
    json!({"name": "archon", "state": "ready", "transport": "stdio: archon mcp", "tools": tools})
}

fn failed(error: &str) -> Value {
    json!({"name": "archon", "state": "failed", "transport": "stdio: archon mcp", "error": error})
}

/// Aether's `archon` server: `mcp_servers` reports `now`, and a refresh
/// replaces it with `refreshed`. Tests edit both as the server changes.
struct Server {
    now: Value,
    refreshed: Value,
}

fn install_server(harness: &Harness, now: Value, refreshed: Value) -> Arc<Mutex<Server>> {
    let server = Arc::new(Mutex::new(Server { now, refreshed }));
    let shared = server.clone();
    harness.fake.set_responder(
        "mcp_servers",
        Arc::new(move |cmd| {
            let mut server = shared.lock().unwrap();
            if cmd["refresh"] == json!(true) {
                server.now = server.refreshed.clone();
            }
            Scripted::reply("mcp_servers", json!([server.now.clone()]))
        }),
    );
    server
}

fn orchestrator(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// A running orchestrator `ORCH` that Aether restarts into `ORCH_2`.
async fn running_orchestrator(harness: &Harness) {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    cancels_stop_orchestrators(harness);
    let successor = orchestrator(ORCH_2, "ready");
    harness
        .fake
        .set_default("restart", Scripted::reply("agent", successor));
}

/// Aether accepts a cancel of `ORCH` or `ORCH_2`, running until then, and
/// `get_agent` reports the cancelled one stopped: only that one restarts.
fn cancels_stop_orchestrators(harness: &Harness) {
    let stopped: Arc<Mutex<HashSet<String>>> = Arc::default();
    let known = stopped.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            if !matches!(id, ORCH | ORCH_2) {
                return Scripted::refuse("not_found", "no such agent");
            }
            let cancelled = known.lock().unwrap().contains(id);
            let state = if cancelled { "cancelled" } else { "running" };
            Scripted::reply("agent", orchestrator(id, state))
        }),
    );
    harness.fake.set_responder(
        "cancel",
        Arc::new(move |cmd| {
            let id = cmd["agent"].as_str().unwrap_or_default();
            stopped.lock().unwrap().insert(id.to_owned());
            Scripted::ok()
        }),
    );
}

fn refreshes(harness: &Harness) -> usize {
    let asked = harness.fake.received_named("mcp_servers");
    asked.iter().filter(|c| c["refresh"] == json!(true)).count()
}

fn after(start: Timestamp, secs: i64) -> Timestamp {
    start + chrono::TimeDelta::seconds(secs)
}

#[tokio::test]
async fn a_failed_server_is_refreshed_and_the_orchestrator_restarted() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    install_server(
        &harness,
        failed("write failed: Broken pipe (os error 32)"),
        ready(),
    );
    let now = archon_core::now();

    let health = supervisor::check_mcp(&harness.ctx, now).await.unwrap();

    assert_eq!(health, McpHealth::Ready { tools: 22 });
    assert_eq!(refreshes(&harness), 1);
    assert_eq!(harness.fake.received_named("cancel")[0]["agent"], ORCH);
    assert_eq!(harness.fake.received_named("restart")[0]["agent"], ORCH);
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    assert_eq!(
        *harness.ctx.supervisor.watch().borrow(),
        Some(agent_id(ORCH_2)),
        "the pump follows the restarted orchestrator"
    );
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes[0].contains("Broken pipe") && notes[0].contains("ready (22 tools)"));
    assert!(notes[1].contains("restarted") && notes[1].contains(ORCH_2));
    let status = supervisor::mcp_status(&harness.ctx).await;
    assert_eq!(status.last_heal, Some(now));
    assert_eq!(status.health, McpHealth::Ready { tools: 22 });

    // Ready again: the next check leaves everything alone.
    supervisor::check_mcp(&harness.ctx, after(now, 60))
        .await
        .unwrap();
    assert_eq!(refreshes(&harness), 1);
    assert_eq!(harness.fake.received_named("restart").len(), 1);
}

/// `ORCH` is mid-turn: Aether accepts its cancel, reports it running for
/// `polls` more looks, and refuses to restart it until it has stopped.
fn orchestrator_that_stops_after(harness: &Harness, polls: usize) {
    // `None` until the cancel; then how many looks still find it running.
    let stopping: Arc<Mutex<Option<usize>>> = Arc::default();
    let (looked, cancelled) = (stopping.clone(), stopping.clone());
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            let mut stopping = looked.lock().unwrap();
            let state = match stopping.as_mut().filter(|_| id == ORCH) {
                Some(0) => "cancelled",
                Some(left) => {
                    *left -= 1;
                    "running"
                }
                None => "running",
            };
            Scripted::reply("agent", orchestrator(id, state))
        }),
    );
    harness.fake.set_responder(
        "cancel",
        Arc::new(move |_| {
            cancelled.lock().unwrap().get_or_insert(polls);
            Scripted::ok()
        }),
    );
    harness.fake.set_responder(
        "restart",
        Arc::new(move |_| match *stopping.lock().unwrap() {
            Some(0) => Scripted::reply("agent", orchestrator(ORCH_2, "ready")),
            _ => Scripted::refuse("invalid_argument", "agent is running"),
        }),
    );
}

/// Aether ends a running turn cooperatively and restarts only a stopped
/// agent: asked too early it refuses, which must not cost the conversation.
#[tokio::test]
async fn a_mid_turn_orchestrator_is_restarted_once_its_cancelled_turn_has_stopped() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    orchestrator_that_stops_after(&harness, 2);
    install_server(&harness, failed("Broken pipe"), ready());

    supervisor::check_mcp(&harness.ctx, archon_core::now())
        .await
        .unwrap();

    let restarts = harness.fake.received_named("restart");
    assert_eq!(restarts.len(), 1, "asked once it had stopped: {restarts:?}");
    assert_eq!(restarts[0]["agent"], ORCH);
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2),
        "the same conversation, restarted from its checkpoint"
    );
    assert_eq!(failed_relinks(&harness).await, 0);
}

/// A turn that outlasts the wait leaves the restart owed: it is neither
/// asked for early nor swapped for a fresh generation.
#[tokio::test]
async fn an_orchestrator_that_has_not_stopped_yet_keeps_its_restart_owed() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    orchestrator_that_stops_after(&harness, usize::MAX);
    install_server(&harness, failed("Broken pipe"), ready());

    supervisor::check_mcp(&harness.ctx, archon_core::now())
        .await
        .unwrap();

    assert!(harness.fake.received_named("restart").is_empty());
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert_eq!(failed_relinks(&harness).await, 1, "tried again later");
    let notes = harness.system_notes().await;
    assert!(
        notes.iter().any(|n| n.contains("is still running")),
        "{notes:?}"
    );
}

/// The stored orchestrator may be generations behind the one that runs:
/// the tools go back to the running one, and the ones between are owed.
#[tokio::test]
async fn the_restart_for_the_tools_follows_the_restart_chain_to_its_newest_agent() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let stopped: Arc<Mutex<bool>> = Arc::default();
    let cancelled = stopped.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            let mut agent = match id {
                ORCH => orchestrator(ORCH, "failed"),
                ORCH_2 if *stopped.lock().unwrap() => orchestrator(ORCH_2, "cancelled"),
                _ => orchestrator(id, "running"),
            };
            if id == ORCH {
                agent["restarted_as"] = json!(ORCH_2);
            }
            Scripted::reply("agent", agent)
        }),
    );
    harness.fake.set_responder(
        "cancel",
        Arc::new(move |_| {
            *cancelled.lock().unwrap() = true;
            Scripted::ok()
        }),
    );
    let successor = orchestrator(ORCH_3, "ready");
    harness
        .fake
        .set_default("restart", Scripted::reply("agent", successor));
    install_server(&harness, failed("Broken pipe"), ready());

    supervisor::check_mcp(&harness.ctx, archon_core::now())
        .await
        .unwrap();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], ORCH_2, "the agent that runs");
    assert_eq!(harness.fake.received_named("restart")[0]["agent"], ORCH_2);
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_3)
    );
    let record = harness.kv("orchestrator_retired").await.expect("a record");
    let record: Value = serde_json::from_str(&record).unwrap();
    let owed: Vec<(&Value, &Value)> = record
        .as_array()
        .unwrap()
        .iter()
        .map(|retired| (&retired["agent"], &retired["owed"]))
        .collect();
    assert_eq!(
        owed,
        [
            (&json!(ORCH), &json!(true)),
            (&json!(ORCH_2), &json!(false))
        ]
    );
}

#[tokio::test]
async fn a_healthy_or_unstarted_server_is_left_alone() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    let server = install_server(&harness, ready(), ready());
    let now = archon_core::now();
    let health = supervisor::check_mcp(&harness.ctx, now).await.unwrap();
    assert_eq!(health, McpHealth::Ready { tools: 22 });
    server.lock().unwrap().now = json!({"name": "archon", "state": "unknown", "transport": "x"});
    let health = supervisor::check_mcp(&harness.ctx, after(now, 60))
        .await
        .unwrap();
    assert_eq!(health, McpHealth::Unknown);
    assert_eq!(refreshes(&harness), 0);
    assert!(harness.fake.received_named("cancel").is_empty());
    assert!(harness.fake.received_named("restart").is_empty());
    assert!(harness.system_notes().await.is_empty());
    assert_eq!(supervisor::mcp_status(&harness.ctx).await.last_heal, None);
}

#[tokio::test]
async fn failed_refreshes_back_off_restart_nothing_and_post_one_blocker() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    let server = install_server(&harness, failed("Broken pipe"), failed("spawn failed"));
    let start = archon_core::now();

    let check = |secs| supervisor::check_mcp(&harness.ctx, after(start, secs));
    let health = check(0).await.unwrap();
    assert!(matches!(health, McpHealth::Failed { error } if error == "spawn failed"));
    assert_eq!(refreshes(&harness), 1);
    check(5).await.unwrap();
    assert_eq!(refreshes(&harness), 1, "the next refresh waits 10s");
    check(10).await.unwrap();
    assert_eq!(refreshes(&harness), 2);
    check(30).await.unwrap();
    assert_eq!(refreshes(&harness), 2, "then 30s");
    check(40).await.unwrap();
    assert_eq!(refreshes(&harness), 3);
    assert!(harness.fake.received_named("cancel").is_empty());
    assert!(harness.fake.received_named("restart").is_empty());
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(blockers[0].contains("3 refreshes failed") && blockers[0].contains("spawn failed"));
    let status = supervisor::mcp_status(&harness.ctx).await;
    assert_eq!((status.failed_refreshes, status.last_heal), (3, None));

    // The fourth refresh works: the orchestrator gets the tools back.
    server.lock().unwrap().refreshed = ready();
    check(40 + 120).await.unwrap();
    assert_eq!(refreshes(&harness), 4);
    assert_eq!(harness.fake.received_named("restart").len(), 1);
    assert_eq!(
        harness.notifications(NotificationKind::Blocker).await.len(),
        1
    );
    let notes = harness.system_notes().await;
    assert!(
        notes
            .iter()
            .any(|n| n == "The archon MCP server is ready again.")
    );
    assert_eq!(
        supervisor::mcp_status(&harness.ctx).await.failed_refreshes,
        0
    );
}

/// A server that failed and refreshes fine, and an orchestrator whose
/// cancel Aether refuses, so each restart for the tools fails.
async fn unrestartable_orchestrator(harness: &Harness) -> Arc<Mutex<Server>> {
    running_orchestrator(harness).await;
    harness
        .fake
        .set_default("cancel", Scripted::refuse("internal", "cancel is broken"));
    install_server(harness, failed("Broken pipe"), ready())
}

async fn failed_relinks(harness: &Harness) -> u32 {
    supervisor::mcp_status(&harness.ctx).await.failed_relinks
}

#[tokio::test]
async fn failed_restarts_back_off_to_the_cap_and_post_one_blocker() {
    let harness = Harness::new().await;
    harness.connect().await;
    unrestartable_orchestrator(&harness).await;
    let start = archon_core::now();
    let check = |secs| supervisor::check_mcp(&harness.ctx, after(start, secs));
    let cancels = || harness.fake.received_named("cancel").len();

    let health = check(0).await.unwrap();
    assert_eq!(health, McpHealth::Ready { tools: 22 });
    assert_eq!((refreshes(&harness), cancels()), (1, 1));
    assert_eq!(failed_relinks(&harness).await, 1);
    // Tries at 0, then after 10s, 30s, 2m, 5m, and every 10m from then on.
    let mut at = 0;
    for pause in [10, 30, 120, 300, 600, 600, 600] {
        let tried = cancels();
        check(at + pause - 1).await.unwrap();
        assert_eq!(cancels(), tried, "the restart after {at}s waits {pause}s");
        at += pause;
        check(at).await.unwrap();
        assert_eq!(cancels(), tried + 1, "a restart is due at {at}s");
    }
    assert_eq!(failed_relinks(&harness).await, 8);
    assert_eq!(refreshes(&harness), 1, "the server stayed ready");
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(blockers[0].contains("3 restarts failed") && blockers[0].contains("cancel is broken"));
    assert!(
        blockers[0]
            .contains("`archon aether mcp refresh archon`, then `archon orchestrator restart`")
    );
    let status = supervisor::mcp_status(&harness.ctx).await;
    assert_eq!(status.health, McpHealth::Ready { tools: 22 });
    assert_eq!(status.failed_refreshes, 0);
}

#[tokio::test]
async fn a_restart_that_works_resets_the_backoff_and_the_blocker() {
    let harness = Harness::new().await;
    harness.connect().await;
    let server = unrestartable_orchestrator(&harness).await;
    let start = archon_core::now();
    let check = |secs| supervisor::check_mcp(&harness.ctx, after(start, secs));
    for secs in [0, 10, 40] {
        check(secs).await.unwrap();
    }
    assert_eq!(failed_relinks(&harness).await, 3);
    assert_eq!(
        harness.notifications(NotificationKind::Blocker).await.len(),
        1
    );

    cancels_stop_orchestrators(&harness);
    check(40 + 120).await.unwrap();
    assert_eq!(harness.fake.received_named("restart").len(), 1);
    assert_eq!(failed_relinks(&harness).await, 0);
    let notes = harness.system_notes().await;
    assert!(
        notes
            .iter()
            .any(|n| n == "The orchestrator has Archon's tools again.")
    );

    // A later failure starts a fresh streak: refreshed and restarted at once.
    harness
        .fake
        .set_default("cancel", Scripted::refuse("internal", "cancel is broken"));
    server.lock().unwrap().now = failed("Broken pipe");
    let cancels = harness.fake.received_named("cancel").len();
    check(40 + 120 + 600).await.unwrap();
    assert_eq!(refreshes(&harness), 2);
    assert_eq!(harness.fake.received_named("cancel").len(), cancels + 1);
    assert_eq!(failed_relinks(&harness).await, 1);
    for secs in [770, 800] {
        check(secs).await.unwrap();
    }
    assert_eq!(failed_relinks(&harness).await, 3);
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(
        blockers.len(),
        2,
        "the new streak is told again: {blockers:?}"
    );
}

#[tokio::test]
async fn a_failed_refresh_never_restarts_the_orchestrator() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    install_server(&harness, failed("Broken pipe"), failed("spawn failed"));
    let start = archon_core::now();
    for secs in [0, 60, 120, 600, 1200, 1800] {
        let health = supervisor::check_mcp(&harness.ctx, after(start, secs))
            .await
            .unwrap();
        assert!(matches!(health, McpHealth::Failed { .. }));
    }
    assert_eq!(refreshes(&harness), 6, "refreshes keep trying, paced");
    assert!(harness.fake.received_named("cancel").is_empty());
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(failed_relinks(&harness).await, 0);
}

#[tokio::test]
async fn a_successor_adopted_while_the_server_was_down_is_restarted_once_it_is_ready() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    let server = install_server(&harness, failed("Broken pipe"), failed("spawn failed"));
    let start = archon_core::now();
    supervisor::check_mcp(&harness.ctx, start).await.unwrap();

    // Aether restarts the orchestrator, and the operator refreshes by hand.
    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id(ORCH_2))
        .await
        .unwrap();
    server.lock().unwrap().now = ready();
    harness.fake.set_default(
        "restart",
        Scripted::reply("agent", orchestrator(ORCH_3, "ready")),
    );
    let health = supervisor::check_mcp(&harness.ctx, after(start, 60))
        .await
        .unwrap();

    assert_eq!(health, McpHealth::Ready { tools: 22 });
    assert_eq!(refreshes(&harness), 1, "the operator's refresh sufficed");
    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], ORCH_2);
    assert_eq!(harness.fake.received_named("restart")[0]["agent"], ORCH_2);
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_3)
    );

    // Restarted once: the next check leaves the new orchestrator alone.
    supervisor::check_mcp(&harness.ctx, after(start, 120))
        .await
        .unwrap();
    assert_eq!(harness.fake.received_named("restart").len(), 1);
}

#[tokio::test]
async fn a_successor_adopted_while_the_server_was_ready_owes_no_restart() {
    let harness = Harness::new().await;
    harness.connect().await;
    running_orchestrator(&harness).await;
    install_server(&harness, ready(), ready());
    let start = archon_core::now();
    supervisor::check_mcp(&harness.ctx, start).await.unwrap();

    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id(ORCH_2))
        .await
        .unwrap();
    supervisor::check_mcp(&harness.ctx, after(start, 60))
        .await
        .unwrap();

    assert!(harness.fake.received_named("cancel").is_empty());
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
}

#[tokio::test]
async fn a_failed_restart_streak_whose_orchestrator_is_gone_says_so() {
    let harness = Harness::new().await;
    harness.connect().await;
    unrestartable_orchestrator(&harness).await;
    let start = archon_core::now();
    let check = |secs| supervisor::check_mcp(&harness.ctx, after(start, secs));
    for secs in [0, 10, 40] {
        check(secs).await.unwrap();
    }
    assert_eq!(failed_relinks(&harness).await, 3);

    // The stored orchestrator goes away, so the next relink has none.
    let removed = harness
        .ctx
        .store
        .lock()
        .await
        .kv_delete(keys::ORCHESTRATOR_AGENT);
    removed.unwrap();
    let cancels = harness.fake.received_named("cancel").len();
    check(40 + 120).await.unwrap();

    assert_eq!(harness.fake.received_named("cancel").len(), cancels);
    assert_eq!(failed_relinks(&harness).await, 0);
    let notes = harness.system_notes().await;
    assert!(
        !notes
            .iter()
            .any(|n| n == "The orchestrator has Archon's tools again."),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("is gone")
            && n.contains("the next one starts with Archon's tools")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn the_operators_restart_ends_a_failed_restart_streak() {
    let harness = Harness::new().await;
    harness.connect().await;
    unrestartable_orchestrator(&harness).await;
    let start = archon_core::now();
    supervisor::check_mcp(&harness.ctx, start).await.unwrap();
    assert_eq!(failed_relinks(&harness).await, 1);

    harness.fake.set_default("cancel", Scripted::ok());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", orchestrator(ORCH_3, "ready")),
    );
    let fresh = supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(fresh, agent_id(ORCH_3));
    assert_eq!(failed_relinks(&harness).await, 0);

    let cancels = harness.fake.received_named("cancel").len();
    supervisor::check_mcp(&harness.ctx, after(start, 60))
        .await
        .unwrap();
    assert_eq!(harness.fake.received_named("cancel").len(), cancels);
    assert!(harness.fake.received_named("restart").is_empty());
}

#[tokio::test]
async fn a_server_healed_before_any_orchestrator_exists_restarts_nothing() {
    let harness = Harness::new().await;
    harness.connect().await;
    install_server(&harness, failed("Broken pipe"), ready());
    let health = supervisor::check_mcp(&harness.ctx, archon_core::now())
        .await
        .unwrap();
    assert_eq!(health, McpHealth::Ready { tools: 22 });
    assert!(harness.fake.received_named("cancel").is_empty());
    assert!(harness.fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn the_pump_checks_the_server_as_soon_as_it_connects() {
    let harness = Harness::new().await;
    running_orchestrator(&harness).await;
    install_server(&harness, failed("Broken pipe"), ready());
    let pump = tokio::spawn(pump::run(
        harness.ctx.clone(),
        harness.fake.endpoint(),
        None,
    ));
    harness
        .eventually("the orchestrator was restarted", || async {
            harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref() == Some(ORCH_2)
        })
        .await;
    pump.abort();
    assert_eq!(refreshes(&harness), 1);
}

#[tokio::test]
async fn status_reports_the_mcp_servers_health() {
    let harness = Harness::new().await;
    let status = |harness: &Harness| {
        let ctx = harness.ctx.clone();
        async move {
            let reply = handlers::handle(
                &ctx,
                ClientKind::ArchonCli,
                Request::Status(Default::default()),
            )
            .await
            .unwrap();
            let Reply::Status(status) = reply else {
                panic!("expected a status, got {reply:?}");
            };
            status.mcp.expect("archond reports the MCP server")
        }
    };
    let offline = status(&harness).await;
    assert!(matches!(offline.health, McpHealth::Unreachable { .. }));
    harness.connect().await;
    install_server(&harness, failed("Broken pipe"), ready());
    let failing = status(&harness).await;
    let error = "Broken pipe".to_owned();
    assert_eq!(failing.health, McpHealth::Failed { error });
    assert_eq!(refreshes(&harness), 0, "status only looks");
}
