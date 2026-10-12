//! The event pump end to end against the fake aetherd: the quiet chat,
//! the policy watchdog, orchestrator revival and restarts, attribution of
//! stage events, settles racing a spawn, reconnects and replays.

mod link_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::aether_ids::AetherTaskId;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{FeedItemId, QueueItemId};
use archon_core::model::{Effort, ModelRef};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::run::{StageRun, StageState};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{handlers, pump, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, WAIT, running_stage, spawning_stage};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";
const CHILD: &str = "agt_0101";
const CHILD_2: &str = "agt_0102";
const STAGE: &str = "agt_0201";
const STAGE_TASK: &str = "tsk_0201";
const DENIED: (&str, &str) = ("claude-code", "claude-haiku-4-5");

/// Agents the fake's `get_agent` knows, editable while the pump runs.
#[derive(Clone, Default)]
struct Agents(Arc<Mutex<HashMap<String, Value>>>);

impl Agents {
    fn install(harness: &Harness) -> Self {
        let agents = Self::default();
        let table = agents.clone();
        harness.fake.set_responder(
            "get_agent",
            Arc::new(
                move |cmd| match table.get(cmd["id"].as_str().unwrap_or_default()) {
                    Some(agent) => Scripted::reply("agent", agent),
                    None => Scripted::refuse("not_found", "no such agent"),
                },
            ),
        );
        agents
    }

    fn get(&self, id: &str) -> Option<Value> {
        self.0.lock().unwrap().get(id).cloned()
    }

    fn set(&self, id: &str, state: &str) -> Value {
        let mut agent = fake::agent(id, None);
        agent["status"] = json!({ "state": state });
        self.0.lock().unwrap().insert(id.to_owned(), agent.clone());
        agent
    }

    /// Aether's `restart`: `from` points at `to`, a ready orchestrator.
    fn restart(&self, from: &str, to: &str) {
        let mut successor = self.set(to, "ready");
        successor["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
        let mut table = self.0.lock().unwrap();
        table.insert(to.to_owned(), successor);
        table.get_mut(from).expect("a known agent")["restarted_as"] = json!(to);
    }

    fn set_effort(&self, id: &str, effort: &str) {
        let mut agent = self.set(id, "running");
        agent["config"] = json!({ "effort": effort });
        self.0.lock().unwrap().insert(id.to_owned(), agent);
    }
}

/// A pump following a live orchestrator, with both streams subscribed.
async fn running_pump(harness: &Harness) -> (Agents, JoinHandle<Result<(), ArchonError>>) {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let agents = Agents::install(harness);
    agents.set(ORCH, "running");
    let pump = tokio::spawn(pump::run(harness.ctx.clone(), harness.fake.endpoint()));
    wait_for_streams(harness, 1).await;
    (agents, pump)
}

async fn wait_for_streams(harness: &Harness, connection: u64) {
    harness
        .eventually("both streams subscribed", || async {
            harness.fake.connections_accepted() >= connection
                && harness.fake.live_subscriptions() == 2
        })
        .await;
}

/// Push an event and wait until the pump persisted the cursor past it.
async fn push(harness: &Harness, event: Value, cursor: &str) -> u64 {
    let seq = harness.fake.push_event(event);
    harness
        .eventually("the pump handled the event", || async {
            let saved = harness.kv(cursor).await;
            saved
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|s| s >= seq)
        })
        .await;
    seq
}

async fn orch(harness: &Harness, kind: Value) -> u64 {
    push(
        harness,
        fake::event(Some(ORCH), None, kind),
        keys::CURSOR_ORCHESTRATOR,
    )
    .await
}

/// An item lands in the orchestrator's inbox; only a user `message` is the
/// operator's.
fn delivered(seq: u64, kind: Value) -> Value {
    json!({"type": "inbox.delivered", "item": {
        "id": format!("inb_{seq}"), "agent_id": ORCH, "seq": seq,
        "delivered_at": "2026-09-26T10:00:00Z", "kind": kind}})
}

fn trigger_delivered(seq: u64) -> Value {
    delivered(
        seq,
        json!({"kind": "external_trigger", "source": "archon",
                          "idempotency_key": format!("trg_{seq}"), "payload": {}}),
    )
}

/// A turn reads its inbox up to `seq`.
fn consumed(up_to: u64) -> Value {
    json!({"type": "inbox.consumed", "up_to": up_to, "items": []})
}

fn said(text: &str) -> Value {
    json!({"type": "agent.message", "role": "assistant", "content": [{"type": "text", "text": text}]})
}

fn tool_call() -> Value {
    json!({"type": "agent.message", "role": "assistant",
           "content": [{"type": "tool_use", "id": "t1", "name": "queue_status", "input": {}}]})
}

fn completed() -> Value {
    json!({"type": "agent.turn_completed", "turn": 1, "invocation": "inv_1", "next": "ready"})
}

fn created(agent: &str, parent: &str, model: (&str, &str)) -> Value {
    let kind = json!({"type": "agent.created", "name": agent,
                      "model": {"provider": model.0, "model": model.1}});
    let mut event = fake::event(Some(agent), None, kind);
    event["parent_agent_id"] = json!(parent);
    event
}

