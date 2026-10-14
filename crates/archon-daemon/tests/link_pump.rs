//! The event pump end to end against the fake aetherd: the quiet chat,
//! the policy watchdog, orchestrator revival and restarts, attribution of
//! stage events, settles racing a spawn, reconnects and replays.

mod link_common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::aether_ids::AetherTaskId;
use archon_core::autonomy::{Directive, DirectiveSource};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{FeedItemId, QueueItemId, RepoId};
use archon_core::model::{Effort, ModelRef};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::Caps;
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{StageRun, StageState};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{handlers, mirror, pump, supervisor};
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

    fn set_scope(&self, id: &str, scope: Value) {
        let mut agent = self.set(id, "running");
        agent["scope"] = scope;
        self.0.lock().unwrap().insert(id.to_owned(), agent);
    }

    fn set_effort(&self, id: &str, effort: &str) {
        let mut agent = self.set(id, "running");
        agent["config"] = json!({ "effort": effort });
        self.0.lock().unwrap().insert(id.to_owned(), agent);
    }
}

/// A pump following a live orchestrator, with both streams subscribed.
async fn running_pump(harness: &Harness) -> (Agents, JoinHandle<Result<(), ArchonError>>) {
    running_pump_beside(harness, &[]).await
}

/// `running_pump` with `live` agents Aether already knows: a stage whose
/// agent it has forgotten is failed, and stopped, as the pump reconciles.
async fn running_pump_beside(
    harness: &Harness,
    live: &[&str],
) -> (Agents, JoinHandle<Result<(), ArchonError>>) {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let agents = Agents::install(harness);
    agents.set(ORCH, "running");
    for agent in live {
        agents.set(agent, "running");
    }
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

/// A turn ends waiting; `condition` is what Aether resumes it on.
async fn waits_for(harness: &Harness, condition: Value) {
    let waiting = json!({"type": "agent.waiting", "condition": condition});
    orch(harness, waiting).await;
    let done = json!({"type": "agent.turn_completed", "turn": 1, "invocation": "inv_1",
                      "next": "waiting"});
    orch(harness, done).await;
}

/// `delegate` with `wait` ends the answering turn; the answer is written by
/// the turn the helper's result wakes, and it reaches the chat.
#[tokio::test]
async fn an_answer_that_waits_for_a_delegated_helper_reaches_the_chat() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    say(&harness, 1, "Which tests are flaky?").await;
    orch(&harness, consumed(1)).await;
    orch(&harness, said("A helper is reading the CI history.")).await;
    orch(&harness, tool_call()).await;
    let helper_task = json!({"on": "any", "of": [
        {"on": "task", "id": "tsk_0101"}, {"on": "deadline", "at": "2026-09-26T10:30:00Z"}]});
    waits_for(&harness, helper_task).await;
    assert!(awaiting_reply(&harness).await, "the answer is still owed");

    let result = json!({"kind": "task_completed", "task": "tsk_0101",
                        "result": {"summary": "three tests"}});
    orch(&harness, delivered(2, result)).await;
    orch(&harness, consumed(2)).await;
    orch(&harness, said("Three tests are flaky: a, b and c.")).await;
    waits_for(&harness, json!({"on": "inbox"})).await;
    assert!(!awaiting_reply(&harness).await);

    orch(&harness, trigger_delivered(3)).await;
    orch(&harness, consumed(3)).await;
    orch(&harness, said("Later trigger work.")).await;
    waits_for(&harness, json!({"on": "inbox"})).await;
    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    orch(&harness, trigger_delivered(4)).await;
    pump.abort();
    assert_eq!(
        harness.orchestrator_replies().await,
        vec![
            "A helper is reading the CI history.",
            "Three tests are flaky: a, b and c."
        ]
    );
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
        caller: None,
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
        lesson_hint: None,
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
    harness.fake.set_default("cancel", Scripted::ok());
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
    // A cancellation is routine news, which by itself is not delivered.
    let mut config = Config::default();
    config.orchestrator.wake_on_success = true;
    let harness = Harness::with(config, None).await;
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
    harness.fake.set_default("cancel", Scripted::ok());
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
    // Owed by the adoption and made by itself, with nothing waiting for it.
    harness
        .eventually("the replaced agent's children are swept", || async {
            !harness.fake.received_named("cancel").is_empty()
        })
        .await;
    let swept = harness.fake.received_named("cancel");
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert_eq!(swept[0]["agent"], ORCH);
    pump.abort();
}

