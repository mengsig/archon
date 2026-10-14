//! Trigger delivery against the fake aetherd: batching, stable keys across
//! retries, refusals, and creating the orchestrator on demand.

mod link_common;

use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::autonomy::{Directive, DirectiveSource};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId};
use archon_daemon::supervisor;
use archon_daemon::triggers::{self, TriggerEvent};
use archon_queue::store::keys;
use link_common::{Harness, WAIT, cancelled_item};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";

#[tokio::test]
async fn a_run_node_pruned_by_an_older_daemon_is_recovered_from_validated_aether_labels() {
    let harness = harness().await;
    let stage =
        link_common::running_stage(&harness.ctx, harness.dir.path(), "agt_stage", "tsk_stage")
            .await;
    let owner = harness.ctx.store.lock().await.run(&stage.run).unwrap();
    let mut node = fake::agent("agt_old_node", None);
    node["role"] = json!(format!("archon-run:{}", owner.pipeline));
    node["scope"] = json!({"network": "allowed", "labels": {
        "archon.run": owner.id, "archon.item": owner.item
    }});
    harness
        .fake
        .script("get_agent", Scripted::reply("agent", node.clone()));
    let event = archon_aether::events::parse_event(json!({
        "seq": 1, "at": "2026-10-09T10:00:00Z", "agent_id": "agt_old_node",
        "kind": {"type": "inbox.delivered", "item": {"seq": 1, "kind": {
            "kind": "message", "from": {"actor": "user"},
            "content": [{"type": "text", "text": "legacy node message"}]
        }}}
    }))
    .unwrap();
    archon_daemon::pump::handle_fleet_event(&harness.ctx, &event)
        .await
        .unwrap();
    let events = triggers::held(&harness.ctx).await.unwrap();
    let TriggerEvent::AgentMessage { message } = &events[0] else {
        panic!("forwarded message");
    };
    assert_eq!(message.run, stage.run);
    assert_eq!(message.item, owner.item);
    assert_eq!(message.text, "legacy node message");
    assert_eq!(
        message.original_sender,
        archon_core::agent_message::MessageSender::Operator
    );
    assert_eq!(
        harness.kv("message_run_node:agt_old_node").await.as_deref(),
        Some(owner.id.as_str())
    );
    archon_daemon::pump::handle_fleet_event(&harness.ctx, &event)
        .await
        .unwrap();
    assert_eq!(triggers::held(&harness.ctx).await.unwrap().len(), 1);
    node["id"] = json!("agt_bad_node");
    node["scope"]["labels"]["archon.item"] = json!(QueueItemId::new());
    harness
        .fake
        .script("get_agent", Scripted::reply("agent", node));
    let mut bad = event;
    bad.agent = Some(link_common::agent_id("agt_bad_node"));
    archon_daemon::pump::handle_fleet_event(&harness.ctx, &bad)
        .await
        .unwrap();
    assert!(
        harness
            .system_notes()
            .await
            .iter()
            .any(|text| text.contains("labels do not match")
                && text.contains("message_orchestrator"))
    );
    assert_eq!(triggers::held(&harness.ctx).await.unwrap().len(), 1);
}

#[tokio::test]
async fn stage_messages_survive_compaction_and_are_delivered_with_attribution() {
    use archon_core::agent_message::{DEFAULT_AGENT_MESSAGES, MessageKind};
    use archon_core::protocol::{ClientKind, Request};
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness.fake.set_default("trigger", Scripted::ok());
    let stage =
        link_common::running_stage(&harness.ctx, harness.dir.path(), "agt_stage", "tsk_stage")
            .await;
    for index in 0..DEFAULT_AGENT_MESSAGES {
        archon_daemon::handlers::handle(
            &harness.ctx,
            ClientKind::ArchonMcp,
            Request::MessageOrchestrator {
                caller: stage.aether_agent.clone(),
                kind: MessageKind::Escalation,
                text: format!("message {index}"),
            },
        )
        .await
        .unwrap();
    }
    triggers::collapse_for_new_orchestrator(&harness.ctx)
        .await
        .unwrap();
    harness
        .fake
        .script("trigger", Scripted::refuse("internal", "busy"));
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    assert_eq!(sent[0]["payload"], sent[1]["payload"]);
    assert_eq!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    let events = sent[1]["payload"]["events"].as_array().unwrap();
    assert_eq!(events.len(), DEFAULT_AGENT_MESSAGES as usize);
    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    for (index, message) in events.iter().enumerate() {
        assert_eq!(message["type"], "agent_message");
        assert_eq!(message["item"], json!(item));
        assert_eq!(message["run"], json!(stage.run));
        assert_eq!(message["stage_run"], json!(stage.id));
        assert_eq!(message["stage"], stage.stage_id);
        assert_eq!(message["attempt"], stage.attempt);
        assert_eq!(message["agent"], "agt_stage");
        assert_eq!(
            message["original_sender"],
            json!({"actor": "agent", "id": "agt_stage"})
        );
        assert_eq!(message["text"], format!("message {index}"));
    }
    assert_eq!(
        harness.ctx.store.lock().await.stage_run(&stage.id).unwrap(),
        stage
    );
}

