//! The operator's `/upgrade` and `/watch` chat commands: only a whole
//! message from Aether's `user` actor in dedicated mode starts anything, and
//! every outcome is `chat_command` news for the orchestrator.

mod common;

use archon_aether::AetherEvent;
use archon_aether::events::parse_event;
use archon_core::config::{AetherMode, Config};
use archon_core::notify::FeedEntry;
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{ops, pump};
use archon_queue::store::keys;
use common::{Harness, REPO_NAME};
use serde_json::{Value, json};

const ORCH: &str = "agt_orch";

async fn harness(mode: AetherMode) -> Harness {
    let mut config = Config::default();
    config.aether.mode = mode;
    let h = Harness::with_config(config).await;
    let mut store = h.ctx.store.lock().await;
    store.kv_set(keys::ORCHESTRATOR_AGENT, ORCH).expect("kv");
    drop(store);
    h
}

/// An `inbox.delivered` on the orchestrator's stream carrying `kind`.
fn delivered(seq: u64, kind: Value) -> AetherEvent {
    let event = json!({
        "id": format!("evt_{seq}"), "seq": seq, "at": "2026-10-12T10:00:00Z",
        "actor": {"type": "system"}, "agent_id": ORCH,
        "kind": {"type": "inbox.delivered", "item": {
            "id": format!("inb_{seq}"), "agent_id": ORCH, "seq": seq,
            "delivered_at": "2026-10-12T10:00:00Z", "kind": kind}}
    });
    parse_event(event).expect("a valid event")
}

fn message(actor: Value, text: &str) -> Value {
    json!({"kind": "message", "message_id": "msg_1", "from": actor,
           "content": [{"type": "text", "text": text}]})
}

fn operator(text: &str) -> Value {
    message(json!({"actor": "user"}), text)
}

async fn deliver(h: &Harness, seq: u64, kind: Value) {
    pump::handle_orchestrator_event(&h.ctx, &delivered(seq, kind))
        .await
        .expect("the pump handles the event");
}

/// `(command, applied, text)` of every `chat_command` held for the orchestrator.
async fn news(h: &Harness) -> Vec<(String, bool, String)> {
    let held = triggers::held(&h.ctx).await.expect("held news");
    let news = held.into_iter().filter_map(|event| match event {
        TriggerEvent::ChatCommand {
            command,
            applied,
            text,
        } => Some((command, applied, text)),
        _ => None,
    });
    news.collect()
}

async fn system_notes(h: &Harness) -> Vec<String> {
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    let notes = feed.into_iter().filter_map(|item| match item.entry {
        FeedEntry::System { text } => Some(text),
        _ => None,
    });
    notes.collect()
}

async fn upgrade_under_way(h: &Harness) -> bool {
    ops::phase(&h.ctx).await.is_some()
}

#[tokio::test]
async fn the_operator_starts_a_drained_restart_from_the_chat() {
    let h = harness(AetherMode::Dedicated).await;
    deliver(&h, 1, operator("/upgrade restart requeue")).await;

    assert!(upgrade_under_way(&h).await, "the upgrade was accepted");
    let news = news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    let (command, applied, text) = &news[0];
    assert_eq!((command.as_str(), *applied), ("/upgrade", true));
    assert!(text.contains("a drained restart"), "{text}");
    // A replayed event applies nothing twice.
    deliver(&h, 1, operator("/upgrade restart requeue")).await;
    assert_eq!(super_news_len(&h).await, 1);
}

async fn super_news_len(h: &Harness) -> usize {
    news(h).await.len()
}

#[tokio::test]
async fn nobody_but_the_operator_upgrades_or_watches_from_the_chat() {
    let h = harness(AetherMode::Dedicated).await;
    let senders = [
        json!({"actor": "agent", "id": "agt_stage"}),
        json!({"actor": "client", "id": "dev_1"}),
        json!({"actor": "system"}),
    ];
    let mut seq = 0;
    for sender in senders {
        for text in ["/upgrade archon main", "/watch archon"] {
            seq += 1;
            deliver(&h, seq, message(sender.clone(), text)).await;
        }
    }
    seq += 1;
    let github = json!({"kind": "external_trigger", "source": "github",
        "idempotency_key": "gh_1", "from": {"actor": "user"},
        "payload": {"repository": "o/r", "number": 3, "text": "/upgrade restart"}});
    deliver(&h, seq, github).await;
    for text in [
        "please /upgrade restart",
        "`/watch archon`",
        "/upgraded both main",
    ] {
        seq += 1;
        deliver(&h, seq, operator(text)).await;
    }

    assert!(!upgrade_under_way(&h).await, "nothing started");
    assert_eq!(news(&h).await, [], "no command was seen");
    assert_eq!(h.ops.snapshot().installs, []);
}

#[tokio::test]
async fn a_shared_instance_refuses_chat_commands_and_says_why() {
    let h = harness(AetherMode::Shared).await;
    deliver(&h, 1, operator("/upgrade restart")).await;
    deliver(&h, 2, operator(&format!("/watch {REPO_NAME}"))).await;

    assert!(!upgrade_under_way(&h).await);
    let news = news(&h).await;
    assert_eq!(news.len(), 2, "{news:?}");
    for (_, applied, why) in &news {
        assert!(!applied);
        assert!(
            why.contains("mode = \"shared\"") && why.contains("[listen]"),
            "{why}"
        );
    }
    let notes = system_notes(&h).await;
    assert!(notes.iter().any(|n| n.contains("was refused")), "{notes:?}");
}

#[tokio::test]
async fn a_malformed_or_failing_command_is_refused_with_its_reason() {
    let h = harness(AetherMode::Dedicated).await;
    deliver(&h, 1, operator("/upgrade archon")).await;
    deliver(&h, 2, operator("/watch nowhere")).await;
    // The test repository has no origin remote to watch.
    deliver(&h, 3, operator(&format!("/watch {REPO_NAME}"))).await;

    assert!(!upgrade_under_way(&h).await);
    let news = news(&h).await;
    let refusals: Vec<&str> = news
        .iter()
        .map(|(_, applied, why)| {
            assert!(!applied, "{why}");
            why.as_str()
        })
        .collect();
    assert_eq!(refusals.len(), 3, "{refusals:?}");
    assert!(
        refusals[0].starts_with("nothing to upgrade"),
        "{}",
        refusals[0]
    );
    assert!(refusals[1].contains("nowhere"), "{}", refusals[1]);
    assert!(refusals[2].contains("no origin remote"), "{}", refusals[2]);
}
