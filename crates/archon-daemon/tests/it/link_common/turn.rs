//! The orchestrator's history as the fake aetherd serves it to the stall watch:
//! event pages, its inbox, and the fresh turn Aether starts after an interrupt.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use archon_aether::fake::{self, Scripted};
use archon_core::Timestamp;
use archon_daemon::supervisor;
use chrono::TimeDelta;
use serde_json::{Value, json};

use super::Harness;

struct History {
    agent: String,
    /// Oldest first; an event's seq is its place, from 1.
    events: Vec<Value>,
    /// When Aether stamps what it adds itself.
    now: Timestamp,
    turn: u64,
    /// The idempotency keys of the triggers taken: a repeat delivers nothing.
    keys: BTreeSet<String>,
}

impl History {
    fn push(&mut self, at: Timestamp, agent: &str, kind: Value) {
        let seq = self.events.len() + 1;
        let event = json!({"seq": seq, "at": at.to_rfc3339(), "agent_id": agent, "kind": kind});
        self.events.push(event);
    }

    fn own(&mut self, kind: Value) {
        let (now, agent) = (self.now, self.agent.clone());
        self.push(now, &agent, kind);
    }

    fn inbox(&self, type_name: &str) -> usize {
        self.events
            .iter()
            .filter(|e| e["kind"]["type"] == type_name && e["agent_id"] == self.agent.as_str())
            .count()
    }

    fn deliver(&mut self, at: Timestamp, kind: Value) {
        let seq = self.inbox("inbox.delivered") + 1;
        let item = json!({"id": format!("ibx_{seq}"), "agent_id": self.agent, "seq": seq,
                          "delivered_at": at.to_rfc3339(), "kind": kind});
        let agent = self.agent.clone();
        self.push(at, &agent, json!({"type": "inbox.delivered", "item": item}));
    }

    /// Aether takes a `send_message` from its socket: an inbox item from the user.
    fn send_message(&mut self, command: &Value) -> Scripted {
        let key = command["idempotency_key"].as_str().unwrap().to_owned();
        if self.keys.insert(key) {
            let now = self.now;
            self.deliver(now, user_message(command["content"].as_str().unwrap()));
        }
        Scripted::reply("message_sent", json!({}))
    }

    /// One page as Aether pages `get_events`: newest first, below `before`.
    fn page(&self, command: &Value) -> Scripted {
        let before = command["query"]["before"].as_u64().unwrap_or(u64::MAX);
        let limit = command["limit"].as_u64().unwrap() as usize;
        let types = command["query"]["types"].as_array().unwrap();
        let page: Vec<Value> = self
            .events
            .iter()
            .rev()
            .filter(|e| e["seq"].as_u64().unwrap() < before)
            .filter(|e| types.contains(&e["kind"]["type"]))
            .take(limit)
            .cloned()
            .collect();
        Scripted::reply("events", Value::Array(page))
    }

    /// Aether stops the turn, then starts a fresh one on the queued inbox.
    fn interrupt(&mut self) -> Scripted {
        let turn = self.turn;
        self.own(json!({"type": "agent.interrupted", "turn": turn}));
        self.own(json!({"type": "agent.turn_completed", "turn": turn, "next": "waiting"}));
        let delivered = self.inbox("inbox.delivered");
        if self.unread() {
            self.turn += 1;
            let turn = self.turn;
            self.own(json!({"type": "agent.started", "turn": turn}));
            self.own(json!({"type": "inbox.consumed", "up_to": delivered}));
        }
        Scripted::reply("interrupted", json!({"was_running": true}))
    }

    /// Whether an item was delivered after the newest `inbox.consumed`.
    fn unread(&self) -> bool {
        let newest = |type_name: &str| {
            self.events.iter().rposition(|e| {
                e["kind"]["type"] == type_name && e["agent_id"] == self.agent.as_str()
            })
        };
        newest("inbox.delivered") > newest("inbox.consumed")
    }
}

/// The fake aetherd's record of one orchestrator, shared with its responders.
#[derive(Clone)]
pub struct FakeTurn(Arc<Mutex<History>>);