/// A fleet event Archon ignores; handling it proves the replay before it
/// was handled too.
fn stranger_usage() -> Value {
    let usage = json!({"type": "agent.usage_updated", "total": {"cost_usd": 9.0}});
    fake::event(Some("agt_0999"), None, usage)
}

/// The operator sends a message, which Aether delivers as inbox item `seq`.
async fn say(harness: &Harness, seq: u64, text: &str) {
    harness
        .fake
        .script("send_message", Scripted::reply("message_sent", json!({})));
    let item = FeedItemId::new();
    supervisor::deliver_operator_message(&harness.ctx, &item, text)
        .await
        .unwrap();
    let message = json!({"kind": "message", "message_id": format!("msg_{seq}"),
                         "from": {"actor": "user"},
                         "content": [{"type": "text", "text": text}]});
    orch(harness, delivered(seq, message)).await;
}

async fn awaiting_reply(harness: &Harness) -> bool {
    let saved = harness.kv(archon_daemon::feed::REPLY_STATE_KEY).await;
    let tracker = archon_daemon::feed::restore_replies(saved.as_deref()).unwrap();
    assert_eq!(
        tracker,
        *harness.ctx.replies.lock().await,
        "persisted as held"
    );
    tracker.awaiting_reply()
}

async fn hangup_and_replay(harness: &Harness, cursor: &str) {
    harness.set_kv(cursor, "0").await;
    let before = harness.fake.connections_accepted();
    harness.fake.hangup(None);
    wait_for_streams(harness, before + 1).await;
}

#[tokio::test]
async fn only_the_answer_to_an_operator_message_reaches_the_chat() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    // A trigger-woken turn: its text is the orchestrator's own business.
    orch(&harness, trigger_delivered(1)).await;
    orch(&harness, consumed(1)).await;
    orch(&harness, said("Noted item 3 finished; nothing to tell.")).await;
    orch(&harness, completed()).await;
    assert!(harness.orchestrator_replies().await.is_empty());

    say(&harness, 2, "What is running?").await;
    assert!(awaiting_reply(&harness).await);
    orch(&harness, consumed(2)).await;
    orch(&harness, tool_call()).await;
    orch(&harness, said("Let me check the queue.")).await;
    let child_text = fake::event(Some(CHILD), None, said("child chatter"));
    let mut child_text = child_text;
    child_text["parent_agent_id"] = json!(ORCH);
    harness.fake.push_event(child_text);
    orch(&harness, tool_call()).await;
    orch(&harness, said("Two items are running: A and B.")).await;
    orch(&harness, said("   ")).await;
    orch(&harness, completed()).await;

    orch(&harness, trigger_delivered(3)).await;
    orch(&harness, consumed(3)).await;
    orch(&harness, said("Later trigger work.")).await;
    orch(&harness, completed()).await;
    pump.abort();
    assert_eq!(
        harness.orchestrator_replies().await,
        vec!["Two items are running: A and B."]
    );
    assert!(!awaiting_reply(&harness).await);
}

#[tokio::test]
async fn a_message_sent_mid_turn_is_answered_by_the_turn_that_consumes_it() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    orch(&harness, trigger_delivered(1)).await;
    say(&harness, 2, "Pause the queue?").await;
    // The trigger turn starts after the message arrived but reads only item 1.
    orch(&harness, consumed(1)).await;
    orch(&harness, said("Handling a trigger.")).await;
    orch(&harness, completed()).await;
    assert!(
        harness.orchestrator_replies().await.is_empty(),
        "that turn never saw the message"
    );

    orch(&harness, consumed(2)).await;
    orch(&harness, said("Paused.")).await;
    orch(&harness, completed()).await;
    pump.abort();
    assert_eq!(harness.orchestrator_replies().await, vec!["Paused."]);
}

#[tokio::test]
async fn an_answer_in_progress_survives_an_archond_restart_and_a_replay() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    say(&harness, 1, "Is item 4 done?").await;
    orch(&harness, consumed(1)).await;
    orch(&harness, said("Checking.")).await;
    pump.abort();
    let _ = pump.await;
    // A fresh archond starts from what the store kept.
    *harness.ctx.replies.lock().await = Default::default();
    let pump = tokio::spawn(pump::run(harness.ctx.clone(), harness.fake.endpoint()));
    wait_for_streams(&harness, 2).await;
    orch(&harness, said("Yes, it merged.")).await;
    let seq = orch(&harness, completed()).await;

    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    harness
        .eventually("the replay was handled", || async {
            harness.kv(keys::CURSOR_ORCHESTRATOR).await.as_deref() == Some(&*seq.to_string())
        })
        .await;
    pump.abort();
    assert_eq!(
        harness.orchestrator_replies().await,
        vec!["Yes, it merged."]
    );
    assert!(!awaiting_reply(&harness).await);
}

