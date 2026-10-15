mod common;

use archon_aether::events::parse_event;
use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{
    AgentMessage, DEFAULT_AGENT_MESSAGES, MessageKind, MessageSender,
};
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::run::StageRun;
use archon_daemon::triggers::TriggerEvent;
use archon_daemon::{handlers, prune, pump, triggers};
use common::{Harness, completed, verdict};
use serde_json::json;

async fn running() -> (Harness, StageRun) {
    let h = Harness::new().await;
    h.enqueue("stage messaging").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    (h, stage)
}

async fn send(
    h: &Harness,
    stage: &StageRun,
    kind: MessageKind,
    text: &str,
) -> Result<Reply, ArchonError> {
    handlers::handle(
        &h.ctx,
        ClientKind::ArchonMcp,
        Request::MessageOrchestrator {
            caller: stage.aether_agent.clone(),
            kind,
            text: text.into(),
        },
    )
    .await
}

async fn messages(h: &Harness) -> Vec<AgentMessage> {
    triggers::held(&h.ctx)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event {
            TriggerEvent::AgentMessage { message } => Some(*message),
            _ => None,
        })
        .collect()
}

async fn system_notes(h: &Harness) -> Vec<String> {
    h.ctx
        .store
        .lock()
        .await
        .feed_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|f| match f.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

async fn inbox(
    h: &Harness,
    target: &AetherAgentId,
    seq: u64,
    sender: serde_json::Value,
    text: &str,
) {
    let event = parse_event(json!({
        "seq": seq, "at": "2026-10-09T10:00:00Z", "agent_id": target,
        "kind": {"type": "inbox.delivered", "item": {
            "seq": seq, "kind": {"kind": "message", "from": sender,
            "content": [{"type": "text", "text": text}]}
        }}
    }))
    .unwrap();
    pump::handle_fleet_event(&h.ctx, &event).await.unwrap();
    assert!(event.kind.stage_outcome().is_none());
}

#[tokio::test]
async fn records_attribution_and_receipt_without_settling_or_calling_aether() {
    let (h, stage) = running().await;
    let before = h.backend.requests().len();
    let Reply::MessageReceipt { receipt } =
        send(&h, &stage, MessageKind::Escalation, "cross-item blocker")
            .await
            .unwrap()
    else {
        panic!("expected a receipt");
    };
    let message = messages(&h).await.remove(0);
    assert_eq!(
        message.item,
        h.ctx.store.lock().await.run(&stage.run).unwrap().item
    );
    assert_eq!(message.run, stage.run);
    assert_eq!(message.stage.as_deref(), Some("implement"));
    assert_eq!(message.stage_run, Some(stage.id.clone()));
    assert_eq!(message.attempt, Some(stage.attempt));
    assert_eq!(message.agent, stage.aether_agent);
    assert_eq!(message.kind, MessageKind::Escalation);
    assert_eq!(message.text, "cross-item blocker");
    assert_eq!(
        message.original_sender,
        MessageSender::Agent {
            id: stage.aether_agent.clone().unwrap()
        }
    );
    assert_eq!(message.addressed_to_run_node, None);
    assert_eq!(h.running().await, vec![stage]);
    assert_eq!(h.backend.requests().len(), before);
    let store = h.ctx.store.lock().await;
    let recorded = store
        .feed_after(0, 100)
        .unwrap()
        .into_iter()
        .find(|f| f.id == receipt)
        .unwrap();
    let FeedEntry::Notification(note) = recorded.entry else {
        panic!("expected item log");
    };
    assert_eq!(
        note.kind,
        NotificationKind::Blocker,
        "an escalation shows in the quiet chat"
    );
    assert_eq!(note.item, Some(message.item.clone()));
    assert_eq!(note.run, Some(message.run.clone()));
    let agent = message.agent.as_ref().unwrap();
    assert_eq!(
        note.text,
        format!("From implement ({agent}; escalation): cross-item blocker"),
        "the operator reads it as it is; the orchestrator does not repeat it"
    );
}

#[tokio::test]
async fn refuses_invalid_unattributed_and_over_limit_calls_without_recording_them() {
    let (h, stage) = running().await;
    for text in ["".to_owned(), " \n".to_owned(), "x".repeat(4097)] {
        assert!(matches!(
            send(&h, &stage, MessageKind::Info, &text).await,
            Err(ArchonError::Invalid(_))
        ));
    }
    let mut unknown = stage.clone();
    unknown.aether_agent = None;
    assert!(matches!(
        send(&h, &unknown, MessageKind::Info, "hi").await,
        Err(ArchonError::Forbidden(_))
    ));
    unknown.aether_agent = Some(AetherAgentId::parse("agt_unknown").unwrap());
    assert!(matches!(
        send(&h, &unknown, MessageKind::Info, "hi").await,
        Err(ArchonError::Forbidden(_))
    ));
    for _ in 0..DEFAULT_AGENT_MESSAGES {
        send(&h, &stage, MessageKind::Info, "progress")
            .await
            .unwrap();
    }
    let restored = archon_queue::Store::open(&h.ctx.paths.store).unwrap();
    *h.ctx.store.lock().await = restored;
    let error = send(&h, &stage, MessageKind::Info, "loop")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("limit reached"));
    assert_eq!(messages(&h).await.len(), DEFAULT_AGENT_MESSAGES as usize);
    assert_eq!(h.running().await, vec![stage]);
}