/// archond starts with a live orchestrator and a sweep Aether refuses: the pump
/// attaches all the same, says once that it is owed, and makes it when asked.
#[tokio::test]
async fn a_sweep_aether_refuses_keeps_nothing_from_the_live_orchestrator() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    let owed = json!([{"agent": ORCH, "at": archon_core::now(), "owed": true}]);
    harness
        .set_kv("orchestrator_retired", &owed.to_string())
        .await;
    let agents = Agents::install(&harness);
    agents.set(ORCH, "cancelled");
    agents.restart(ORCH, ORCH_2);
    harness
        .fake
        .set_default("cancel", Scripted::refuse("internal", "store busy"));
    let pump = tokio::spawn(pump::run(harness.ctx.clone(), harness.fake.endpoint()));
    wait_for_streams(&harness, 1).await;
    harness
        .eventually("the orchestrator's stream is attached", || async {
            let subscribes = harness.fake.received_named("subscribe");
            subscribes.iter().any(|s| s["query"]["agent"] == ORCH_2)
        })
        .await;
    harness
        .eventually("the refused sweep is said", || async {
            let notes = harness.system_notes().await;
            notes.iter().any(|n| n.contains("store busy"))
        })
        .await;
    let serving = harness.ctx.supervisor.watch().borrow().clone();
    assert_eq!(serving, Some(link_common::agent_id(ORCH_2)));

    // The restart that owed the sweep is seen again: nothing to adopt, and
    // the sweep is tried at once instead of at its next pause.
    harness.fake.set_default("cancel", Scripted::ok());
    let (from, to) = (link_common::agent_id(ORCH), link_common::agent_id(ORCH_2));
    supervisor::adopt_restart(&harness.ctx, &from, &to)
        .await
        .unwrap();
    harness
        .eventually("the sweep is made", || async {
            let notes = harness.system_notes().await;
            notes.iter().any(|n| n.contains("Cancelled what"))
        })
        .await;
    pump.abort();
    let notes = harness.system_notes().await;
    let refused = notes.iter().filter(|n| n.contains("store busy")).count();
    assert_eq!(refused, 1, "said once, not at every try: {notes:?}");
    let record = harness.kv("orchestrator_retired").await.expect("kept");
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record[0]["agent"], ORCH);
    assert_eq!(record[0]["owed"], false, "{record}");
}

