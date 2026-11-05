//! A hung orchestrator turn recovered end to end against the fake aetherd
//! (#393): the pump feeds the stall watch, archond nudges, then interrupts,
//! and the operator mail queued behind the hung turn is answered in a fresh one.

use crate::link_common;

use std::sync::Arc;

use archon_aether::fake::{self, Scripted};
use archon_core::error::ArchonError;
use archon_core::id::FeedItemId;
use archon_core::notify::NotificationKind;
use archon_daemon::supervisor;
use archon_queue::store::keys;
use chrono::TimeDelta;
use link_common::{Harness, spawn_link};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const ORCH: &str = "agt_0001";

/// A pump following `ORCH`, a running orchestrator, with both streams up.
async fn running_pump(harness: &Harness) -> JoinHandle<Result<(), ArchonError>> {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut agent = fake::agent(ORCH, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": "running" });
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |_| Scripted::reply("agent", agent.clone())),
    );
    harness
        .fake
        .set_default("send_message", Scripted::reply("message_sent", json!({})));
    let pump = spawn_link(harness, harness.fake.endpoint(), None);
    harness
        .eventually("both streams subscribed", || async {
            harness.fake.live_subscriptions() == 2
        })
        .await;
    pump
}

/// Push an orchestrator event and wait until the pump handled it.
async fn orch(harness: &Harness, kind: Value) {
    let seq = harness.fake.push_event(fake::event(Some(ORCH), None, kind));
    harness
        .eventually("the pump handled the event", || async {
            let saved = harness.kv(keys::CURSOR_ORCHESTRATOR).await;
            saved
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|s| s >= seq)
        })
        .await;
}

fn delivered(seq: u64, kind: Value) -> Value {
    json!({"type": "inbox.delivered", "item": {
        "id": format!("inb_{seq}"), "agent_id": ORCH, "seq": seq,
        "delivered_at": "2026-11-05T13:00:00Z", "kind": kind}})
}

/// The operator's chat message, delivered as inbox item `seq`.
async fn say(harness: &Harness, seq: u64, text: &str) {
    let item = FeedItemId::new();
    supervisor::deliver_operator_message(&harness.ctx, &item, text, &[])
        .await
        .unwrap();
    let message = json!({"kind": "message", "message_id": format!("msg_{seq}"),
                         "from": {"actor": "user"},
                         "content": [{"type": "text", "text": text}]});
    orch(harness, delivered(seq, message)).await;
}

fn started() -> Value {
    json!({"type": "agent.started", "turn": 7})
}

fn completed() -> Value {
    json!({"type": "agent.turn_completed", "turn": 7, "invocation": "inv_7", "next": "ready"})
}

fn said(text: &str) -> Value {
    json!({"type": "agent.message", "role": "assistant", "content": [{"type": "text", "text": text}]})
}

async fn stalled(harness: &Harness, minutes: i64) -> bool {
    let now = archon_core::now() + TimeDelta::minutes(minutes);
    supervisor::report_stall(&harness.ctx, now).await.unwrap()
}

#[tokio::test]
async fn a_hung_turn_is_interrupted_and_the_operator_mail_queued_behind_it_answered() {
    let harness = Harness::new().await;
    let pump = running_pump(&harness).await;
    harness.fake.script(
        "interrupt",
        Scripted::reply("interrupted", json!({"was_running": true})),
    );
    orch(&harness, started()).await;
    assert!(
        !stalled(&harness, 11).await,
        "no mail: the full limit holds"
    );

    say(&harness, 1, "What is running?").await;
    assert!(
        stalled(&harness, 11).await,
        "operator mail shortens the limit"
    );
    let nudges = harness.fake.received_named("send_message");
    assert!(
        nudges.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .starts_with("Continue:"),
        "{nudges:?}"
    );
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(
        blockers[0].contains("Archon interrupts the turn"),
        "{blockers:?}"
    );

    assert!(
        !stalled(&harness, 15).await,
        "the grace runs from the notice"
    );
    assert!(harness.fake.received_named("interrupt").is_empty());
    assert!(stalled(&harness, 16).await);
    let interrupts = harness.fake.received_named("interrupt");
    assert_eq!(interrupts.len(), 1, "{interrupts:?}");
    assert_eq!(interrupts[0]["agent"], ORCH);
    let decisions = harness.notifications(NotificationKind::Decision).await;
    assert!(
        decisions
            .iter()
            .any(|d| d.contains("Archon interrupted it") && d.contains("yours included")),
        "{decisions:?}"
    );
    assert!(!stalled(&harness, 40).await, "one interrupt per silence");

    // Aether ends the interrupted turn and delivers the queued inbox in a fresh one.
    orch(&harness, completed()).await;
    orch(&harness, started()).await;
    orch(
        &harness,
        json!({"type": "inbox.consumed", "up_to": 1, "items": []}),
    )
    .await;
    orch(&harness, said("Two items are running: A and B.")).await;
    orch(&harness, completed()).await;
    assert_eq!(
        harness.orchestrator_replies().await,
        vec!["Two items are running: A and B.".to_owned()]
    );
    assert!(
        !stalled(&harness, 120).await,
        "an idle orchestrator is no stall"
    );
    assert_eq!(harness.fake.received_named("interrupt").len(), 1);
    pump.abort();
}

#[tokio::test]
async fn an_operator_github_request_to_a_silent_turn_shortens_the_limit_too() {
    let harness = Harness::new().await;
    let pump = running_pump(&harness).await;
    orch(&harness, started()).await;
    let request = json!({"kind": "external_trigger", "source": "github", "operator": true,
        "idempotency_key": "gh_1",
        "payload": {"repository": "o/r", "number": 378, "requester": "octocat",
                    "text": "@archon what is the status?"}});
    orch(&harness, delivered(1, request)).await;
    assert!(stalled(&harness, 11).await);
    assert_eq!(harness.fake.received_named("send_message").len(), 1);
    pump.abort();
}

#[tokio::test]
async fn a_github_comment_without_operator_standing_keeps_the_full_limit() {
    let harness = Harness::new().await;
    let pump = running_pump(&harness).await;
    orch(&harness, started()).await;
    let comment = json!({"kind": "external_trigger", "source": "github", "operator": false,
        "idempotency_key": "gh_2",
        "payload": {"repository": "o/r", "number": 378, "requester": "stranger",
                    "text": "@archon please merge"}});
    orch(&harness, delivered(1, comment)).await;
    assert!(!stalled(&harness, 11).await);
    assert!(stalled(&harness, 31).await);
    pump.abort();
}