#[tokio::test]
async fn orchestrator_questions_are_asked_once_even_when_replayed() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let kind = json!({"type": "question.asked", "question": "Ship it now?"});
    let seq = orch(&harness, kind).await;
    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    harness
        .eventually("the replay was handled", || async {
            harness.kv(keys::CURSOR_ORCHESTRATOR).await.as_deref() == Some(&*seq.to_string())
        })
        .await;
    pump.abort();
    let questions: Vec<_> = harness
        .feed()
        .await
        .into_iter()
        .filter_map(|e| match e {
            FeedEntry::Question(q) => Some(q),
            _ => None,
        })
        .collect();
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert_eq!(
        questions[0].number, 1,
        "the feed shows the question numbered"
    );
    let aether = questions[0].aether.as_ref().unwrap();
    assert_eq!((aether.agent.as_str(), aether.seq), (ORCH, seq));
}

#[tokio::test]
async fn an_orchestrator_failure_wakes_it_then_a_cancellation_restarts_it() {
    let harness = Harness::new().await;
    let (agents, pump) = running_pump(&harness).await;
    let woken = agents.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| Scripted::reply("agent", woken.set(ORCH, "ready"))),
    );
    let successor = agents.clone();
    harness.fake.set_responder(
        "restart",
        Arc::new(move |_| Scripted::reply("agent", successor.set(ORCH_2, "ready"))),
    );

    agents.set(ORCH, "failed");
    orch(
        &harness,
        json!({"type": "agent.failed", "error": "model overloaded"}),
    )
    .await;
    assert_eq!(harness.fake.received_named("wake").len(), 1);
    assert!(harness.fake.received_named("restart").is_empty());
    // Its next turn goes well, which ends the failure streak.
    orch(&harness, completed()).await;

    agents.set(ORCH, "cancelled");
    orch(
        &harness,
        json!({"type": "agent.cancelled", "reason": "operator"}),
    )
    .await;
    harness.fake.wait_for("restart", 1, WAIT).await;
    harness
        .eventually("the pump follows the successor", || async {
            let subscribes = harness.fake.received_named("subscribe");
            subscribes.iter().any(|s| s["query"]["agent"] == ORCH_2)
        })
        .await;
    pump.abort();
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    let notes = harness.system_notes().await;
    assert!(notes.iter().any(|n| n.contains("woken")), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("restarted") && n.contains(ORCH_2)),
        "{notes:?}"
    );
}

/// `trigger` refused for a dead agent, as Aether does, and counted when a
/// live one takes it.
fn triggers_reach_only(harness: &Harness, live: &'static str) -> Arc<AtomicUsize> {
    let taken = Arc::new(AtomicUsize::new(0));
    let count = taken.clone();
    harness.fake.set_responder(
        "trigger",
        Arc::new(move |cmd| {
            if cmd["agent"] != live {
                return Scripted::refuse("invalid_argument", "the agent is failed");
            }
            count.fetch_add(1, Ordering::SeqCst);
            Scripted::ok()
        }),
    );
    taken
}

/// The operator's `/answer`, read off the orchestrator's stream by the
/// pump, is applied once; the same text from an agent is only text.
#[tokio::test]
async fn an_operator_chat_command_is_applied_once_through_the_pump() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let ask = Request::AskOperator {
        text: "Ship to prod?".into(),
        item: None,
        options: vec![],
    };
    let reply = handlers::handle(&harness.ctx, ClientKind::ArchonCli, ask).await;
    let Ok(Reply::Question(asked)) = reply else {
        panic!("asked: {reply:?}")
    };
    let command = format!("/answer {} yes", asked.number);
    let from = |actor: Value| {
        json!({"kind": "message", "message_id": "msg_1", "from": actor,
               "content": [{"type": "text", "text": command}]})
    };
    let open = || async { harness.ctx.store.lock().await.question(&asked.id).unwrap() };
    let peer = json!({"actor": "agent", "id": STAGE});
    orch(&harness, delivered(1, from(peer))).await;
    assert!(open().await.is_open(), "an agent's message answers nothing");
    assert!(triggers::held(&harness.ctx).await.unwrap().is_empty());

    let seq = orch(&harness, delivered(2, from(json!({"actor": "user"})))).await;
    assert_eq!(open().await.answer.as_deref(), Some("yes"));
    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    harness
        .eventually("the replay was handled", || async {
            harness.kv(keys::CURSOR_ORCHESTRATOR).await.as_deref() == Some(&*seq.to_string())
        })
        .await;
    pump.abort();
    let expected = vec![
        TriggerEvent::OperatorAnswer {
            question: asked.id.clone(),
            text: "yes".into(),
        },
        TriggerEvent::ChatAnswer {
            number: Some(asked.number),
            applied: true,
            text: "yes".into(),
        },
    ];
    assert_eq!(triggers::held(&harness.ctx).await.unwrap(), expected);
}

fn item_finished(title: &str) -> TriggerEvent {
    TriggerEvent::ItemFinished {
        item: QueueItemId::new(),
        title: title.into(),
        state: "failed".into(),
        summary: format!("Failed: {title}."),
        pr_url: None,
        cost_usd: 0.0,
        origin: None,
    }
}

async fn held_news(harness: &Harness) -> u32 {
    let reply = handlers::handle(&harness.ctx, ClientKind::ArchonCli, Request::Status)
        .await
        .unwrap();
    let Reply::Status(status) = reply else {
        panic!("expected the status, got {reply:?}");
    };
    status.held_news
}

