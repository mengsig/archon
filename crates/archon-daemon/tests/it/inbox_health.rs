//! Inbox items a running stage's agents leave unread are a stage health
//! warning in `get_item` and `status`.

use crate::common;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::id::QueueItemId;
use archon_core::protocol::{Reply, Request, StageWarning};
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun};
use common::{Harness, completed, verdict};
use serde_json::{Value, json};

async fn send(h: &Harness, agent: &AetherAgentId, kind: Value, at: Timestamp) {
    let mut wire = archon_aether::fake::event(Some(agent.as_str()), None, kind);
    wire["seq"] = json!(1);
    wire["at"] = json!(at);
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
}

async fn deliver(h: &Harness, agent: &AetherAgentId, seq: u64, minutes_ago: i64) {
    let at = archon_core::now() - chrono::Duration::minutes(minutes_ago);
    let item = json!({
        "id": format!("inb_{seq}"), "agent_id": agent, "seq": seq, "delivered_at": at,
        "kind": {"kind": "task_result", "task_id": "tsk_1"},
    });
    send(
        h,
        agent,
        json!({"type": "inbox.delivered", "item": item}),
        at,
    )
    .await;
}

async fn delegate(h: &Harness, parent: &AetherAgentId, agent: &str) -> AetherAgentId {
    let model: archon_core::model::ModelRef = common::MODEL.parse().unwrap();
    let kind = json!({"type": "agent.created", "name": agent, "model": model});
    let mut wire = archon_aether::fake::event(Some(agent), None, kind);
    wire["parent_agent_id"] = json!(parent);
    wire["seq"] = json!(1);
    wire["at"] = json!(archon_core::now());
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
    AetherAgentId::parse(agent).unwrap()
}

async fn item_warnings(h: &Harness, item: &QueueItemId) -> Vec<StageWarning> {
    match h
        .cli(Request::GetItem { item: item.clone() })
        .await
        .unwrap()
    {
        Reply::ItemDetail(detail) => detail.stage_warnings,
        other => panic!("item detail expected: {other:?}"),
    }
}

async fn status_warnings(h: &Harness) -> Vec<StageWarning> {
    match h.cli(Request::Status(Default::default())).await.unwrap() {
        Reply::Status(status) => status.stage_warnings,
        other => panic!("status expected: {other:?}"),
    }
}

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

async fn unread_keys(h: &Harness) -> usize {
    let store = h.ctx.store.lock().await;
    store.kv_prefix("inbox_unread_").unwrap().len()
}

/// A lead whose children finished but who never took their results warns,
/// with its stage, until it reads them; a fresh item does not.
#[tokio::test]
async fn an_inbox_item_left_unread_past_the_threshold_warns_until_it_is_taken() {
    let h = Harness::new().await;
    let item = h.enqueue("a lead that stopped reading").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let stage_agent = stage.aether_agent.clone().unwrap();
    let lead = delegate(&h, &stage_agent, "agt_inbox_lead").await;
    deliver(&h, &lead, 1, 20).await;
    deliver(&h, &lead, 1, 20).await;
    deliver(&h, &lead, 2, 18).await;
    deliver(&h, &stage_agent, 1, 1).await;

    let warnings = item_warnings(&h, &item.id).await;
    assert_eq!(
        warnings.len(),
        1,
        "the stage agent's item is fresh: {warnings:?}"
    );
    let warning = &warnings[0];
    assert_eq!(
        (&warning.agent, &warning.stage, warning.unread),
        (&lead, &stage.id, 2),
        "a replayed delivery counts once"
    );
    assert_eq!(warning.stage_id, stage.stage_id);
    assert!(
        warning.describe().contains("may have died"),
        "{}",
        warning.describe()
    );
    assert_eq!(status_warnings(&h).await, warnings, "status shows the same");

    send(
        &h,
        &lead,
        json!({"type": "inbox.consumed", "up_to": 1}),
        archon_core::now(),
    )
    .await;
    assert_eq!(item_warnings(&h, &item.id).await[0].unread, 1);
    send(
        &h,
        &lead,
        json!({"type": "inbox.consumed", "up_to": 2}),
        archon_core::now(),
    )
    .await;
    assert!(item_warnings(&h, &item.id).await.is_empty());
    assert!(status_warnings(&h).await.is_empty());
}