/// Aether's events outlive the generation they describe: what a replaced
/// orchestrator delegated to and spent is still Archon's when they arrive.
#[tokio::test]
async fn a_replaced_orchestrators_late_children_and_spend_are_still_archons() {
    let mut config = Config::default();
    deny_model(&mut config);
    let mut harness = Harness::with(config, None).await;
    let (agents, pump) = running_pump(&harness).await;
    agents.set(ORCH, "cancelled");
    agents.restart(ORCH, ORCH_2);
    harness.fake.set_default("cancel", Scripted::ok());
    let restarted = json!({"type": "agent.restarted", "from": ORCH, "to": ORCH_2,
                           "transcript_tail": 0});
    orch(&harness, restarted).await;
    harness
        .eventually(
            "the successor is adopted and the old agent swept",
            || async {
                let adopted = harness.kv(keys::ORCHESTRATOR_AGENT).await;
                adopted.as_deref() == Some(ORCH_2)
                    && !harness.fake.received_named("cancel").is_empty()
            },
        )
        .await;

    // Delegated by the generation since replaced, and only now reported.
    let fleet = keys::CURSOR_FLEET;
    push(&harness, created(CHILD, ORCH, DENIED), fleet).await;
    let allowed = ("claude-code", "claude-opus-5-5");
    push(&harness, created(CHILD_2, ORCH, allowed), fleet).await;
    push(&harness, usage_of(CHILD_2, 0.5), fleet).await;
    push(&harness, usage_of(ORCH, 0.25), fleet).await;
    push(&harness, stranger_usage(), fleet).await;
    pump.abort();

    let cancelled: Vec<Value> = harness.fake.received_named("cancel");
    let cancelled: Vec<&Value> = cancelled.iter().map(|c| &c["agent"]).collect();
    assert_eq!(cancelled, [ORCH, CHILD], "the sweep, then the policy");
    let violation = TriggerEvent::PolicyViolation {
        agent: CHILD.into(),
        model: format!("{}/{}", DENIED.0, DENIED.1),
        action: "cancelled".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![violation]);
    let owned = harness.kv(&format!("link_child_{CHILD_2}")).await;
    assert_eq!(owned.as_deref(), Some("null"), "an orchestrator's child");
    assert_eq!(
        spent_in_total(&harness).await,
        0.75,
        "the child's and its own"
    );
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
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    let refused = format!("Cancelled agent {CHILD}: model policy: model ");
    assert!(blockers[0].starts_with(&refused), "{blockers:?}");
    assert!(blockers[0].contains("not allowed"), "{blockers:?}");
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
    let (_agents, pump) = running_pump_beside(&harness, &[STAGE]).await;
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

/// A helper whose scope names no directory may read every mirror, so a
/// model any registered repository refuses is refused to it.
#[tokio::test]
async fn the_orchestrators_helpers_follow_every_registered_repositorys_policy() {
    let mut harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let archon_dir = repo.path().join(".archon");
    std::fs::create_dir_all(&archon_dir).unwrap();
    let denied = format!("[models]\ndeny = [\"{}/{}\"]\n", DENIED.0, DENIED.1);
    std::fs::write(archon_dir.join("config.toml"), denied).unwrap();
    let global = harness.ctx.config.models.policy();
    assert!(global.permits_model(&ModelRef::new(DENIED.0, DENIED.1)));
    link_common::queued_item(&harness.ctx, repo.path()).await;
    let (_agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    push(&harness, created(CHILD, ORCH, DENIED), keys::CURSOR_FLEET).await;
    push(
        &harness,
        created(CHILD_2, ORCH, ALLOWED),
        keys::CURSOR_FLEET,
    )
    .await;
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["agent"], CHILD);
    let violation = TriggerEvent::PolicyViolation {
        agent: CHILD.into(),
        model: format!("{}/{}", DENIED.0, DENIED.1),
        action: "cancelled".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![violation]);
    let kept = harness.kv(&format!("link_child_{CHILD_2}")).await;
    assert_eq!(
        kept.as_deref(),
        Some("null"),
        "an allowed helper is tracked"
    );
}

/// A helper whose workspace or allowed path leads into a registered
/// repository, even through a link, is cancelled: it reads mirrors only.
#[tokio::test]
async fn a_helper_placed_in_a_repository_is_cancelled_wherever_the_path_leads_from() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    link_common::queued_item(&harness.ctx, repo.path()).await;
    let own = harness.ctx.paths.orchestrator_workspace.clone();
    std::fs::create_dir_all(own.join("notes")).unwrap();
    let link = own.join("alpha");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_scope(CHILD, json!({ "workspace_root": link }));
    let inside = repo.path().join("src");
    agents.set_scope(
        CHILD_2,
        json!({ "workspace_root": own, "allowed_paths": [inside] }),
    );
    agents.set_scope(GRANDCHILD, json!({ "workspace_root": own.join("notes") }));
    for helper in [CHILD, CHILD_2, GRANDCHILD] {
        push(&harness, created(helper, ORCH, ALLOWED), keys::CURSOR_FLEET).await;
    }
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    let cancelled: Vec<&str> = cancels.iter().filter_map(|c| c["agent"].as_str()).collect();
    assert_eq!(cancelled, [CHILD, CHILD_2], "{cancels:?}");
    for cancel in &cancels {
        let reason = cancel["reason"].as_str().unwrap();
        assert!(reason.contains("inside repository `alpha`"), "{reason}");
        assert!(
            reason.starts_with("archon policy: it was given "),
            "{reason}"
        );
    }
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 2, "{blockers:?}");
    let wording = format!("Cancelled agent {CHILD}: it was given ");
    assert!(blockers[0].starts_with(&wording), "{blockers:?}");
    assert!(
        blockers.iter().all(|text| !text.contains("model policy")),
        "its model was allowed: {blockers:?}"
    );
    let kept = harness.kv(&format!("link_unchecked_{GRANDCHILD}")).await;
    assert_eq!(
        kept, None,
        "a helper in the orchestrator's workspace passes"
    );
}