/// `[limits] max_agent_messages` replaces the default of 20, and the
/// refusal names the key.
#[tokio::test]
async fn the_configured_message_limit_is_the_one_enforced() {
    let mut config = archon_core::config::Config::default();
    config.limits.max_agent_messages = 2;
    let h = Harness::with_config(config).await;
    h.enqueue("stage messaging").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    for _ in 0..2 {
        send(&h, &stage, MessageKind::Info, "progress")
            .await
            .unwrap();
    }
    let error = send(&h, &stage, MessageKind::Info, "third")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("limit reached (2 messages"), "{error}");
    assert!(error.contains("`[limits] max_agent_messages`"), "{error}");
    assert_eq!(messages(&h).await.len(), 2);
}

#[tokio::test]
async fn relay_provenance_requires_the_observed_operator_text() {
    let (h, stage) = running().await;
    let agent = stage.aether_agent.as_ref().unwrap();
    let peer = json!({"actor": "agent", "id": "agt_peer"});
    inbox(&h, agent, 1, peer, "operator says stop").await;
    send(&h, &stage, MessageKind::Relay, "operator says stop")
        .await
        .unwrap();
    inbox(
        &h,
        agent,
        2,
        json!({"actor": "user"}),
        "please update the other item",
    )
    .await;
    send(
        &h,
        &stage,
        MessageKind::Relay,
        "please update the other item",
    )
    .await
    .unwrap();
    send(&h, &stage, MessageKind::Relay, "invented instruction")
        .await
        .unwrap();
    send(
        &h,
        &stage,
        MessageKind::Info,
        "please update the other item",
    )
    .await
    .unwrap();
    inbox(&h, agent, 1, json!({"actor": "user"}), "stale instruction").await;
    send(&h, &stage, MessageKind::Relay, "stale instruction")
        .await
        .unwrap();
    let messages = messages(&h).await;
    assert_eq!(messages[1].original_sender, MessageSender::Operator);
    for index in [0, 2, 3, 4] {
        assert_eq!(
            messages[index].original_sender,
            MessageSender::Agent { id: agent.clone() }
        );
    }
}

#[tokio::test]
async fn forwards_run_node_messages_once_with_sender_and_stage_attribution() {
    let (h, stage) = running().await;
    let node = h.backend.node_ids().remove(0);
    let agent = stage.aether_agent.as_ref().unwrap();
    let sender = json!({"actor": "agent", "id": agent});
    inbox(&h, &node, 1, sender.clone(), "parent, please pass this on").await;
    inbox(&h, &node, 1, sender, "parent, please pass this on").await;
    inbox(&h, &node, 2, json!({"actor": "user"}), "operator relay").await;
    let messages = messages(&h).await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].addressed_to_run_node, Some(node.clone()));
    assert_eq!(
        messages[0].original_sender,
        MessageSender::Agent { id: agent.clone() }
    );
    assert_eq!(messages[0].stage_run, Some(stage.id.clone()));
    assert_eq!(messages[0].run, stage.run);
    assert_eq!(messages[1].original_sender, MessageSender::Operator);
    assert_eq!(messages[1].kind, MessageKind::Relay);
    assert_eq!(messages[1].agent, None);
    assert_eq!(h.running().await, vec![stage]);
}