async fn harness() -> Harness {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 50;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    harness
}

fn orchestrator_in(state: &str) -> Scripted {
    let mut agent = fake::agent(ORCH, None);
    agent["status"] = json!({ "state": state });
    Scripted::reply("agent", agent)
}

fn live_orchestrator(harness: &Harness) {
    harness.fake.set_default(
        "get_agent",
        Scripted::reply("agent", fake::agent(ORCH, None)),
    );
}

fn blocked(reason: &str) -> TriggerEvent {
    TriggerEvent::ItemBlocked {
        item: QueueItemId::new(),
        reason: reason.into(),
    }
}

/// Waking news the orchestrator handles in its own time.
fn violation(action: &str) -> TriggerEvent {
    TriggerEvent::PolicyViolation {
        agent: "agt_helper".into(),
        model: "claude-code/claude-opus-5-5".into(),
        action: action.into(),
    }
}

fn actions(trigger: &Value) -> Vec<&str> {
    let events = trigger["payload"]["events"].as_array().unwrap();
    events
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect()
}

fn reasons(trigger: &Value) -> Vec<&str> {
    let events = trigger["payload"]["events"].as_array().unwrap();
    events
        .iter()
        .map(|e| e["reason"].as_str().unwrap())
        .collect()
}

fn budget(scope: &str) -> TriggerEvent {
    TriggerEvent::Budget {
        scope: scope.into(),
        spent_usd: 51.0,
        limit_usd: 50.0,
    }
}

fn types(trigger: &Value) -> Vec<&str> {
    let events = trigger["payload"]["events"].as_array().unwrap();
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn a_failed_delivery_is_retried_with_the_same_key_and_payload() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness
        .fake
        .script("trigger", Scripted::refuse("internal", "busy"));
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("first"))
        .await
        .unwrap();
    triggers::push(&harness.ctx, blocked("second"))
        .await
        .unwrap();

    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    let store = supervisor::store_id(&harness.ctx).await.unwrap();
    assert_eq!(sent[0]["idempotency_key"], format!("trg_{store}_1_2"));
    assert_eq!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    assert_eq!(sent[0]["payload"], sent[1]["payload"]);
    assert_eq!(sent[0]["agent"], ORCH);
    assert_eq!(sent[0]["payload"]["archon"], 1);
    let summary = sent[0]["payload"]["summary"].as_str().unwrap();
    assert!(summary.starts_with("2 events: item blocked:"), "{summary}");
    assert!(summary.chars().count() <= 160);
    assert_eq!(reasons(&sent[0]), vec!["first", "second"]);
}

#[tokio::test]
async fn a_refused_payload_is_dropped_with_a_note_and_later_news_still_flows() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness.fake.script(
        "trigger",
        Scripted::refuse("invalid_argument", "payload too large"),
    );
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("huge")).await.unwrap();
    harness.fake.wait_for("trigger", 1, WAIT).await;
    harness
        .eventually("the refusal is noted", || async {
            !harness.system_notes().await.is_empty()
        })
        .await;
    triggers::push(&harness.ctx, blocked("small"))
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    assert_ne!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    assert_eq!(reasons(&sent[1]), vec!["small"]);
    assert!(harness.system_notes().await[0].contains("payload too large"));
}