/// A transient engine error leaves the orchestrator failed, its
/// news piles up, and the operator runs `aether restart`.
#[tokio::test]
async fn an_aether_restart_of_the_orchestrator_is_adopted_and_gets_the_held_news() {
    let harness = Harness::new().await;
    let (agents, pump) = running_pump(&harness).await;
    let still_failed = agents.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| Scripted::reply("agent", still_failed.set(ORCH, "failed"))),
    );
    let taken = triggers_reach_only(&harness, ORCH_2);
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    agents.set(ORCH, "failed");
    let oauth = "claude-code failed: Failed to refresh OAuth token";
    orch(&harness, json!({"type": "agent.failed", "error": oauth})).await;
    triggers::push(&harness.ctx, item_finished("first"))
        .await
        .unwrap();
    triggers::push(&harness.ctx, item_finished("second"))
        .await
        .unwrap();
    harness
        .eventually("the news is held for a later revival", || async {
            harness.ctx.supervisor.next_revival().is_some() && held_news(&harness).await == 2
        })
        .await;

    agents.restart(ORCH, ORCH_2);
    let restarted = json!({"type": "agent.restarted", "from": ORCH, "to": ORCH_2,
                           "transcript_tail": 3});
    orch(&harness, restarted).await;
    harness
        .eventually("the held news reaches the new orchestrator", || async {
            taken.load(Ordering::SeqCst) == 1 && held_news(&harness).await == 0
        })
        .await;
    harness
        .eventually("the pump follows the new orchestrator", || async {
            let subscribes = harness.fake.received_named("subscribe");
            subscribes.iter().any(|s| s["query"]["agent"] == ORCH_2)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    delivery.abort();
    pump.abort();
    assert_eq!(taken.load(Ordering::SeqCst), 1, "delivered exactly once");
    let sent: Vec<Value> = harness.fake.received_named("trigger");
    let delivered = sent.last().unwrap();
    assert_eq!(delivered["agent"], ORCH_2);
    let titles: Vec<&str> = delivered["payload"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["first", "second"], "in order");
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    assert_eq!(harness.ctx.supervisor.next_revival(), None);
    let notes = harness.system_notes().await;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("restarted in Aether") && n.contains(ORCH_2)),
        "{notes:?}"
    );
    assert!(harness.fake.received_named("restart").is_empty());
}

/// Cancel the item that `stage` belongs to, as the operator would.
async fn cancel_item_of(harness: &Harness, stage: &StageRun) {
    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    let cancel = Request::CancelItem {
        item,
        reason: Some("no longer needed".into()),
    };
    handlers::handle(&harness.ctx, ClientKind::ArchonCli, cancel)
        .await
        .expect("cancel");
}

/// `trigger` refused while `ORCH` is failed, as Aether does, and counted
/// once it takes one.
fn triggers_reach_orch_when_live(agents: &Agents, harness: &Harness) -> Arc<AtomicUsize> {
    let taken = Arc::new(AtomicUsize::new(0));
    let (count, agents) = (taken.clone(), agents.clone());
    harness.fake.set_responder(
        "trigger",
        Arc::new(move |_| {
            if agents.get(ORCH).unwrap()["status"]["state"] == "failed" {
                return Scripted::refuse("invalid_argument", "the agent is failed");
            }
            count.fetch_add(1, Ordering::SeqCst);
            Scripted::ok()
        }),
    );
    taken
}