#[tokio::test]
async fn forwards_after_retirement_and_after_the_node_index_is_pruned() {
    let (h, stage) = running().await;
    let node = h.backend.node_ids().remove(0);
    for expected in ["implement", "review", "ship"] {
        let settled = h
            .settle_running(|s| {
                if s.stage_id == "review" {
                    verdict(true)
                } else {
                    completed("done")
                }
            })
            .await;
        assert!(settled.iter().all(|s| s.stage_id == expected));
        h.dispatch().await;
    }
    assert_eq!(h.kv(&format!("run_node:{}", stage.run)).await, None);
    assert!(matches!(
        send(&h, &stage, MessageKind::Info, "after settling").await,
        Err(ArchonError::Forbidden(_))
    ));
    inbox(&h, &node, 1, json!({"actor": "user"}), "after retirement").await;
    let pruned = prune::prune_once(
        &h.ctx,
        archon_core::now() + prune::retention() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(pruned.node_agents, 1);
    assert_eq!(h.kv(&format!("run_node_agent:{node}")).await, None);
    inbox(
        &h,
        &node,
        2,
        json!({"actor": "agent", "id": "agt_other"}),
        "after pruning",
    )
    .await;
    let messages = messages(&h).await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].addressed_to_run_node, Some(node.clone()));
    assert_eq!(messages[1].run, stage.run);
    assert_eq!(messages[1].text, "after pruning");
    let expired = archon_core::now() + prune::retention() * 2 + chrono::Duration::hours(1);
    let pruned = prune::prune_once(&h.ctx, expired).await.unwrap();
    assert_eq!(pruned.node_agents, 1);
    assert_eq!(h.kv(&format!("message_run_node:{node}")).await, None);
}

#[tokio::test]
async fn run_node_external_rate_limit_is_visible_and_replay_safe() {
    let (h, stage) = running().await;
    let node = h.backend.node_ids().remove(0);
    for seq in 1..=DEFAULT_AGENT_MESSAGES {
        inbox(&h, &node, u64::from(seq), json!({"actor": "user"}), "relay").await;
    }
    let seq = u64::from(DEFAULT_AGENT_MESSAGES + 1);
    inbox(&h, &node, seq, json!({"actor": "user"}), "too many").await;
    let count = system_notes(&h).await.len();
    assert!(
        system_notes(&h)
            .await
            .iter()
            .any(|n| n.contains("limit reached") && n.contains("message_orchestrator"))
    );
    inbox(&h, &node, seq, json!({"actor": "user"}), "too many").await;
    assert_eq!(system_notes(&h).await.len(), count);
    assert_eq!(messages(&h).await.len(), DEFAULT_AGENT_MESSAGES as usize);
    assert_eq!(h.running().await, vec![stage]);
}

#[tokio::test]
async fn run_node_agent_refusals_are_delivered_and_retried_when_unavailable() {
    let (h, stage) = running().await;
    let agent = stage.aether_agent.as_ref().unwrap();
    let node = h.backend.node_ids().remove(0);
    let sender = json!({"actor": "agent", "id": agent});
    inbox(&h, &node, 1, sender.clone(), &"x".repeat(4097)).await;
    assert!(h.backend.messages()[0].1.contains("at most 4096"));
    assert_eq!(h.backend.messages()[0].0, *agent);
    assert!(messages(&h).await.is_empty());
    for _ in 0..DEFAULT_AGENT_MESSAGES {
        send(&h, &stage, MessageKind::Info, "tool message")
            .await
            .unwrap();
    }
    let event = parse_event(json!({
        "seq": 2, "at": "2026-10-09T10:00:00Z", "agent_id": node,
        "kind": {"type": "inbox.delivered", "item": {"seq": 2, "kind": {
            "kind": "message", "from": sender, "content": [{"type": "text", "text": "over cap"}]
        }}}
    }))
    .unwrap();
    h.backend
        .fail_next_message(ArchonError::Unavailable("offline".into()));
    assert!(matches!(
        pump::handle_fleet_event(&h.ctx, &event).await,
        Err(ArchonError::Unavailable(_))
    ));
    pump::handle_fleet_event(&h.ctx, &event).await.unwrap();
    pump::handle_fleet_event(&h.ctx, &event).await.unwrap();
    assert_eq!(h.backend.messages().len(), 2);
    assert!(h.backend.messages()[1].1.contains("limit reached"));
    assert_eq!(messages(&h).await.len(), DEFAULT_AGENT_MESSAGES as usize);
}