#[tokio::test]
async fn delivery_creates_the_orchestrator_when_none_exists() {
    let harness = harness().await;
    let mut created = fake::agent(ORCH, None);
    created["status"] = json!({"state": "ready"});
    harness
        .fake
        .script("create_agent", Scripted::reply("agent", created));
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("first news"))
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 1, WAIT).await;
    delivery.abort();
    assert_eq!(harness.fake.received_named("create_agent").len(), 1);
    assert_eq!(sent[0]["agent"], ORCH);
}

#[tokio::test]
async fn a_trigger_refused_because_the_orchestrator_just_stopped_is_retried() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    // Live for `ensure_orchestrator`, failed by the time the trigger lands,
    // then live again once revived.
    harness.fake.script("get_agent", orchestrator_in("running"));
    harness.fake.script("get_agent", orchestrator_in("failed"));
    live_orchestrator(&harness);
    harness.fake.script(
        "trigger",
        Scripted::refuse("invalid_argument", "agent agt_0001 is failed"),
    );
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("item done"))
        .await
        .unwrap();

    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    assert_eq!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    assert_eq!(reasons(&sent[1]), vec!["item done"]);
    assert!(
        harness.system_notes().await.is_empty(),
        "nothing was dropped"
    );
}

#[tokio::test]
async fn held_news_survives_an_archond_restart_under_its_key() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness
        .fake
        .set_default("trigger", Scripted::refuse("internal", "busy"));
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("in flight"))
        .await
        .unwrap();
    let first = harness.fake.wait_for("trigger", 1, WAIT).await[0].clone();
    triggers::push(&harness.ctx, blocked("arrived meanwhile"))
        .await
        .unwrap();
    harness
        .eventually("the later event is saved", || async {
            harness
                .kv("trigger_outbox")
                .await
                .is_some_and(|raw| raw.contains("arrived meanwhile"))
        })
        .await;
    delivery.abort();
    let _ = delivery.await;

    harness.fake.set_default("trigger", Scripted::ok());
    let tried = harness.fake.received_named("trigger").len();
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let sent = harness.fake.wait_for("trigger", tried + 2, WAIT).await;
    delivery.abort();
    let (resumed, next) = (&sent[tried], &sent[tried + 1]);
    assert_eq!(resumed["idempotency_key"], first["idempotency_key"]);
    assert_eq!(resumed["payload"], first["payload"]);
    assert_ne!(next["idempotency_key"], first["idempotency_key"]);
    assert_eq!(reasons(next), vec!["arrived meanwhile"]);
}

/// News pushed inside the coalesce window, or while no delivery
/// runs at all, is saved before `push` returns and delivered after a stop.
#[tokio::test]
async fn news_inside_the_coalesce_window_survives_a_stop() {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 400;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, violation("finished"))
        .await
        .unwrap();
    triggers::push(&harness.ctx, violation("cancelled"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    delivery.abort();
    let _ = delivery.await;
    assert!(harness.fake.received_named("trigger").is_empty());
    assert_eq!(triggers::held_news(&harness.ctx).await.unwrap(), 2);

    triggers::push(&harness.ctx, violation("while stopped"))
        .await
        .unwrap();
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let sent = harness.fake.wait_for("trigger", 1, WAIT).await;
    harness
        .eventually("the delivered news is no longer held", || async {
            triggers::held_news(&harness.ctx).await.unwrap() == 0
        })
        .await;
    delivery.abort();
    assert_eq!(
        actions(&sent[0]),
        ["finished", "cancelled", "while stopped"]
    );
    let store = supervisor::store_id(&harness.ctx).await.unwrap();
    assert_eq!(sent[0]["idempotency_key"], format!("trg_{store}_1_3"));
}

/// News piling up behind a batch in flight is compacted when it
/// is keyed, while the batch in flight goes out exactly as it was keyed.
#[tokio::test]
async fn a_long_backlog_is_compacted_behind_an_untouched_batch_in_flight() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    harness
        .fake
        .set_default("trigger", Scripted::refuse("internal", "busy"));
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("in flight"))
        .await
        .unwrap();
    let first = harness.fake.wait_for("trigger", 1, WAIT).await[0].clone();
    let answer = TriggerEvent::OperatorAnswer {
        question: QuestionId::new(),
        text: "ship it".into(),
    };
    triggers::push(&harness.ctx, answer).await.unwrap();
    for n in 0..15 {
        triggers::push(&harness.ctx, budget(&n.to_string()))
            .await
            .unwrap();
    }
    harness.fake.set_default("trigger", Scripted::ok());
    let tried = harness.fake.received_named("trigger").len();
    let sent = harness.fake.wait_for("trigger", tried + 2, WAIT).await;
    delivery.abort();
    let resumed = &sent[tried];
    assert_eq!(resumed["idempotency_key"], first["idempotency_key"]);
    assert_eq!(
        resumed["payload"], first["payload"],
        "the keyed batch is untouched"
    );

    let next = &sent[tried + 1];
    let mut expected = vec!["news_digest", "operator_answer"];
    expected.extend(["budget"; 9]);
    assert_eq!(types(next), expected);
    let digest = &next["payload"]["events"][0];
    assert_eq!(digest["dropped"], json!({"budget": 6}));
    assert!(digest["note"].as_str().unwrap().contains("list_queue"));
    assert_eq!(
        next["payload"]["events"][2]["scope"], "6",
        "the newest are kept"
    );
    let store = supervisor::store_id(&harness.ctx).await.unwrap();
    assert_eq!(next["idempotency_key"], format!("trg_{store}_2_12"));
}