impl FakeTurn {
    /// Serve `agent`, running, from an empty history that starts at `now`.
    pub fn serve(harness: &Harness, agent: &str, now: Timestamp) -> Self {
        let history = History {
            agent: agent.to_owned(),
            events: Vec::new(),
            now,
            turn: 1,
            keys: BTreeSet::new(),
        };
        let turn = Self(Arc::new(Mutex::new(history)));
        let mut running = fake::agent(agent, None);
        running["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
        running["status"] = json!({"state": "running"});
        harness.fake.set_responder(
            "get_agent",
            Arc::new(move |_| Scripted::reply("agent", running.clone())),
        );
        let served = turn.clone();
        harness
            .fake
            .set_responder("get_events", Arc::new(move |command| served.page(command)));
        let served = turn.clone();
        harness.fake.set_responder(
            "trigger",
            Arc::new(move |command| {
                let kind = json!({"kind": "external_trigger", "source": command["source"],
                                  "payload": command["payload"]});
                let key = command["idempotency_key"].as_str().unwrap().to_owned();
                let mut history = served.history();
                if history.keys.insert(key) {
                    let now = history.now;
                    history.deliver(now, kind);
                }
                Scripted::ok()
            }),
        );
        let served = turn.clone();
        harness.fake.set_responder(
            "send_message",
            Arc::new(move |command| served.history().send_message(command)),
        );
        let served = turn.clone();
        harness
            .fake
            .set_responder("interrupt", Arc::new(move |_| served.history().interrupt()));
        turn
    }

    fn history(&self) -> std::sync::MutexGuard<'_, History> {
        self.0.lock().unwrap()
    }

    /// Add an event of the orchestrator at `at`.
    pub fn event(&self, at: Timestamp, kind: Value) {
        let mut history = self.history();
        let agent = history.agent.clone();
        history.push(at, &agent, kind);
    }

    /// Add an event of another agent, as a child's shows in the same pages.
    pub fn child_event(&self, at: Timestamp, kind: Value) {
        self.history().push(at, "agt_child", kind);
    }

    pub fn started(&self, at: Timestamp) {
        let turn = self.history().turn;
        self.event(at, json!({"type": "agent.started", "turn": turn}));
    }

    /// A Bash call `id` requested at `at`; `timeout_ms` as its input declares.
    pub fn bash(&self, at: Timestamp, id: &str, command: &str, timeout_ms: Option<u64>) {
        let mut input = json!({"command": command});
        if let Some(timeout) = timeout_ms {
            input["timeout"] = json!(timeout);
        }
        let call = json!({"id": id, "name": "Bash", "input": input});
        self.event(at, json!({"type": "tool.requested", "call": call}));
    }

    pub fn succeeded(&self, at: Timestamp, id: &str) {
        self.event(at, json!({"type": "tool.succeeded", "call": id}));
    }

    /// The operator's chat message, delivered at `at` and not yet read.
    pub fn operator_says(&self, at: Timestamp, text: &str) {
        self.deliver(at, user_message(text));
    }

    /// The operator's chat line sent at `at` the way archond sends it: Aether
    /// receives the `send_message` and puts it in the orchestrator's inbox.
    pub async fn say(&self, harness: &Harness, at: Timestamp, text: &str) {
        self.history().now = at;
        let item = archon_core::id::FeedItemId::new();
        supervisor::deliver_operator_message(&harness.ctx, &item, text, &[])
            .await
            .unwrap();
    }

    /// An inbox item of Aether's `kind`, delivered at `at` and not yet read.
    pub fn deliver(&self, at: Timestamp, kind: Value) {
        self.history().deliver(at, kind);
    }

    /// The reply to one `get_events` command.
    pub fn page(&self, command: &Value) -> Scripted {
        self.history().page(command)
    }

    /// The stall check as of `at`, which is also when Aether stamps what the
    /// check makes it add.
    pub async fn check(&self, harness: &Harness, at: Timestamp) -> bool {
        self.history().now = at;
        supervisor::report_stall(&harness.ctx, at).await.unwrap()
    }

    /// Every event of `type_name` the orchestrator has, oldest first.
    pub fn events(&self, type_name: &str) -> Vec<Value> {
        let history = self.history();
        history
            .events
            .iter()
            .filter(|e| e["kind"]["type"] == type_name && e["agent_id"] == history.agent.as_str())
            .cloned()
            .collect()
    }

    /// The text of each note Archon sent the orchestrator, oldest first.
    pub fn notes(&self) -> Vec<String> {
        self.events("inbox.delivered")
            .iter()
            .filter(|e| e["kind"]["item"]["kind"]["source"] == "archon")
            .map(|e| {
                e["kind"]["item"]["kind"]["payload"]["events"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    /// The inbox seq the newest `inbox.consumed` read up to.
    pub fn read_up_to(&self) -> Option<u64> {
        self.events("inbox.consumed")
            .last()
            .map(|e| e["kind"]["up_to"].as_u64().unwrap())
    }
}

/// The inbox item Aether makes of a message sent over its local socket.
fn user_message(text: &str) -> Value {
    let from = json!({"actor": "user", "origin": {"transport": "local_socket"}});
    json!({"kind": "message", "message_id": "msg_1", "from": from,
           "content": [{"type": "text", "text": text}]})
}

/// `minutes` and `seconds` after `start`.
pub fn after(start: Timestamp, minutes: i64, seconds: i64) -> Timestamp {
    start + TimeDelta::minutes(minutes) + TimeDelta::seconds(seconds)
}
