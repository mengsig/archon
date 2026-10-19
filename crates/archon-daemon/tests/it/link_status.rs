//! `Status` against the fake aetherd: Aether's sections come from answers
//! archond asks for off the request path, so a silent aetherd holds status
//! up by at most a short wait, and each section says how old it is.

use crate::link_common;

use std::time::{Duration, Instant};

use archon_aether::fake::{self, Scripted};
use archon_core::protocol::{ClientKind, DaemonStatus, McpHealth, Reply, Request};
use archon_daemon::handlers;
use archon_queue::store::keys;
use link_common::Harness;
use serde_json::json;

const ORCH: &str = "agt_0001";
/// How long a status may take while aetherd never answers.
const BOUND: Duration = Duration::from_secs(2);

async fn status(harness: &Harness) -> (DaemonStatus, Duration) {
    let started = Instant::now();
    let request = Request::Status(Default::default());
    let reply = handlers::handle(&harness.ctx, ClientKind::ArchonMcp, request);
    let reply = tokio::time::timeout(Duration::from_secs(5), reply)
        .await
        .expect("status answers while aetherd is silent")
        .unwrap();
    let Reply::Status(status) = reply else {
        panic!("expected a status, got {reply:?}");
    };
    (*status, started.elapsed())
}

async fn connected_with_orchestrator() -> Harness {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness
}

fn answering(harness: &Harness) {
    let agent = Scripted::reply("agent", fake::agent(ORCH, None));
    harness.fake.set_default("get_agent", agent);
    let server = json!({"name": "archon", "state": "ready", "transport": "stdio", "tools": []});
    let servers = Scripted::reply("mcp_servers", json!([server]));
    harness.fake.set_default("mcp_servers", servers);
}

fn silent(harness: &Harness) {
    for cmd in ["get_agent", "mcp_servers", "ping"] {
        harness.fake.set_default(cmd, Scripted::Silent);
    }
}

#[tokio::test]
async fn a_silent_aetherd_leaves_status_prompt_and_its_sections_stale_with_their_age() {
    let harness = connected_with_orchestrator().await;
    answering(&harness);
    let (fresh, _) = status(&harness).await;
    let health = fresh.health.expect("archond reports how fresh Aether is");
    assert!(!health.aether.stale && !health.mcp.stale, "{health:?}");
    let answered = health.aether.as_of.expect("Aether answered");
    assert_eq!(fresh.orchestrator.unwrap().status, "ready");

    silent(&harness);
    let (stale, took) = status(&harness).await;
    assert!(
        took < BOUND,
        "status took {took:?} against a silent aetherd"
    );
    let health = stale.health.expect("health");
    assert!(health.aether.stale && health.mcp.stale, "{health:?}");
    assert_eq!(
        health.aether.as_of,
        Some(answered),
        "the last answer's time"
    );
    assert!(health.aether.age_secs(archon_core::now()).is_some());
    assert_eq!(health.aether.last_error, None, "the ask is still running");
    let described = health.stale(archon_core::now()).join("; ");
    assert!(described.contains("aether: last answer"), "{described}");
    assert_eq!(
        stale.orchestrator.unwrap().status,
        "ready",
        "the last answer stands"
    );
    let mcp = stale.mcp.unwrap().health;
    assert!(matches!(mcp, McpHealth::Ready { .. }), "{mcp:?}");

    let (_, took) = status(&harness).await;
    assert!(
        took < BOUND,
        "status took {took:?} against a silent aetherd"
    );
    let asked = harness.fake.received_named("get_agent").len();
    assert_eq!(asked, 2, "a status joins the ask still running");
}

#[tokio::test]
async fn a_status_before_aether_ever_answers_says_so_within_the_bound() {
    let harness = connected_with_orchestrator().await;
    silent(&harness);
    let (status, took) = status(&harness).await;
    assert!(
        took < BOUND,
        "status took {took:?} against a silent aetherd"
    );
    let health = status.health.expect("health");
    assert_eq!(health.aether.as_of, None);
    assert!(health.aether.stale && health.mcp.stale, "{health:?}");
    let orchestrator = status.orchestrator.unwrap();
    assert_eq!(orchestrator.status, "unknown (Aether has not answered yet)");
    let mcp = status.mcp.unwrap().health;
    assert!(matches!(mcp, McpHealth::Unreachable { .. }), "{mcp:?}");
}

#[tokio::test]
async fn a_failed_ask_names_its_error_and_keeps_the_last_answers_time() {
    let harness = connected_with_orchestrator().await;
    answering(&harness);
    let (fresh, _) = status(&harness).await;
    let answered = fresh.health.unwrap().aether.as_of.expect("answered");

    harness
        .fake
        .script("get_agent", Scripted::refuse("internal", "store locked"));
    let (failed, _) = status(&harness).await;
    let aether = failed.health.unwrap().aether;
    assert!(aether.stale, "{aether:?}");
    assert_eq!(aether.as_of, Some(answered));
    let error = aether.last_error.expect("the failure is named");
    assert!(error.contains("store locked"), "{error}");
    let shown = failed.orchestrator.unwrap().status;
    assert!(
        shown.starts_with("unknown (") && shown.contains("store locked"),
        "{shown}"
    );

    let (healed, _) = status(&harness).await;
    let aether = healed.health.unwrap().aether;
    assert!(!aether.stale && aether.last_error.is_none(), "{aether:?}");
    assert!(aether.as_of >= Some(answered), "a fresh answer");
}

#[tokio::test]
async fn a_down_link_marks_every_section_failed_without_asking() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let (status, took) = status(&harness).await;
    assert!(took < BOUND, "status took {took:?} with no link");
    let health = status.health.expect("health");
    for section in [&health.aether, &health.mcp] {
        assert!(section.stale && section.last_error.is_some(), "{section:?}");
    }
    assert!(
        health.github.is_some(),
        "a dedicated Aether's watcher is read"
    );
    assert!(harness.fake.received_named("get_agent").is_empty());
}