/// An item's agents are placed by Archon: the same paths are theirs.
#[tokio::test]
async fn a_stage_agents_child_may_work_in_its_items_repository() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    stage_with_task(&harness, repo.path()).await;
    let (agents, pump) = running_pump_beside(&harness, &[STAGE]).await;
    agents.set_scope(CHILD, json!({ "workspace_root": repo.path() }));
    push(&harness, created(CHILD, STAGE, ALLOWED), keys::CURSOR_FLEET).await;
    pump.abort();
    let cancels = harness.fake.received_named("cancel");
    assert!(cancels.is_empty(), "{cancels:?}");
    assert!(harness.kv(&format!("link_child_{CHILD}")).await.is_some());
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
    let (_agents, pump) = running_pump_beside(&harness, &[STAGE]).await;
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
    let (_agents, pump) = running_pump_beside(&harness, &[STAGE]).await;
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
/// How often that wait looks again unasked (`SPAWN_RECHECK`).
const SPAWN_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

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

/// The stop of what a settled stage agent delegated waits for its turn to
/// end (§6.3), so the fleet stream asks Aether for that end.
#[tokio::test]
async fn the_fleet_stream_asks_for_an_agents_completed_turn() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let subscribes = harness.fake.received_named("subscribe");
    pump.abort();
    let fleet: Vec<_> = subscribes
        .iter()
        .filter(|s| s["query"]["agent"].is_null())
        .collect();
    assert_eq!(fleet.len(), 1, "one fleet subscription: {subscribes:?}");
    let types = fleet[0]["query"]["types"].as_array().expect("its types");
    for ended in ["agent.completed", "agent.failed", "agent.cancelled"] {
        assert!(types.contains(&json!(ended)), "{ended} in {types:?}");
    }
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

/// Record the spawn reply of `stage` as `spawn::issue` does, without the
/// signal that follows it.
async fn record_spawned(harness: &Harness, stage: &StageRun) {
    let mut store = harness.ctx.store.lock().await;
    let mut spawned = store.stage_run(&stage.id).unwrap();
    assert_eq!(spawned.state, StageState::Spawning, "not settled early");
    spawned.aether_agent = Some(link_common::agent_id(STAGE));
    spawned.aether_task = Some(AetherTaskId::parse(STAGE_TASK).unwrap());
    spawned.state = StageState::Running;
    store.upsert_stage_run(&spawned).unwrap();
}

async fn settled(harness: &Harness, stage: &StageRun) -> bool {
    let store = harness.ctx.store.lock().await;
    store.stage_run(&stage.id).unwrap().state.is_terminal()
}