/// A fresh orchestrator generation gets even a short backlog
/// compacted, so news it inherits about settled items does not reach it.
#[tokio::test]
async fn a_fresh_orchestrator_generation_gets_its_held_news_collapsed() {
    let harness = harness().await;
    let mut created = fake::agent(ORCH, None);
    created["status"] = json!({"state": "ready"});
    let fake = &harness.fake;
    fake.script("create_agent", Scripted::refuse("internal", "down"));
    fake.script("create_agent", Scripted::reply("agent", created));
    live_orchestrator(&harness);
    fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("in flight"))
        .await
        .unwrap();
    fake.wait_for("create_agent", 1, WAIT).await;
    let settled = cancelled_item(&harness.ctx, harness.dir.path()).await;
    let stale = TriggerEvent::ItemBlocked {
        item: settled,
        reason: "dependency failed".into(),
    };
    triggers::push(&harness.ctx, stale).await.unwrap();
    triggers::push(&harness.ctx, budget("daily")).await.unwrap();
    let sent = fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    assert_eq!(fake.received_named("create_agent").len(), 2);
    assert_eq!(
        reasons(&sent[0]),
        ["in flight"],
        "the keyed batch is untouched"
    );
    assert_eq!(types(&sent[1]), ["news_digest", "budget"]);
    let digest = &sent[1]["payload"]["events"][0];
    assert_eq!(digest["dropped"], json!({"item_blocked": 1}));
    assert!(
        harness
            .kv("trigger_outbox")
            .await
            .is_some_and(|raw| !raw.contains(r#""collapse":true"#)),
        "only the first batch after the new generation is collapsed"
    );
}

/// News pushed after a fresh generation started is news to it,
/// not a backlog it inherited, so a short batch of it goes out whole.
#[tokio::test]
async fn news_after_a_fresh_generation_started_is_not_collapsed() {
    let harness = harness().await;
    let mut created = fake::agent(ORCH, None);
    created["status"] = json!({"state": "ready"});
    harness
        .fake
        .script("create_agent", Scripted::reply("agent", created));
    live_orchestrator(&harness);
    harness
        .fake
        .set_default("trigger", Scripted::refuse("internal", "busy"));
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(&harness.ctx, blocked("in flight"))
        .await
        .unwrap();
    let first = harness.fake.wait_for("trigger", 1, WAIT).await[0].clone();
    assert_eq!(harness.fake.received_named("create_agent").len(), 1);
    let settled = cancelled_item(&harness.ctx, harness.dir.path()).await;
    let stale = TriggerEvent::ItemBlocked {
        item: settled,
        reason: "dependency failed".into(),
    };
    triggers::push(&harness.ctx, stale).await.unwrap();
    triggers::push(&harness.ctx, budget("daily")).await.unwrap();
    harness.fake.set_default("trigger", Scripted::ok());
    let tried = harness.fake.received_named("trigger").len();
    let sent = harness.fake.wait_for("trigger", tried + 2, WAIT).await;
    delivery.abort();
    assert_eq!(sent[tried]["payload"], first["payload"]);
    assert_eq!(types(&sent[tried + 1]), ["item_blocked", "budget"]);
}

/// News pushed while no batch goes out is compacted as it is
/// pushed, so the outbox stays bounded and every count reaches the digest.
#[tokio::test]
async fn news_held_behind_a_stuck_batch_stays_bounded() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    live_orchestrator(&harness);
    let answer = TriggerEvent::OperatorAnswer {
        question: QuestionId::new(),
        text: "ship it".into(),
    };
    triggers::push(&harness.ctx, answer.clone()).await.unwrap();
    for n in 0..10 * triggers::MAX_PENDING {
        triggers::push(&harness.ctx, budget(&n.to_string()))
            .await
            .unwrap();
        let held = triggers::held(&harness.ctx).await.unwrap();
        assert!(held.len() <= triggers::MAX_PENDING, "{} held", held.len());
    }
    let held = triggers::held(&harness.ctx).await.unwrap();
    assert!(held.contains(&answer), "an answer is never dropped");

    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let sent = harness.fake.wait_for("trigger", 1, WAIT).await;
    delivery.abort();
    let mut expected = vec!["news_digest", "operator_answer"];
    expected.extend(["budget"; 9]);
    assert_eq!(types(&sent[0]), expected);
    let pushed = 10 * triggers::MAX_PENDING;
    let digest = &sent[0]["payload"]["events"][0];
    assert_eq!(digest["dropped"], json!({ "budget": pushed - 9 }));
    let newest = &sent[0]["payload"]["events"][10]["scope"];
    assert_eq!(newest, &json!((pushed - 1).to_string()));
}