/// While the orchestrator is down an item's outcome still reaches the chat;
/// its news waits and reaches the orchestrator once revival recovers it.
#[tokio::test]
async fn outcomes_show_while_the_orchestrator_is_down_and_backoff_delivers_its_news() {
    let harness = Harness::new().await;
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    let still_failed = agents.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| Scripted::reply("agent", still_failed.set(ORCH, "failed"))),
    );
    let taken = triggers_reach_orch_when_live(&agents, &harness);
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    agents.set(ORCH, "failed");
    orch(
        &harness,
        json!({"type": "agent.failed", "error": "overloaded"}),
    )
    .await;
    let repo = tempfile::tempdir().unwrap();
    let stage = running_stage(&harness.ctx, repo.path(), STAGE, STAGE_TASK).await;
    cancel_item_of(&harness, &stage).await;
    harness
        .eventually("the outcome shows and its news is held", || async {
            let shown = !harness
                .notifications(NotificationKind::Outcome)
                .await
                .is_empty();
            let held = harness.ctx.supervisor.next_revival().is_some();
            shown && held && held_news(&harness).await == 1
        })
        .await;
    assert_eq!(taken.load(Ordering::SeqCst), 0);

    let recovering = agents.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| Scripted::reply("agent", recovering.set(ORCH, "ready"))),
    );
    harness.rewind_backoff(10.0).await;
    harness
        .eventually("the revived orchestrator gets the news", || async {
            taken.load(Ordering::SeqCst) == 1 && held_news(&harness).await == 0
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    delivery.abort();
    pump.abort();
    assert_eq!(taken.load(Ordering::SeqCst), 1, "delivered exactly once");
    let delivered = harness.fake.received_named("trigger").pop().unwrap();
    assert_eq!(delivered["payload"]["events"].as_array().unwrap().len(), 1);
    assert!(harness.fake.received_named("restart").is_empty());
}

#[tokio::test]
async fn the_restart_event_alone_moves_the_pump_to_the_new_orchestrator() {
    let harness = Harness::new().await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(ORCH, "cancelled");
    agents.restart(ORCH, ORCH_2);
    let asked = harness.fake.received_named("get_agent").len();
    let restarted = json!({"type": "agent.restarted", "from": ORCH, "to": ORCH_2,
                           "transcript_tail": 0});
    orch(&harness, restarted).await;
    let asked: Vec<Value> = harness.fake.received_named("get_agent")[asked..].to_vec();
    assert_eq!(asked.len(), 1, "only the successor is looked up: {asked:?}");
    assert_eq!(asked[0]["id"], ORCH_2);
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    harness
        .eventually("the pump follows the new orchestrator", || async {
            let subscribes = harness.fake.received_named("subscribe");
            subscribes.iter().any(|s| s["query"]["agent"] == ORCH_2)
        })
        .await;
    pump.abort();
}

fn deny_model(config: &mut Config) {
    config.models.deny = vec![ModelRef::new(DENIED.0, DENIED.1)];
}

#[tokio::test]
async fn the_watchdog_cancels_an_off_policy_child_once_and_tracks_allowed_ones() {
    let mut config = Config::default();
    deny_model(&mut config);
    let mut harness = Harness::with(config, None).await;
    let (_agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    let fleet = keys::CURSOR_FLEET;
    push(&harness, created(CHILD, ORCH, DENIED), fleet).await;
    push(
        &harness,
        created(CHILD_2, ORCH, ("claude-code", "claude-opus-5-5")),
        fleet,
    )
    .await;
    push(&harness, created("agt_0999", "agt_0998", DENIED), fleet).await;
    hangup_and_replay(&harness, fleet).await;
    push(&harness, stranger_usage(), fleet).await;
    // What the cancelled child spent before the cancel still counts.
    let usage = json!({"type": "agent.usage_updated", "total": {"cost_usd": 0.75}});
    push(&harness, fake::event(Some(CHILD), None, usage), fleet).await;
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], CHILD);
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert!(
        blockers.len() == 1 && blockers[0].contains(CHILD),
        "{blockers:?}"
    );
    let violation = TriggerEvent::PolicyViolation {
        agent: CHILD.into(),
        model: format!("{}/{}", DENIED.0, DENIED.1),
        action: "cancelled".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![violation]);
    assert_eq!(
        harness
            .kv(&format!("link_child_{CHILD_2}"))
            .await
            .as_deref(),
        Some("null")
    );
    assert_eq!(
        harness.kv("link_child_agt_0999").await,
        None,
        "not Archon's agent"
    );
    let (_today, total) = harness
        .ctx
        .store
        .lock()
        .await
        .spend_totals(archon_core::now(), true)
        .unwrap();
    assert_eq!(total, 0.75, "only Archon's agents are counted");
}

#[tokio::test]
async fn the_watchdog_enforces_the_effort_cap_from_the_agent_config() {
    let mut config = Config::default();
    config.models.max_effort = Some(Effort::Medium);
    let harness = Harness::with(config, None).await;
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_effort(CHILD, "high");
    agents.set_effort(CHILD_2, "low");
    let allowed = ("claude-code", "claude-opus-5-5");
    push(&harness, created(CHILD, ORCH, allowed), keys::CURSOR_FLEET).await;
    push(
        &harness,
        created(CHILD_2, ORCH, allowed),
        keys::CURSOR_FLEET,
    )
    .await;
    pump.abort();
    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], CHILD);
    assert!(cancels[0]["reason"].as_str().unwrap().contains("effort"));
}

#[tokio::test]
async fn a_stage_agents_children_follow_its_repositorys_narrower_policy() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let archon_dir = repo.path().join(".archon");
    std::fs::create_dir_all(&archon_dir).unwrap();
    let denied = format!("[models]\ndeny = [\"{}/{}\"]\n", DENIED.0, DENIED.1);
    std::fs::write(archon_dir.join("config.toml"), denied).unwrap();
    let stage = stage_with_task(&harness, repo.path()).await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(STAGE, "running");
    harness.fake.set_default("cancel", Scripted::ok());
    push(&harness, created(CHILD, STAGE, DENIED), keys::CURSOR_FLEET).await;
    pump.abort();
    assert_eq!(harness.fake.received_named("cancel")[0]["agent"], CHILD);
    let feed = harness.feed().await;
    let blocker = feed.iter().find_map(|e| match e {
        FeedEntry::Notification(n) if n.kind == NotificationKind::Blocker => Some(n),
        _ => None,
    });
    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    assert_eq!(blocker.unwrap().item.as_ref(), Some(&item));
}

/// A running stage for `STAGE`, with Aether reporting its task as active.
async fn stage_with_task(harness: &Harness, repo: &Path) -> StageRun {
    let stage = running_stage(&harness.ctx, repo, STAGE, STAGE_TASK).await;
    let task = fake::task(
        STAGE_TASK,
        Some(STAGE),
        "run_0001",
        json!({"state": "active"}),
    );
    harness
        .fake
        .set_default("get_task", Scripted::reply("task", task));
    stage
}