/// The wait still covers the race it exists for: a settle in a spawning
/// stage's Aether run, arriving before its spawn reply is recorded, settles
/// the stage once the reply is. Nothing signals this record, so the wait
/// finds it at its next look.
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
    record_spawned(&harness, &stage).await;
    harness
        .eventually("the racing settle reached its stage", || {
            settled(&harness, &stage)
        })
        .await;
    pump.abort();
}

/// A recorded spawn ends the wait itself: the settle reaches its stage long
/// before the wait would have looked again.
#[tokio::test]
async fn a_recorded_spawn_ends_the_wait_for_it_at_once() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = spawning_stage(&harness.ctx, repo.path(), SPAWN_RUN, Some("agt_0300")).await;
    let (_agents, pump) = running_pump(&harness).await;
    harness
        .fake
        .push_event(settled_in(STAGE, STAGE_TASK, SPAWN_RUN));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!settled(&harness, &stage).await, "the settle is waiting");
    record_spawned(&harness, &stage).await;
    let recorded = tokio::time::Instant::now();
    harness.ctx.spawn_recorded.notify_waiters();
    harness
        .eventually("the waiting settle reached its stage", || {
            settled(&harness, &stage)
        })
        .await;
    let took = recorded.elapsed();
    pump.abort();
    assert!(took < SPAWN_RECHECK / 2, "the settle waited {took:?}");
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

/// Whether the dispatcher was asked for a pass since the last look.
async fn dispatcher_woken(harness: &Harness) -> bool {
    let woken = harness.ctx.dispatch.notified();
    tokio::time::timeout(Duration::ZERO, woken).await.is_ok()
}

/// A usage update costs a dispatch pass only when a total or daily budget is
/// set, and then the pass holding work at the budget follows at once.
#[tokio::test]
async fn usage_wakes_the_dispatcher_only_while_a_global_budget_is_set() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    stage_with_task(&harness, repo.path()).await;
    let (_agents, pump) = running_pump_beside(&harness, &[STAGE]).await;
    assert!(
        dispatcher_woken(&harness).await,
        "connecting asks for a pass"
    );

    push(&harness, usage_of(STAGE, 0.25), keys::CURSOR_FLEET).await;
    assert!((spent_in_total(&harness).await - 0.25).abs() < 1e-9);
    assert!(
        !dispatcher_woken(&harness).await,
        "no budget this spend could reach"
    );

    let budgeted = Caps {
        budget_usd_daily: 10.0,
        ..Caps::default()
    };
    *harness.ctx.caps_override.lock().await = Some(budgeted);
    push(&harness, usage_of(STAGE, 0.5), keys::CURSOR_FLEET).await;
    pump.abort();
    assert!((spent_in_total(&harness).await - 0.5).abs() < 1e-9);
    assert!(
        dispatcher_woken(&harness).await,
        "the daily budget is checked"
    );
}

/// A stage agent under `node` with three generations beneath it, the
/// grandchild on a refused model, and spend reported by each of the four.
async fn push_three_generations_with_usage(harness: &Harness, node: &str) {
    let fleet = keys::CURSOR_FLEET;
    push(harness, created(STAGE, node, ALLOWED), fleet).await;
    push(harness, created(CHILD, STAGE, ALLOWED), fleet).await;
    push(harness, created(GRANDCHILD, CHILD, DENIED), fleet).await;
    let deepest = created(GREAT_GRANDCHILD, GRANDCHILD, ALLOWED);
    push(harness, deepest, fleet).await;
    for (agent, cost) in [
        (STAGE, 0.25),
        (CHILD, 0.5),
        (GRANDCHILD, 1.0),
        (GREAT_GRANDCHILD, 2.0),
    ] {
        push(harness, usage_of(agent, cost), fleet).await;
    }
}