/// The quiet `item_finished` news of the cancelled `item`.
fn routine(item: &QueueItemId) -> TriggerEvent {
    TriggerEvent::ItemFinished {
        item: item.clone(),
        title: "bump a dependency".into(),
        state: "cancelled".into(),
        summary: "a long account the list leaves out".into(),
        pr_url: None,
        cost_usd: 0.25,
        origin: None,
        lesson_hint: None,
    }
}

async fn delivering(harness: &Harness) -> tokio::task::JoinHandle<Result<(), ArchonError>> {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    // As the supervisor does for a generation it creates.
    triggers::collapse_for_new_orchestrator(&harness.ctx)
        .await
        .unwrap();
    live_orchestrator(harness);
    harness.fake.set_default("trigger", Scripted::ok());
    tokio::spawn(triggers::deliver_loop(harness.ctx.clone()))
}

#[tokio::test]
async fn quiet_news_sends_no_trigger_and_rides_with_the_next_one() {
    let harness = harness().await;
    let delivery = delivering(&harness).await;
    let item = cancelled_item(&harness.ctx, harness.dir.path()).await;
    triggers::push_quiet(&harness.ctx, routine(&item))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        harness.fake.received_named("trigger").is_empty(),
        "six coalesce windows passed and a routine outcome woke the orchestrator"
    );
    assert_eq!(triggers::held_news(&harness.ctx).await.unwrap(), 1);

    triggers::push(&harness.ctx, blocked("needs a decision"))
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 1, WAIT).await;
    delivered(&harness).await;
    delivery.abort();
    assert_eq!(types(&sent[0]), ["item_blocked", "items_finished"]);
    let listed = &sent[0]["payload"]["events"][1]["items"];
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
    assert_eq!(listed[0]["item"], item.as_str());
    assert_eq!(listed[0]["state"], "cancelled");
    assert_eq!(listed[0]["cost_usd"], 0.25);
    assert_eq!(listed[0].get("summary"), None, "the list is compact");
}

#[tokio::test]
async fn a_flush_delivers_quiet_news_at_once_and_only_when_there_is_some() {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 60_000;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    let delivery = delivering(&harness).await;
    triggers::flush_quiet(&harness.ctx).await.unwrap();
    let item = cancelled_item(&harness.ctx, harness.dir.path()).await;
    triggers::push_quiet(&harness.ctx, routine(&item))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        harness.fake.received_named("trigger").is_empty(),
        "a flush of nothing sends nothing, and does not flush what comes later"
    );

    triggers::flush_quiet(&harness.ctx).await.unwrap();
    let sent = harness.fake.received_named("trigger");
    assert_eq!(
        sent.len(),
        1,
        "Aether has the news when the flush returns, before any operator message"
    );
    delivery.abort();
    assert_eq!(
        types(&sent[0]),
        ["items_finished"],
        "sent without the minute-long coalesce window"
    );
}