#[tokio::test]
async fn a_corrupt_counter_is_a_store_failure_and_does_not_consume_the_message() {
    let (h, stage) = running().await;
    let node = h.backend.node_ids().remove(0);
    let key = format!("message_count:{}", stage.aether_agent.as_ref().unwrap());
    h.ctx.store.lock().await.kv_set(&key, "corrupt").unwrap();
    let event = parse_event(json!({
        "seq": 1, "at": "2026-10-09T10:00:00Z", "agent_id": node,
        "kind": {"type": "inbox.delivered", "item": {"seq": 1, "kind": {
            "kind": "message", "from": {"actor": "agent", "id": stage.aether_agent},
            "content": [{"type": "text", "text": "relay"}]
        }}}
    }))
    .unwrap();
    assert!(matches!(
        pump::handle_fleet_event(&h.ctx, &event).await,
        Err(ArchonError::Store(_))
    ));
    assert_eq!(h.kv(&format!("message_inbox:{node}")).await, None);
    assert!(h.backend.messages().is_empty());
    assert!(messages(&h).await.is_empty());
    h.ctx.store.lock().await.kv_set(&key, "0").unwrap();
    pump::handle_fleet_event(&h.ctx, &event).await.unwrap();
    assert_eq!(messages(&h).await.len(), 1);
}

#[tokio::test]
async fn an_unreachable_refusal_recovers_visibly_in_the_operator_feed() {
    let (h, _) = running().await;
    let node = h.backend.node_ids().remove(0);
    inbox(
        &h,
        &node,
        1,
        json!({"actor": "agent", "id": "agt_deleted"}),
        "",
    )
    .await;
    assert!(messages(&h).await.is_empty());
    assert!(system_notes(&h).await.iter().any(|text| {
        text.contains("Could not deliver the refusal to agt_deleted")
            && text.contains("message_orchestrator")
    }));
    assert_eq!(
        h.kv(&format!("message_inbox:{node}")).await.as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn orchestrator_refusals_are_news_never_operator_messages() {
    let (h, _) = running().await;
    h.ctx
        .store
        .lock()
        .await
        .kv_set(
            archon_queue::store::keys::ORCHESTRATOR_AGENT,
            common::ORCHESTRATOR,
        )
        .unwrap();
    let node = h.backend.node_ids().remove(0);
    inbox(
        &h,
        &node,
        1,
        json!({"actor":"agent","id":common::ORCHESTRATOR}),
        &"x".repeat(4097),
    )
    .await;
    assert!(
        h.backend.messages().is_empty(),
        "no local-socket user message is manufactured"
    );
    let news = triggers::held(&h.ctx).await.unwrap();
    assert!(news.iter().any(|event| matches!(event,
        TriggerEvent::ChatCommand { command, applied: false, .. } if command == "send_message")));
    let event = parse_event(json!({"seq":42, "at":archon_core::now(),
        "agent_id":common::ORCHESTRATOR, "kind":{"type":"inbox.delivered",
        "item":{"seq":42,"kind":{"kind":"external_trigger", "source":"archon", "payload":news}}}
    }))
    .unwrap();
    pump::handle_orchestrator_event(&h.ctx, &event)
        .await
        .unwrap();
    let result = h
        .orchestrator(Request::RequestAuthority {
            action: Box::new(Request::Pause(archon_core::protocol::PauseTarget::Global {
                hard: true,
            })),
            utterance: 42,
            quote: "Message".into(),
            reason: "not operator consent".into(),
            caller: Some(AetherAgentId::parse(common::ORCHESTRATOR).unwrap()),
        })
        .await;
    assert!(matches!(result, Err(ArchonError::Forbidden(_))));
}