/// A stage agent that delegates before its spawn reply is recorded,
/// and its children's children, are all policed and billed to the item;
/// a child a third delegation down is cancelled for its depth.
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
    push_three_generations_with_usage(&harness, node).await;
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    let cancelled: Vec<&str> = cancels.iter().filter_map(|c| c["agent"].as_str()).collect();
    assert_eq!(cancelled, [GRANDCHILD, GREAT_GRANDCHILD], "{cancels:?}");
    let model = cancels[0]["reason"].as_str().unwrap();
    assert!(model.contains("is not allowed"), "{model}");
    let depth = cancels[1]["reason"].as_str().unwrap();
    assert!(
        depth.contains("3 delegations below its stage agent; at most 2 are allowed"),
        "{depth}"
    );
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

/// An aetherd that is not running is asked for every second, not at a growing
/// interval: one that Archon restarts is found before a command shows the gap.
#[tokio::test]
async fn an_aetherd_that_starts_late_is_found_within_a_second_or_so() {
    use archon_aether::client::Endpoint;
    let harness = Harness::new().await;
    let socket = harness.dir.path().join("late.sock");
    let pump = tokio::spawn(pump::run(
        harness.ctx.clone(),
        Endpoint::Unix(socket.clone()),
    ));
    // A doubling interval would have its attempts 1, 3, 7 and 15 s in.
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!pump.is_finished(), "an absent aetherd is not fatal");
    assert!(!harness.ctx.aether.is_connected());

    let late = fake::FakeAether::start(&socket).await.unwrap();
    let found = tokio::time::Instant::now() + Duration::from_secs(3);
    while late.connections_accepted() == 0 {
        assert!(tokio::time::Instant::now() < found, "not found in 3 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pump.abort();
}

const HELPER_5: &str = "agt_0105";

/// Repositories `alpha` (refusing [`DENIED`]) and `beta`, mirrored in the
/// orchestrator's workspace; returns where they are and that workspace.
async fn two_mirrored_repos(harness: &Harness) -> (tempfile::TempDir, PathBuf) {
    let repos = tempfile::tempdir().unwrap();
    let (alpha, beta) = (repos.path().join("alpha"), repos.path().join("beta"));
    std::fs::create_dir_all(alpha.join(".archon")).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let denied = format!("[models]\ndeny = [\"{}/{}\"]\n", DENIED.0, DENIED.1);
    std::fs::write(alpha.join(".archon/config.toml"), denied).unwrap();
    link_common::queued_item(&harness.ctx, &alpha).await;
    let second = Repo {
        id: RepoId::new(),
        name: "beta".into(),
        path: beta,
        ship: ShipMode::Local,
        base_branch: None,
        default_pipeline: None,
        autonomy_max: Default::default(),
        added_at: archon_core::now(),
    };
    harness.ctx.store.lock().await.insert_repo(&second).unwrap();
    let own = harness.ctx.paths.orchestrator_workspace.clone();
    let mirrors = mirror::mirrors_root(&own);
    std::fs::create_dir_all(mirrors.join("alpha/.git/worktrees/w1")).unwrap();
    std::fs::create_dir_all(mirrors.join("alpha/src")).unwrap();
    std::fs::create_dir_all(mirrors.join("beta")).unwrap();
    std::fs::create_dir_all(own.join(mirror::SCRATCH_DIR)).unwrap();
    (repos, own)
}

/// The agents cancelled so far, each with the reason Aether was given.
fn cancelled(harness: &Harness) -> Vec<(String, String)> {
    let text = |cancel: &Value, field: &str| cancel[field].as_str().unwrap().to_owned();
    let cancels = harness.fake.received_named("cancel");
    cancels
        .iter()
        .map(|cancel| (text(cancel, "agent"), text(cancel, "reason")))
        .collect()
}

/// A helper answers only to the repositories whose mirrors it reads: `alpha`
/// refusing a model does not refuse it in `beta`'s mirror or in scratch.
#[tokio::test]
async fn a_helper_follows_the_policy_of_the_mirrors_its_scope_reads() {
    let harness = Harness::new().await;
    let (_repos, own) = two_mirrored_repos(&harness).await;
    let mirrors = mirror::mirrors_root(&own);
    let scratch = own.join(mirror::SCRATCH_DIR);
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_scope(CHILD, json!({ "workspace_root": mirrors.join("beta") }));
    agents.set_scope(
        CHILD_2,
        json!({ "workspace_root": mirrors.join("alpha/src") }),
    );
    agents.set_scope(GRANDCHILD, json!({ "workspace_root": scratch }));
    agents.set_scope(
        GREAT_GRANDCHILD,
        json!({ "workspace_root": scratch, "allowed_paths": [mirrors.join("alpha")] }),
    );
    agents.set_scope(HELPER_5, json!({ "workspace_root": own }));
    for helper in [CHILD, CHILD_2, GRANDCHILD, GREAT_GRANDCHILD, HELPER_5] {
        push(&harness, created(helper, ORCH, DENIED), keys::CURSOR_FLEET).await;
    }
    pump.abort();

    let cancels = cancelled(&harness);
    let agents: Vec<&str> = cancels.iter().map(|(agent, _)| agent.as_str()).collect();
    assert_eq!(agents, [CHILD_2, GREAT_GRANDCHILD, HELPER_5], "{cancels:?}");
    for (_, reason) in &cancels[..2] {
        assert!(reason.contains("is not allowed"), "{reason}");
        assert!(reason.contains("here the mirror of `alpha`;"), "{reason}");
    }
    let whole = &cancels[2].1;
    assert!(whole.contains("here every repository's mirror;"), "{whole}");
    for kept in [CHILD, GRANDCHILD] {
        let tracked = harness.kv(&format!("link_child_{kept}")).await;
        assert_eq!(
            tracked.as_deref(),
            Some("null"),
            "{kept} is a tracked helper"
        );
        assert_eq!(harness.kv(&format!("link_unchecked_{kept}")).await, None);
    }
}

/// Aether gives an isolated child a git worktree of the mirror, outside the
/// orchestrator's workspace: it still answers to that mirror's repository.
#[tokio::test]
async fn an_isolated_child_of_a_mirror_answers_to_that_mirrors_repository() {
    let harness = Harness::new().await;
    let (repos, own) = two_mirrored_repos(&harness).await;
    let isolated = repos.path().join("aether/delegated/workspace");
    std::fs::create_dir_all(&isolated).unwrap();
    let git_dir = mirror::mirrors_root(&own).join("alpha/.git/worktrees/w1");
    std::fs::write(
        isolated.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    for helper in [CHILD, CHILD_2] {
        agents.set_scope(helper, json!({ "workspace_root": isolated }));
    }
    push(&harness, created(CHILD, ORCH, DENIED), keys::CURSOR_FLEET).await;
    push(
        &harness,
        created(CHILD_2, ORCH, ALLOWED),
        keys::CURSOR_FLEET,
    )
    .await;
    pump.abort();

    let cancels = cancelled(&harness);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0].0, CHILD);
    assert!(
        cancels[0].1.contains("here the mirror of `alpha`;"),
        "{}",
        cancels[0].1
    );
    let tracked = harness.kv(&format!("link_child_{CHILD_2}")).await;
    assert_eq!(
        tracked.as_deref(),
        Some("null"),
        "an allowed model works there"
    );
}