/// An agent that ended, or a stage that is no longer running, has nobody
/// left to read its inbox, so neither warns.
#[tokio::test]
async fn an_ended_agent_or_a_settled_stage_does_not_warn() {
    let h = Harness::new().await;
    let item = h.enqueue("ended agents").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let stage_agent = stage.aether_agent.clone().unwrap();
    let lead = delegate(&h, &stage_agent, "agt_inbox_ended").await;
    deliver(&h, &lead, 1, 30).await;
    deliver(&h, &stage_agent, 1, 30).await;
    assert_eq!(item_warnings(&h, &item.id).await.len(), 2);

    send(
        &h,
        &lead,
        json!({"type": "agent.completed"}),
        archon_core::now(),
    )
    .await;
    let left = item_warnings(&h, &item.id).await;
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].agent, stage_agent);

    h.settle_running(passing).await;
    assert!(
        h.running().await.iter().all(|s| s.id != stage.id),
        "the stage settled"
    );
    assert!(item_warnings(&h, &item.id).await.is_empty());
    assert!(status_warnings(&h).await.is_empty());
}

/// What an agent of a finished item left unread goes with the next prune.
#[tokio::test]
async fn a_finished_items_unread_inboxes_are_pruned() {
    let h = Harness::new().await;
    let item = h.enqueue("finished with mail").await;
    h.dispatch().await;
    let stage_agent = h.running().await.remove(0).aether_agent.unwrap();
    deliver(&h, &stage_agent, 1, 30).await;
    for _ in ["implement", "review", "ship"] {
        h.settle_running(passing).await;
        h.dispatch().await;
    }
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    assert_eq!(unread_keys(&h).await, 1, "kept until pruned");
    assert!(status_warnings(&h).await.is_empty(), "no stage runs");

    let pruned = archon_daemon::prune::prune_once(&h.ctx, archon_core::now())
        .await
        .unwrap();
    assert_eq!(pruned.unread_inboxes, 1, "{pruned:?}");
    assert_eq!(unread_keys(&h).await, 0);
}

/// Past the items kept one by one, the rest are still counted, and taking
/// only the kept ones leaves them warning.
#[tokio::test]
async fn items_past_the_kept_ones_still_warn_once_the_kept_ones_are_taken() {
    let h = Harness::new().await;
    let item = h.enqueue("a flooded inbox").await;
    h.dispatch().await;
    let stage_agent = h.running().await.remove(0).aether_agent.unwrap();
    for seq in 1..=66 {
        deliver(&h, &stage_agent, seq, 20).await;
    }
    deliver(&h, &stage_agent, 65, 20).await;
    assert_eq!(
        item_warnings(&h, &item.id).await[0].unread,
        66,
        "a replay counts once"
    );

    let consumed = |up_to: u64| json!({"type": "inbox.consumed", "up_to": up_to});
    send(&h, &stage_agent, consumed(64), archon_core::now()).await;
    let left = item_warnings(&h, &item.id).await;
    assert_eq!(left.len(), 1, "seq 65 and 66 are unread: {left:?}");
    assert_eq!(left[0].unread, 2);

    send(&h, &stage_agent, consumed(65), archon_core::now()).await;
    assert_eq!(item_warnings(&h, &item.id).await[0].unread, 1);
    send(&h, &stage_agent, consumed(66), archon_core::now()).await;
    assert!(item_warnings(&h, &item.id).await.is_empty());
    assert_eq!(
        unread_keys(&h).await,
        0,
        "the record goes with the last item"
    );
}