#[tokio::test]
async fn stage_questions_and_spend_are_attributed_to_the_item_and_survive_replay() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = stage_with_task(&harness, repo.path()).await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(STAGE, "running");
    assert_eq!(
        harness.fake.received_named("get_task")[0]["id"],
        STAGE_TASK,
        "reconciled"
    );
    let fleet = keys::CURSOR_FLEET;
    let question = json!({"type": "question.asked", "question": "Which API version?"});
    push(
        &harness,
        fake::event(Some(STAGE), Some(STAGE_TASK), question),
        fleet,
    )
    .await;
    let usage = json!({"type": "agent.usage_updated",
                       "total": {"input_tokens": 1200, "output_tokens": 300, "cost_usd": 0.5}});
    push(&harness, fake::event(Some(STAGE), None, usage), fleet).await;
    hangup_and_replay(&harness, fleet).await;
    push(&harness, stranger_usage(), fleet).await;
    pump.abort();

    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    let questions: Vec<_> = harness
        .feed()
        .await
        .into_iter()
        .filter_map(|e| match e {
            FeedEntry::Question(q) => Some(q),
            _ => None,
        })
        .collect();
    assert_eq!(
        questions.len(),
        1,
        "a replay does not ask again: {questions:?}"
    );
    assert_eq!(questions[0].item.as_ref(), Some(&item));
    let spent = harness
        .ctx
        .store
        .lock()
        .await
        .spend_for_item(&item, true)
        .unwrap();
    assert!((spent - 0.5).abs() < 1e-9, "spent {spent}");
}

#[tokio::test]
async fn an_aether_restart_is_announced_and_the_link_comes_back() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    assert!(
        harness.system_notes().await.is_empty(),
        "a first connect is quiet"
    );
    harness.fake.set_instance_id("fake-instance-2");
    harness
        .fake
        .hangup(Some(("shutting_down", "aetherd restarting")));
    wait_for_streams(&harness, 2).await;
    pump.abort();
    let notes = harness.system_notes().await;
    assert!(
        notes.iter().any(|n| n.contains("Aether restarted")),
        "{notes:?}"
    );
    assert_eq!(
        harness.kv(keys::AETHER_INSTANCE).await.as_deref(),
        Some("fake-instance-2")
    );
}

#[tokio::test]
async fn an_undecodable_event_is_reported_without_spinning() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let bad = json!({"type": "agent.failed", "error": 42});
    harness.fake.push_event(fake::event(Some(STAGE), None, bad));
    harness
        .eventually("the operator hears the fleet stream is stuck", || async {
            harness
                .system_notes()
                .await
                .iter()
                .any(|n| n.contains("fleet"))
        })
        .await;
    pump.abort();
    let resubscribes = harness.fake.received_named("subscribe").len();
    assert!(
        resubscribes <= 6,
        "backoff keeps retries few: {resubscribes}"
    );
}

#[tokio::test]
async fn a_settled_stage_reaches_the_executor_once_even_when_replayed() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = stage_with_task(&harness, repo.path()).await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(STAGE, "running");
    let done = json!({"type": "task.completed",
                      "result": {"summary": "VERDICT: PASS", "artifacts": [], "structured": null}});
    push(
        &harness,
        fake::event(Some(STAGE), Some(STAGE_TASK), done),
        keys::CURSOR_FLEET,
    )
    .await;
    let settled = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert!(settled.state.is_terminal(), "{:?}", settled.state);

    hangup_and_replay(&harness, keys::CURSOR_FLEET).await;
    push(&harness, stranger_usage(), keys::CURSOR_FLEET).await;
    pump.abort();
    let replayed = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(replayed, settled, "a replayed settle changes nothing");
}

#[tokio::test]
async fn reconcile_settles_a_stage_that_finished_while_disconnected() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = running_stage(&harness.ctx, repo.path(), STAGE, STAGE_TASK).await;
    let status = json!({"state": "completed",
                        "result": {"summary": "done", "artifacts": [], "structured": null}});
    let task = fake::task(STAGE_TASK, Some(STAGE), "run_0001", status);
    harness
        .fake
        .set_default("get_task", Scripted::reply("task", task));
    let (_agents, pump) = running_pump(&harness).await;
    pump.abort();
    let settled = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert!(settled.state.is_terminal(), "{:?}", settled.state);
}

#[tokio::test]
async fn reconcile_fails_a_stage_aether_no_longer_knows_as_infra() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = running_stage(&harness.ctx, repo.path(), STAGE, STAGE_TASK).await;
    harness.fake.set_default(
        "get_task",
        Scripted::refuse("not_found", "no such task tsk_0201"),
    );
    let (_agents, pump) = running_pump(&harness).await;
    pump.abort();
    let settled = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    let StageState::Failed { error } = &settled.state else {
        panic!("expected a failed stage: {:?}", settled.state);
    };
    assert!(error.contains("no longer knows"), "{error}");
    assert!(
        harness.system_notes().await.is_empty(),
        "settled, not reported as a failed check"
    );
}

#[tokio::test]
async fn a_stage_agent_that_dies_without_settling_fails_its_stage_as_infra() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = stage_with_task(&harness, repo.path()).await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(STAGE, "failed");
    let died = json!({"type": "agent.failed", "error": "engine crashed"});
    push(
        &harness,
        fake::event(Some(STAGE), None, died),
        keys::CURSOR_FLEET,
    )
    .await;
    pump.abort();
    let settled = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert!(settled.state.is_terminal(), "{:?}", settled.state);
}