/// A code question needs no item: a helper reads the mirror, never the checkout
/// or the items' worktrees, also through a link the repository tracks.
#[tokio::test]
async fn a_helper_reads_a_mirror_and_is_kept_out_of_the_checkout_and_worktrees() {
    let harness = Harness::new().await;
    let (repos, own) = two_mirrored_repos(&harness).await;
    let checkout = repos.path().join("alpha");
    let mirror_dir = mirror::mirrors_root(&own).join("alpha");
    std::os::unix::fs::symlink(&checkout, mirror_dir.join("vendored")).unwrap();
    let worktree = harness.ctx.paths.worktrees.join("item-1");
    std::fs::create_dir_all(&worktree).unwrap();
    let (agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    agents.set_scope(CHILD, json!({ "workspace_root": mirror_dir.join("src") }));
    agents.set_scope(CHILD_2, json!({ "workspace_root": checkout }));
    agents.set_scope(GRANDCHILD, json!({ "workspace_root": worktree }));
    agents.set_scope(
        GREAT_GRANDCHILD,
        json!({ "workspace_root": mirror_dir.join("vendored") }),
    );
    for helper in [CHILD, CHILD_2, GRANDCHILD, GREAT_GRANDCHILD] {
        push(&harness, created(helper, ORCH, ALLOWED), keys::CURSOR_FLEET).await;
    }
    pump.abort();

    let cancels = cancelled(&harness);
    let agents: Vec<&str> = cancels.iter().map(|(agent, _)| agent.as_str()).collect();
    assert_eq!(
        agents,
        [CHILD_2, GRANDCHILD, GREAT_GRANDCHILD],
        "{cancels:?}"
    );
    assert!(
        cancels[0].1.contains("inside repository `alpha`"),
        "{}",
        cancels[0].1
    );
    assert!(
        cancels[1].1.contains("inside the items' worktrees"),
        "{}",
        cancels[1].1
    );
    assert!(
        cancels[2].1.contains("inside repository `alpha`"),
        "{}",
        cancels[2].1
    );
    for (_, reason) in &cancels {
        assert!(reason.ends_with("in its mirror under `repos/`"), "{reason}");
    }
    let tracked = harness.kv(&format!("link_child_{CHILD}")).await;
    assert_eq!(
        tracked.as_deref(),
        Some("null"),
        "the mirror is the helper's to read"
    );
    assert_eq!(harness.kv(&format!("link_unchecked_{CHILD}")).await, None);
}

/// The orchestrator's helper may delegate once more; a third level is
/// cancelled, like under a stage agent.
#[tokio::test]
async fn the_orchestrators_helpers_delegate_one_level_deeper_and_no_further() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    harness.fake.set_default("cancel", Scripted::ok());
    let fleet = keys::CURSOR_FLEET;
    push(&harness, created(CHILD, ORCH, ALLOWED), fleet).await;
    push(&harness, created(GRANDCHILD, CHILD, ALLOWED), fleet).await;
    push(
        &harness,
        created(GREAT_GRANDCHILD, GRANDCHILD, ALLOWED),
        fleet,
    )
    .await;
    pump.abort();

    let cancels = cancelled(&harness);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0].0, GREAT_GRANDCHILD);
    let reason = &cancels[0].1;
    assert!(
        reason.contains("3 delegations below the orchestrator; at most 2 are allowed"),
        "{reason}"
    );
    for kept in [CHILD, GRANDCHILD] {
        let tracked = harness.kv(&format!("link_child_{kept}")).await;
        assert_eq!(
            tracked.as_deref(),
            Some("null"),
            "{kept} is a tracked helper"
        );
    }
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(blockers[0].contains("3 delegations below the orchestrator"));
}

/// Aether folded the orchestrator's context: what it was told of the
/// directives may be gone, so they are stated again in a trigger of their own.
#[tokio::test]
async fn a_compacted_orchestrator_is_told_its_directives_again() {
    let mut harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let compacted = json!({"type": "compaction.completed", "checkpoint": "chk_1",
                           "version": 1, "from_seq": 1, "to_seq": 40});
    orch(&harness, compacted.clone()).await;
    assert!(
        harness.drain_triggers().await.is_empty(),
        "no directive is in force: nothing to restate"
    );

    let directive = Directive::new(
        "no yolo today",
        DirectiveSource::Operator,
        None,
        archon_core::now(),
    );
    let mut store = harness.ctx.store.lock().await;
    store.insert_directive(&directive.unwrap()).unwrap();
    drop(store);
    orch(&harness, compacted).await;
    pump.abort();
    let restated = TriggerEvent::DirectivesRestated {
        reason: "your context was compacted".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![restated]);
}