/// A flush waits behind the batch already in flight, then for its own.
#[tokio::test]
async fn a_flush_returns_once_the_batch_in_flight_and_the_held_news_are_delivered() {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 60_000;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    let delivery = delivering(&harness).await;
    triggers::push(&harness.ctx, blocked("needs a decision"))
        .await
        .unwrap();
    let item = cancelled_item(&harness.ctx, harness.dir.path()).await;
    triggers::push_quiet(&harness.ctx, routine(&item))
        .await
        .unwrap();
    triggers::push(&harness.ctx, violation("cancelled"))
        .await
        .unwrap();

    triggers::flush_quiet(&harness.ctx).await.unwrap();
    let sent = harness.fake.received_named("trigger");
    delivery.abort();
    let told: Vec<&str> = sent.iter().flat_map(|trigger| types(trigger)).collect();
    assert!(told.contains(&"items_finished"), "{told:?}");
    assert!(told.contains(&"policy_violation"), "{told:?}");
    assert_eq!(triggers::held_news(&harness.ctx).await.unwrap(), 0);
}

/// News the operator waits on, or must act on, goes out at once; other
/// waking news still waits the window for more to share its turn.
#[tokio::test]
async fn news_the_operator_waits_on_skips_the_coalesce_window() {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 60_000;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    let delivery = delivering(&harness).await;
    triggers::push(&harness.ctx, violation("cancelled"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        harness.fake.received_named("trigger").is_empty(),
        "routine news waits the window"
    );

    triggers::push(&harness.ctx, blocked("needs a decision"))
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 1, WAIT).await;
    assert_eq!(types(&sent[0]), ["policy_violation", "item_blocked"]);
    delivered(&harness).await;

    let answer = TriggerEvent::OperatorAnswer {
        question: archon_core::id::QuestionId::new(),
        text: "use the staging database".into(),
    };
    triggers::push(&harness.ctx, answer).await.unwrap();
    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivery.abort();
    assert_eq!(
        types(&sent[1]),
        ["operator_answer"],
        "an answer opens no window of its own"
    );
}

async fn set_directive(harness: &Harness, text: &str) -> Directive {
    let directive =
        Directive::new(text, DirectiveSource::Operator, None, archon_core::now()).unwrap();
    let mut store = harness.ctx.store.lock().await;
    store.insert_directive(&directive).unwrap();
    directive
}

/// Wait until the orchestrator has every batch: none is in flight or held.
async fn delivered(harness: &Harness) {
    let all_sent = || async { triggers::held_news(&harness.ctx).await.unwrap() == 0 };
    harness
        .eventually("every batch is delivered", all_sent)
        .await;
}

/// Push waking news and return the payload of the trigger it leaves in.
async fn next_payload(harness: &Harness, sent_before: usize, reason: &str) -> Value {
    triggers::push(&harness.ctx, blocked(reason)).await.unwrap();
    let sent = harness
        .fake
        .wait_for("trigger", sent_before + 1, WAIT)
        .await;
    assert_eq!(sent.len(), sent_before + 1, "one trigger per push here");
    delivered(harness).await;
    sent[sent_before]["payload"].clone()
}