/// The Aether run of the pipeline run a spawning stage belongs to.
const SPAWN_RUN: &str = "run_0001";
/// How long a settle may wait for a spawning stage's reply (`SPAWN_GRACE`).
const SPAWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// A settling event for `agent` in Aether run `run`.
fn settled_in(agent: &str, task: &str, run: &str) -> Value {
    let done = json!({"type": "task.completed",
                      "result": {"summary": "done", "artifacts": [], "structured": null}});
    let mut event = fake::event(Some(agent), Some(task), done);
    event["run"] = json!(run);
    event
}

/// Push a fleet event and return how long the pump took to handle it.
async fn handled_in(harness: &Harness, event: Value) -> std::time::Duration {
    let started = tokio::time::Instant::now();
    push(harness, event, keys::CURSOR_FLEET).await;
    started.elapsed()
}

/// While a stage is spawning, the settle of an agent in another Aether
/// run (another stage's child, the orchestrator's task, anyone's agent)
/// cannot be that stage's, so it does not hold up the fleet stream.
#[tokio::test]
async fn an_unknown_agent_in_another_run_does_not_wait_for_a_spawn() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, Some("agt_0300")).await;
    let (_agents, pump) = running_pump(&harness).await;
    let elsewhere = settled_in("agt_0999", "tsk_0999", "run_0002");
    let took = handled_in(&harness, elsewhere).await;
    let died = json!({"type": "agent.failed", "error": "crashed"});
    let mut stranger_died = fake::event(Some("agt_0998"), None, died);
    stranger_died["run"] = json!("run_0003");
    let took_too = handled_in(&harness, stranger_died).await;
    pump.abort();
    assert!(took < SPAWN_GRACE / 2, "a settle waited {took:?}");
    assert!(took_too < SPAWN_GRACE / 2, "a death waited {took_too:?}");
    let still = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(still.state, StageState::Spawning);
}

/// A stage not yet bound to its node was never sent, so no event can
/// be its agent's, even one in its own Aether run.
#[tokio::test]
async fn an_unsent_spawn_holds_up_no_event() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, None).await;
    let (_agents, pump) = running_pump(&harness).await;
    let took = handled_in(&harness, settled_in("agt_0999", "tsk_0999", SPAWN_RUN)).await;
    pump.abort();
    assert!(took < SPAWN_GRACE / 2, "waited {took:?}");
}

/// A run node's own events, a retired node's `agent.cancelled` above
/// all, are recognised and skipped at once, even in the Aether run of a
/// stage that is spawning under it.
#[tokio::test]
async fn a_run_nodes_events_are_skipped_without_waiting() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let node = "agt_0300";
    let stage = spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, Some(node)).await;
    // The executor indexes every node agent it learns of, and keeps it.
    harness
        .set_kv(&format!("run_node_agent:{node}"), stage.run.as_str())
        .await;
    let (_agents, pump) = running_pump(&harness).await;
    let retired = json!({"type": "agent.cancelled", "reason": "pipeline run finished"});
    let mut cancelled = fake::event(Some(node), None, retired);
    cancelled["run"] = json!(SPAWN_RUN);
    let took = handled_in(&harness, cancelled).await;
    pump.abort();
    assert!(took < SPAWN_GRACE / 2, "a node event waited {took:?}");
    let still = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(still.state, StageState::Spawning, "a node is not its stage");
}

/// The wait still covers the race it exists for: a settle in a spawning
/// stage's Aether run, arriving before its spawn reply is recorded, settles
/// the stage once the reply is.
#[tokio::test]
async fn a_settle_racing_its_spawn_reply_waits_for_it() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, Some("agt_0300")).await;
    let (_agents, pump) = running_pump(&harness).await;
    harness
        .fake
        .push_event(settled_in(STAGE, STAGE_TASK, SPAWN_RUN));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    {
        let mut store = harness.ctx.store.lock().await;
        let mut spawned = store.stage_run(&stage.id).unwrap();
        assert_eq!(spawned.state, StageState::Spawning, "not settled early");
        spawned.aether_agent = Some(link_common::agent_id(STAGE));
        spawned.aether_task = Some(AetherTaskId::parse(STAGE_TASK).unwrap());
        spawned.state = StageState::Running;
        store.upsert_stage_run(&spawned).unwrap();
    }
    harness
        .eventually("the racing settle reached its stage", || async {
            let store = harness.ctx.store.lock().await;
            store.stage_run(&stage.id).unwrap().state.is_terminal()
        })
        .await;
    pump.abort();
}

const GRANDCHILD: &str = "agt_0103";
const GREAT_GRANDCHILD: &str = "agt_0104";
const ALLOWED: (&str, &str) = ("claude-code", "claude-opus-5-5");

fn usage_of(agent: &str, cost_usd: f64) -> Value {
    let usage = json!({"type": "agent.usage_updated", "total": {"cost_usd": cost_usd}});
    fake::event(Some(agent), None, usage)
}

