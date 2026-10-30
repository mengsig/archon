//! A stage agent at `[limits] max_agent_messages` is never silent: the
//! orchestrator is told once, escalations keep a reserve, and replies to the
//! orchestrator are granted.

use crate::agent_messages::{inbox, messages, send};
use crate::common::{self, Harness};

use archon_core::agent_message::{ESCALATION_RESERVE, MAX_REPLY_GRANTS, MessageCap, MessageKind};
use archon_core::config::Config;
use archon_core::notify::FeedEntry;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::run::StageRun;
use archon_core::steer::Steer;
use archon_daemon::triggers::{self, TriggerEvent};
use serde_json::json;

/// A running stage whose agent has sent all `most` of its messages.
async fn capped(most: u32) -> (Harness, QueueItem, StageRun) {
    let mut config = Config::default();
    config.limits.max_agent_messages = most;
    let h = Harness::with_config(config).await;
    let item = h.enqueue("message caps").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    for _ in 0..most {
        send(&h, &stage, MessageKind::Info, "progress")
            .await
            .unwrap();
    }
    assert_eq!(messages(&h).await.len(), most as usize);
    (h, item, stage)
}

async fn cap_notices(h: &Harness) -> Vec<MessageCap> {
    let held = triggers::held(&h.ctx).await.unwrap().into_iter();
    held.filter_map(|event| match event {
        TriggerEvent::MessageCapReached { cap } => Some(cap),
        _ => None,
    })
    .collect()
}

async fn refusal(h: &Harness, stage: &StageRun, kind: MessageKind) -> String {
    let refused = send(h, stage, kind, "refused").await;
    refused.expect_err("past the cap").to_string()
}

#[tokio::test]
async fn a_stage_at_its_cap_tells_the_orchestrator_once_and_its_escalation_still_arrives() {
    let (h, item, stage) = capped(2).await;
    let agent = stage.aether_agent.clone().unwrap();
    for kind in [MessageKind::Info, MessageKind::Relay, MessageKind::Info] {
        refusal(&h, &stage, kind).await;
    }
    let node = h.backend.node_ids().remove(0);
    inbox(
        &h,
        &node,
        1,
        json!({"actor": "agent", "id": agent}),
        "via the node",
    )
    .await;

    let notices = cap_notices(&h).await;
    assert_eq!(notices.len(), 1, "one notice per agent, not per refusal");
    let notice = &notices[0];
    assert_eq!((&notice.item, &notice.stage_run), (&item.id, &stage.id));
    assert_eq!((notice.agent.clone(), notice.cap), (agent.clone(), 2));
    assert_eq!(notice.stage, "implement");
    let feed = h.ctx.store.lock().await.feed_after(0, 1000).unwrap();
    let told = feed.iter().filter(|f| {
        matches!(&f.entry, FeedEntry::Notification(n) if n.text.contains("reached its message cap"))
    });
    assert_eq!(told.count(), 1, "the operator's feed shows it once too");

    send(
        &h,
        &stage,
        MessageKind::Escalation,
        "ready for the live test",
    )
    .await
    .expect("an escalation at the cap still arrives");
    let last = messages(&h).await.pop().unwrap();
    assert_eq!(
        (last.kind, last.text.as_str()),
        (MessageKind::Escalation, "ready for the live test")
    );
    assert_eq!(cap_notices(&h).await.len(), 1);

    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("expected the item");
    };
    assert_eq!(detail.message_caps, notices, "get_item shows the cap");
    let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap() else {
        panic!("expected the status");
    };
    assert_eq!(status.message_caps, notices, "queue_status shows the cap");
}

#[tokio::test]
async fn escalations_keep_a_reserve_that_info_and_relays_cannot_use() {
    let (h, _, stage) = capped(2).await;
    for kind in [MessageKind::Info, MessageKind::Relay, MessageKind::Info] {
        let error = refusal(&h, &stage, kind).await;
        assert!(error.contains("may still use 3 of its reserve"), "{error}");
    }
    for n in 0..ESCALATION_RESERVE {
        send(&h, &stage, MessageKind::Escalation, &format!("blocker {n}"))
            .await
            .expect("within the reserve");
    }
    let error = refusal(&h, &stage, MessageKind::Escalation).await;
    assert!(
        error.contains("reserve of 3 escalations is used up"),
        "{error}"
    );
    let sent = 2 + ESCALATION_RESERVE as usize;
    assert_eq!(messages(&h).await.len(), sent, "the reserve is bounded");
}

#[tokio::test]
async fn each_orchestrator_message_grants_one_reply_up_to_a_bound() {
    let (h, item, stage) = capped(1).await;
    let agent = stage.aether_agent.clone().unwrap();
    let mut store = h.ctx.store.lock().await;
    store
        .kv_set(
            archon_queue::store::keys::ORCHESTRATOR_AGENT,
            common::ORCHESTRATOR,
        )
        .unwrap();
    drop(store);
    let orchestrator = json!({"actor": "agent", "id": common::ORCHESTRATOR});
    refusal(&h, &stage, MessageKind::Info).await;

    inbox(&h, &agent, 1, orchestrator.clone(), "are you ready?").await;
    send(&h, &stage, MessageKind::Info, "yes")
        .await
        .expect("a granted reply");
    refusal(&h, &stage, MessageKind::Info).await;
    inbox(&h, &agent, 1, orchestrator.clone(), "are you ready?").await;
    inbox(
        &h,
        &agent,
        2,
        json!({"actor": "agent", "id": "agt_peer"}),
        "ping",
    )
    .await;
    refusal(&h, &stage, MessageKind::Info).await;

    for seq in 3..8 {
        inbox(&h, &agent, seq, orchestrator.clone(), "status?").await;
    }
    for _ in 0..MAX_REPLY_GRANTS {
        send(&h, &stage, MessageKind::Info, "status")
            .await
            .expect("a granted reply");
    }
    refusal(&h, &stage, MessageKind::Info).await;

    let note = Steer {
        note: Some("tell me when it is ready".into()),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: note,
    };
    h.cli(steer).await.expect("steered");
    send(&h, &stage, MessageKind::Info, "ready")
        .await
        .expect("a steer note grants a reply");
    refusal(&h, &stage, MessageKind::Info).await;
    let replies = 2 + MAX_REPLY_GRANTS as usize + 1;
    assert_eq!(messages(&h).await.len(), replies);
}

#[tokio::test]
async fn a_refusal_names_whose_allowance_is_used_up_and_the_cap() {
    let (h, _, stage) = capped(2).await;
    let agent = stage.aether_agent.clone().unwrap();
    let error = refusal(&h, &stage, MessageKind::Relay).await;
    assert!(error.contains("(2 messages per agent"), "{error}");
    assert!(
        error.contains(&format!("the allowance of {agent}, the sender, is used up")),
        "{error}"
    );
    assert!(error.contains("charged to it, not to them"), "{error}");

    let node = h.backend.node_ids().remove(0);
    inbox(
        &h,
        &node,
        1,
        json!({"actor": "agent", "id": agent}),
        "for my reviewer",
    )
    .await;
    let (to, told) = h.backend.messages().pop().expect("the sender is told");
    assert_eq!(to, agent);
    assert!(
        told.contains(&format!("the allowance of {agent}, the sender")),
        "{told}"
    );
    assert!(told.contains("(2 messages per agent"), "{told}");
}