#[tokio::test]
async fn directives_are_stated_in_full_once_and_then_only_as_changes() {
    let harness = harness().await;
    let delivery = delivering(&harness).await;
    let first = set_directive(&harness, "no yolo today").await;

    let full = next_payload(&harness, 0, "one").await;
    assert_eq!(full["directives"][0]["text"], "no yolo today");
    assert_eq!(full["directives_in_force"], 1);
    assert_eq!(full.get("directive_changes"), None);

    let unchanged = next_payload(&harness, 1, "two").await;
    assert_eq!(unchanged.get("directives"), None, "told once: {unchanged}");
    assert_eq!(unchanged.get("directive_changes"), None);
    assert_eq!(
        unchanged["directives_in_force"], 1,
        "the count is always there"
    );

    set_directive(&harness, "merge docs fixes yourself").await;
    let added = next_payload(&harness, 2, "three").await;
    assert_eq!(added.get("directives"), None);
    let set = &added["directive_changes"]["set"];
    assert_eq!(set.as_array().map(Vec::len), Some(1), "{added}");
    assert_eq!(set[0]["text"], "merge docs fixes yourself");
    assert_eq!(added["directives_in_force"], 2);

    let cleared = {
        let mut store = harness.ctx.store.lock().await;
        store.clear_directives(Some(&first.id)).unwrap()
    };
    assert_eq!(cleared, 1);
    let removed = next_payload(&harness, 3, "four").await;
    assert_eq!(
        removed["directive_changes"]["cleared"][0]["text"],
        "no yolo today"
    );
    assert_eq!(removed["directive_changes"].get("set"), None);
    assert_eq!(removed["directives_in_force"], 1);
    delivery.abort();
}

#[tokio::test]
async fn a_new_generation_and_a_compacted_context_are_told_every_directive_again() {
    let harness = harness().await;
    let delivery = delivering(&harness).await;
    set_directive(&harness, "no yolo today").await;
    let full = next_payload(&harness, 0, "one").await;
    assert_eq!(full["directives_in_force"], 1);

    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "2").await;
    triggers::collapse_for_new_orchestrator(&harness.ctx)
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    delivered(&harness).await;
    assert_eq!(types(&sent[1]), ["directives_restated"]);
    assert_eq!(sent[1]["payload"]["directives"][0]["text"], "no yolo today");
    let reason = sent[1]["payload"]["events"][0]["reason"].as_str().unwrap();
    assert!(reason.contains("new orchestrator generation"), "{reason}");

    let unchanged = next_payload(&harness, 2, "two").await;
    assert_eq!(unchanged.get("directives"), None, "generation 2 was told");

    triggers::restate_directives(&harness.ctx, "your context was compacted")
        .await
        .unwrap();
    let sent = harness.fake.wait_for("trigger", 4, WAIT).await;
    assert_eq!(types(&sent[3]), ["directives_restated"]);
    assert_eq!(sent[3]["payload"]["directives"][0]["text"], "no yolo today");
    assert_eq!(
        sent[3]["payload"]["events"][0]["reason"],
        "your context was compacted"
    );
    delivery.abort();
}

/// Its prompt says every trigger carries the directives in force.
#[tokio::test]
async fn an_orchestrator_an_earlier_archon_started_gets_every_directive_each_time() {
    let harness = harness().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "3").await;
    live_orchestrator(&harness);
    harness.fake.set_default("trigger", Scripted::ok());
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    set_directive(&harness, "no yolo today").await;
    for (sent_before, reason) in [(0, "one"), (1, "two")] {
        let payload = next_payload(&harness, sent_before, reason).await;
        assert_eq!(payload["directives"][0]["text"], "no yolo today");
        assert_eq!(payload.get("directive_changes"), None, "{payload}");
    }
    delivery.abort();
}

#[tokio::test]
async fn directives_lost_twice_before_a_trigger_leaves_are_restated_once() {
    let harness = harness().await;
    set_directive(&harness, "no yolo today").await;
    for reason in ["your context was compacted", "you were restarted"] {
        triggers::restate_directives(&harness.ctx, reason)
            .await
            .unwrap();
    }
    let held = triggers::held(&harness.ctx).await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let TriggerEvent::DirectivesRestated { reason } = &held[0] else {
        panic!("held {held:?}");
    };
    assert_eq!(reason, "your context was compacted");
    assert!(triggers::due(&harness.ctx).await.unwrap(), "it wakes");
}

#[tokio::test]
async fn nothing_is_restated_when_no_directive_is_in_force() {
    let harness = harness().await;
    let delivery = delivering(&harness).await;
    triggers::restate_directives(&harness.ctx, "your context was compacted")
        .await
        .unwrap();
    triggers::collapse_for_new_orchestrator(&harness.ctx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(harness.fake.received_named("trigger").is_empty());
    let payload = next_payload(&harness, 0, "one").await;
    assert_eq!(
        payload["directives"],
        json!([]),
        "a first trigger says there are none"
    );
    assert_eq!(payload["directives_in_force"], 0);
    delivery.abort();
}