async fn spent_in_total(harness: &Harness) -> f64 {
    let store = harness.ctx.store.lock().await;
    store.spend_totals(archon_core::now(), true).unwrap().1
}

/// A stage agent that delegates before its spawn reply is recorded,
/// and its children's children, are all policed and billed to the item.
#[tokio::test]
async fn children_delegated_before_their_stage_is_recorded_are_policed_at_every_depth() {
    let mut config = Config::default();
    deny_model(&mut config);
    let harness = Harness::with(config, None).await;
    let repo = tempfile::tempdir().unwrap();
    let node = "agt_0300";
    let stage = spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, Some(node)).await;
    harness
        .set_kv(&format!("run_node_agent:{node}"), stage.run.as_str())
        .await;
    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    let (_agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    let fleet = keys::CURSOR_FLEET;
    push(&harness, created(STAGE, node, ALLOWED), fleet).await;
    push(&harness, created(CHILD, STAGE, ALLOWED), fleet).await;
    push(&harness, created(GRANDCHILD, CHILD, DENIED), fleet).await;
    push(
        &harness,
        created(GREAT_GRANDCHILD, GRANDCHILD, ALLOWED),
        fleet,
    )
    .await;
    for (agent, cost) in [
        (STAGE, 0.25),
        (CHILD, 0.5),
        (GRANDCHILD, 1.0),
        (GREAT_GRANDCHILD, 2.0),
    ] {
        push(&harness, usage_of(agent, cost), fleet).await;
    }
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], GRANDCHILD);
    let owned = Some(format!("\"{item}\""));
    assert_eq!(harness.kv(&format!("link_child_{CHILD}")).await, owned);
    assert_eq!(
        harness.kv(&format!("link_child_{GREAT_GRANDCHILD}")).await,
        owned
    );
    assert_eq!(
        harness.kv(&format!("link_child_{STAGE}")).await,
        None,
        "a stage agent is not a watched child, so its settle still waits for its spawn"
    );
    let still = harness.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(still.state, StageState::Spawning);
    let spent = harness
        .ctx
        .store
        .lock()
        .await
        .spend_for_item(&item, true)
        .unwrap();
    assert!(
        (spent - 3.75).abs() < 1e-9,
        "the whole subtree is billed: {spent}"
    );
    let store = harness.ctx.store.lock().await;
    assert_eq!(store.spend_for_run(&stage.run, true).unwrap(), 3.75);
    assert_eq!(
        store
            .agent_run(&archon_core::aether_ids::AetherAgentId::parse(GREAT_GRANDCHILD).unwrap())
            .unwrap(),
        Some(stage.run)
    );
}

/// A child already gone when its effort is read is still Archon's, and
/// its model is still checked.
#[tokio::test]
async fn a_child_gone_before_its_effort_is_read_is_owned_and_checked_by_model() {
    let mut config = Config::default();
    deny_model(&mut config);
    config.models.max_effort = Some(Effort::Medium);
    let harness = Harness::with(config, None).await;
    let (_agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    let fleet = keys::CURSOR_FLEET;
    push(&harness, created(CHILD, ORCH, ALLOWED), fleet).await;
    push(&harness, created(CHILD_2, ORCH, DENIED), fleet).await;
    push(&harness, usage_of(CHILD, 0.5), fleet).await;
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], CHILD_2);
    assert_eq!(
        harness.kv(&format!("link_child_{CHILD}")).await.as_deref(),
        Some("null")
    );
    assert_eq!(harness.kv(&format!("link_unchecked_{CHILD}")).await, None);
    assert_eq!(spent_in_total(&harness).await, 0.5);
    let notes = harness.system_notes().await;
    assert!(notes.is_empty(), "handled, not skipped: {notes:?}");
}

/// An effort Archon cannot compare with the cap fails the cap.
#[tokio::test]
async fn an_unrecognised_effort_under_a_cap_is_refused() {
    let mut config = Config::default();
    config.models.max_effort = Some(Effort::Medium);
    let harness = Harness::with(config, None).await;
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_effort(CHILD, "ludicrous");
    push(&harness, created(CHILD, ORCH, ALLOWED), keys::CURSOR_FLEET).await;
    pump.abort();
    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    let reason = cancels[0]["reason"].as_str().unwrap();
    assert!(reason.contains("ludicrous"), "{reason}");
}

/// A check cut short by a lost connection is retried from the same
/// event: owning the child first does not make the retry skip its check.
#[tokio::test]
async fn a_check_cut_short_by_a_lost_connection_is_retried() {
    let mut config = Config::default();
    config.models.max_effort = Some(Effort::Medium);
    let harness = Harness::with(config, None).await;
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_effort(CHILD, "high");
    let hung_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let table = agents.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            if id == CHILD && !hung_up.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Scripted::Hangup { notice: None };
            }
            match table.get(id) {
                Some(agent) => Scripted::reply("agent", agent),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    push(&harness, created(CHILD, ORCH, ALLOWED), keys::CURSOR_FLEET).await;
    pump.abort();
    assert!(harness.fake.connections_accepted() >= 2, "the link dropped");
    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], CHILD);
    assert_eq!(harness.kv(&format!("link_unchecked_{CHILD}")).await, None);
}
