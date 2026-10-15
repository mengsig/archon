//! A scripted in-process `aetherd` on a Unix socket (feature `fake`).
//!
//! It speaks the real wire protocol: hello at a configurable version window,
//! request/reply by `req`, and subscriptions that replay from `after` and then
//! follow [`FakeAether::push_event`]. Like Aether, it may send replayed events
//! *before* the `subscribed` reply. Every command after the hello is recorded.
//! Replies come from per-command scripts (one-shot, in order), then
//! per-command responders, then built-ins (`ping` → ok); anything else is
//! refused with `internal`, so an unscripted call fails loudly. Like Aether,
//! it answers `incompatible` instead of a GitHub watcher the connection's
//! protocol cannot carry (an allowlist below 15, operator logins below 17).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::client::Endpoint;
use crate::wire::{ALLOWLIST_PROTOCOL_VERSION, OPERATOR_LOGINS_PROTOCOL_VERSION, PROTOCOL_VERSION};

/// Aether's refusals of a watcher an older peer cannot carry (`compat.rs`).
pub const ALLOWLIST_WATCHER_VERSION_ERROR: &str =
    "GitHub watcher allowed_users requires protocol 15; upgrade the Aether client";
pub const OPERATOR_USERS_WATCHER_VERSION_ERROR: &str =
    "GitHub watcher operator_users requires protocol 17; upgrade the Aether client";

/// What the fake answers one command with.
#[derive(Debug, Clone, PartialEq)]
pub enum Scripted {
    /// `{"Ok":{"reply":kind,"data":data}}`.
    Reply { kind: String, data: Value },
    /// `{"Err":{"code":code,"message":message}}`.
    Refuse { code: String, message: String },
    /// Never answer; exercises the client's request timeout.
    Silent,
    /// Hang up, first sending an id-0 notice `(code, message)` when given.
    Hangup { notice: Option<(String, String)> },
}

impl Scripted {
    pub fn reply(kind: &str, data: Value) -> Self {
        Self::Reply {
            kind: kind.to_owned(),
            data,
        }
    }

    pub fn ok() -> Self {
        Self::reply("ok", Value::Null)
    }

    pub fn refuse(code: &str, message: &str) -> Self {
        Self::Refuse {
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }
}

/// Computes a reply from the received command (e.g. `get_task` by id).
pub type Responder = Arc<dyn Fn(&Value) -> Scripted + Send + Sync>;

enum Out {
    Line(String),
    Close,
}

struct Connection {
    id: u64,
    out: mpsc::UnboundedSender<Out>,
    /// `(subscription id, query)`.
    subs: Vec<(u64, Value)>,
    next_sub: u64,
}

struct State {
    instance_id: String,
    serves: (u32, u32),
    idle_timeout_secs: Option<u64>,
    submission_horizon: Option<String>,
    features: Vec<String>,
    scripts: HashMap<String, VecDeque<Scripted>>,
    responders: HashMap<String, Responder>,
    received: Vec<Value>,
    /// The event log; an event's seq is its index + 1.
    events: Vec<Value>,
    connections: Vec<Connection>,
    next_connection: u64,
    accepted: u64,
}

/// The fake daemon. Dropping it hangs up every connection.
pub struct FakeAether {
    socket: PathBuf,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    accept: JoinHandle<()>,
}

impl std::fmt::Debug for FakeAether {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeAether")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl FakeAether {
    /// Listen on `socket` (its directory must exist and be writable).
    pub async fn start(socket: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(socket)?;
        let state = Arc::new(Mutex::new(State {
            instance_id: "fake-instance-1".to_owned(),
            serves: (PROTOCOL_VERSION - 1, PROTOCOL_VERSION),
            idle_timeout_secs: None,
            submission_horizon: None,
            features: Vec::new(),
            scripts: HashMap::new(),
            responders: HashMap::new(),
            received: Vec::new(),
            events: Vec::new(),
            connections: Vec::new(),
            next_connection: 1,
            accepted: 0,
        }));
        let changed = Arc::new(Notify::new());
        let accept = tokio::spawn(accept_loop(listener, state.clone(), changed.clone()));
        Ok(Self {
            socket: socket.to_owned(),
            state,
            changed,
            accept,
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Queue a one-shot reply for the next `cmd`; several queue in order.
    pub fn script(&self, cmd: &str, reply: Scripted) {
        let mut state = self.state();
        state
            .scripts
            .entry(cmd.to_owned())
            .or_default()
            .push_back(reply);
    }

    /// Answer every `cmd` not covered by a script with `reply`.
    pub fn set_default(&self, cmd: &str, reply: Scripted) {
        self.set_responder(cmd, Arc::new(move |_| reply.clone()));
    }

    /// Answer every `cmd` not covered by a script by calling `responder`.
    pub fn set_responder(&self, cmd: &str, responder: Responder) {
        self.state().responders.insert(cmd.to_owned(), responder);
    }

    /// Welcomes from now on carry this instance id (an Aether restart).
    pub fn set_instance_id(&self, instance_id: &str) {
        assert!(!instance_id.is_empty(), "Aether always has an instance id");
        self.state().instance_id = instance_id.to_owned();
    }

    /// The protocol versions hello accepts, inclusive.
    pub fn set_protocol_window(&self, min: u32, max: u32) {
        assert!(min <= max, "empty protocol window {min}..{max}");
        self.state().serves = (min, max);
    }

    pub fn set_submission_horizon(&self, horizon: Option<String>) {
        self.state().submission_horizon = horizon;
    }

    pub fn set_features(&self, features: Vec<String>) {
        self.state().features = features;
    }

    /// Advertised in the welcome, as Aether's TCP listener does.
    pub fn set_idle_timeout(&self, secs: Option<u64>) {
        self.state().idle_timeout_secs = secs;
    }

    /// Every command received after a hello, in arrival order.
    pub fn received(&self) -> Vec<Value> {
        self.state().received.clone()
    }

    /// The received commands named `cmd`.
    pub fn received_named(&self, cmd: &str) -> Vec<Value> {
        let state = self.state();
        state
            .received
            .iter()
            .filter(|c| c["cmd"] == cmd)
            .cloned()
            .collect()
    }

    /// Connections accepted since start, including closed ones.
    pub fn connections_accepted(&self) -> u64 {
        self.state().accepted
    }

    /// Live subscriptions across all connections.
    pub fn live_subscriptions(&self) -> usize {
        self.state().connections.iter().map(|c| c.subs.len()).sum()
    }

    /// Wait until at least `count` commands named `cmd` have arrived.
    pub async fn wait_for(&self, cmd: &str, count: usize, limit: Duration) -> Vec<Value> {
        let waited = tokio::time::timeout(limit, async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let seen = self.received_named(cmd);
                if seen.len() >= count {
                    return seen;
                }
                notified.await;
            }
        });
        match waited.await {
            Ok(seen) => seen,
            Err(_) => panic!(
                "fake aetherd saw {} `{cmd}` within {limit:?}, wanted {count}",
                self.received_named(cmd).len()
            ),
        }
    }

    /// Append an event to the log and push it to matching subscriptions.
    /// `seq` is assigned here; `id`, `at` and `actor` are filled when absent.
    pub fn push_event(&self, mut event: Value) -> u64 {
        assert!(event.is_object(), "an event is a JSON object");
        assert!(
            event["kind"]["type"].is_string(),
            "an event needs kind.type"
        );
        let mut state = self.state();
        let seq = state.events.len() as u64 + 1;
        fill_envelope(&mut event, seq);
        for connection in &state.connections {
            for (sub, query) in &connection.subs {
                if matches(query, &event) {
                    let _gone = connection.out.send(Out::Line(event_line(*sub, &event)));
                }
            }
        }
        state.events.push(event);
        seq
    }

    /// Hang up every live connection, optionally after an id-0 notice.
    pub fn hangup(&self, notice: Option<(&str, &str)>) {
        let mut state = self.state();
        for connection in state.connections.drain(..) {
            if let Some((code, message)) = notice {
                let _gone = connection.out.send(Out::Line(notice_line(code, message)));
            }
            let _gone = connection.out.send(Out::Close);
        }
    }
}

impl Drop for FakeAether {
    fn drop(&mut self) {
        self.accept.abort();
        self.hangup(None);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // Test scaffolding: a panicking test thread must not hide the fake's state.
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn accept_loop(listener: UnixListener, state: Arc<Mutex<State>>, changed: Arc<Notify>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::error!(%error, "fake aetherd stopped accepting");
                return;
            }
        };
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let id = {
            let mut state = lock(&state);
            let id = state.next_connection;
            state.next_connection += 1;
            state.accepted += 1;
            id
        };
        changed.notify_waiters();
        let session = Session {
            id,
            state: state.clone(),
            changed: changed.clone(),
            out: out_tx,
            version: None,
        };
        tokio::spawn(session.run(stream, out_rx));
    }
}

/// One client connection inside the fake.
struct Session {
    id: u64,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    out: mpsc::UnboundedSender<Out>,
    /// The protocol the hello settled on; `None` until welcomed.
    version: Option<u32>,
}

impl Session {
    async fn run(mut self, stream: UnixStream, mut out_rx: mpsc::UnboundedReceiver<Out>) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => self.handle(&line),
                    Ok(None) | Err(_) => break,
                },
                out = out_rx.recv() => match out {
                    Some(Out::Line(line)) => {
                        if write.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Some(Out::Close) | None => break,
                },
            }
        }
        let _closing = write.shutdown().await;
        lock(&self.state).connections.retain(|c| c.id != self.id);
    }

    fn send(&self, line: String) {
        let _gone = self.out.send(Out::Line(line));
    }

    fn handle(&mut self, line: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            self.send(reply_line(
                0,
                &Scripted::refuse("invalid_argument", "bad frame"),
            ));
            return;
        };
        let req = frame["req"].as_u64().unwrap_or(0);
        let cmd = frame["cmd"].as_str().unwrap_or_default().to_owned();
        let Some(version) = self.version else {
            self.hello(req, &frame);
            return;
        };
        lock(&self.state).received.push(frame.clone());
        self.changed.notify_waiters();
        // Refused before any responder runs, so a refused save changes nothing.
        let saved = (cmd == "configure_github_watcher").then(|| &frame["config"]);
        if let Some(refusal) = saved.and_then(|config| watcher_gate(version, config)) {
            self.answer(req, refusal);
            return;
        }
        let scripted = self.scripted(&cmd, &frame).map(|reply| match &reply {
            Scripted::Reply { kind, data } if kind == "github_watcher" => {
                watcher_gate(version, &data["config"]).unwrap_or(reply)
            }
            _ => reply,
        });
        match (cmd.as_str(), scripted) {
            ("subscribe", None) => self.subscribe(req, &frame),
            ("unsubscribe", None) => self.unsubscribe(req, &frame),
            (_, Some(reply)) => self.answer(req, reply),
            (_, None) => self.answer(req, builtin(&cmd)),
        }
    }

    fn scripted(&self, cmd: &str, frame: &Value) -> Option<Scripted> {
        let responder = {
            let mut state = lock(&self.state);
            if let Some(next) = state.scripts.get_mut(cmd).and_then(VecDeque::pop_front) {
                return Some(next);
            }
            state.responders.get(cmd).cloned()
        };
        responder.map(|respond| respond(frame))
    }

    fn answer(&self, req: u64, reply: Scripted) {
        match reply {
            Scripted::Silent => {}
            Scripted::Hangup { notice } => {
                if let Some((code, message)) = notice {
                    self.send(notice_line(&code, &message));
                }
                let _gone = self.out.send(Out::Close);
            }
            reply => self.send(reply_line(req, &reply)),
        }
    }

    fn hello(&mut self, req: u64, frame: &Value) {
        let state = lock(&self.state);
        let (min, max) = state.serves;
        let asked = frame["protocol_version"].as_u64().unwrap_or(0);
        let reply = match (frame["cmd"] == "hello", u32::try_from(asked)) {
            (false, _) => Scripted::refuse("incompatible", "first frame must be hello"),
            (true, Ok(v)) if (min..=max).contains(&v) => Scripted::reply(
                "welcome",
                json!({
                    "protocol_version": v, "daemon_version": "0.0.0-fake",
                    "instance_id": state.instance_id, "schema_version": 1,
                    "pid": std::process::id(), "features": state.features,
                    "daemon_protocol_version": max, "min_protocol_version": min,
                    "idle_timeout_secs": state.idle_timeout_secs,
                    "submission_horizon": state.submission_horizon,
                }),
            ),
            (true, _) => Scripted::refuse(
                "incompatible",
                &format!(
                    "client protocol {asked} is not served: this daemon speaks {min} to {max}"
                ),
            ),
        };
        drop(state);
        let accepted = matches!(reply, Scripted::Reply { .. });
        self.send(reply_line(req, &reply));
        if !accepted {
            let _gone = self.out.send(Out::Close);
            return;
        }
        self.version = u32::try_from(asked).ok();
        assert!(
            self.version.is_some(),
            "a welcomed version is in the window"
        );
        lock(&self.state).connections.push(Connection {
            id: self.id,
            out: self.out.clone(),
            subs: Vec::new(),
            next_sub: 1,
        });
    }

    /// Replays, then answers, then follows: all under the state lock, so a
    /// concurrent `push_event` is delivered exactly once.
    fn subscribe(&self, req: u64, frame: &Value) {
        let mut state = lock(&self.state);
        let last = state.events.len() as u64;
        let start = frame["after"].as_u64().unwrap_or(last);
        let query = frame["query"].clone();
        let replay: Vec<Value> = state
            .events
            .iter()
            .skip(usize::try_from(start).unwrap_or(usize::MAX))
            .filter(|event| matches(&query, event))
            .cloned()
            .collect();
        let Some(connection) = state.connections.iter_mut().find(|c| c.id == self.id) else {
            return;
        };
        let sub = connection.next_sub;
        connection.next_sub += 1;
        connection.subs.push((sub, query));
        for event in &replay {
            self.send(event_line(sub, event));
        }
        let reply = Scripted::reply("subscribed", json!({"id": sub, "last_seq": start}));
        self.send(reply_line(req, &reply));
    }

    fn unsubscribe(&self, req: u64, frame: &Value) {
        let sub = frame["id"].as_u64().unwrap_or(0);
        let mut state = lock(&self.state);
        if let Some(connection) = state.connections.iter_mut().find(|c| c.id == self.id) {
            connection.subs.retain(|(id, _)| *id != sub);
        }
        drop(state);
        self.send(reply_line(req, &Scripted::ok()));
    }
}

/// Aether's refusal of a watcher `config` that a peer speaking `version`
/// cannot carry, sent or received; `None` when it can, or for no config.
fn watcher_gate(version: u32, config: &Value) -> Option<Scripted> {
    let set = |field: &Value| !field.is_null();
    let narrowed = config["repositories"]
        .as_array()
        .is_some_and(|repos| repos.iter().any(|repo| set(&repo["allowed_users"])));
    if version < ALLOWLIST_PROTOCOL_VERSION && narrowed {
        return Some(Scripted::refuse(
            "incompatible",
            ALLOWLIST_WATCHER_VERSION_ERROR,
        ));
    }
    if version < OPERATOR_LOGINS_PROTOCOL_VERSION && set(&config["operator_users"]) {
        return Some(Scripted::refuse(
            "incompatible",
            OPERATOR_USERS_WATCHER_VERSION_ERROR,
        ));
    }
    None
}

fn builtin(cmd: &str) -> Scripted {
    match cmd {
        "ping" => Scripted::ok(),
        "status" => Scripted::reply("status", json!({"network_default": "allowed"})),
        _ => Scripted::refuse(
            "internal",
            &format!("fake aetherd has no reply scripted for `{cmd}`"),
        ),
    }
}

fn reply_line(req: u64, reply: &Scripted) -> String {
    let result = match reply {
        Scripted::Reply { kind, data } => json!({"Ok": {"reply": kind, "data": data}}),
        Scripted::Refuse { code, message } => {
            json!({"Err": {"code": code, "message": message}})
        }
        Scripted::Silent | Scripted::Hangup { .. } => {
            unreachable!("silent and hangup scripts are never encoded")
        }
    };
    format!(
        "{}\n",
        json!({"frame": "reply", "id": req, "result": result})
    )
}

fn notice_line(code: &str, message: &str) -> String {
    reply_line(0, &Scripted::refuse(code, message))
}

fn event_line(sub: u64, event: &Value) -> String {
    format!(
        "{}\n",
        json!({"frame": "event", "sub": sub, "event": event})
    )
}

fn fill_envelope(event: &mut Value, seq: u64) {
    let object = event.as_object_mut().expect("checked by the caller");
    object.insert("seq".to_owned(), json!(seq));
    object
        .entry("id")
        .or_insert_with(|| json!(format!("evt_{seq}")));
    object
        .entry("at")
        .or_insert_with(|| json!(chrono::Utc::now().to_rfc3339()));
    object
        .entry("actor")
        .or_insert_with(|| json!({"type": "system"}));
}

/// Aether's `EventFilter::matches`: an agent filter also takes its children.
fn matches(query: &Value, event: &Value) -> bool {
    if let Some(agent) = query.get("agent").filter(|a| !a.is_null())
        && event.get("agent_id") != Some(agent)
        && event.get("parent_agent_id") != Some(agent)
    {
        return false;
    }
    for (field, key) in [("task", "task_id"), ("run", "run")] {
        if let Some(wanted) = query.get(field).filter(|w| !w.is_null())
            && event.get(key) != Some(wanted)
        {
            return false;
        }
    }
    let type_name = event["kind"]["type"].as_str().unwrap_or_default();
    let family = type_name.split('.').next().unwrap_or_default();
    let listed = |key: &str, value: &str| match query[key].as_array() {
        Some(list) if !list.is_empty() => list.iter().any(|v| v == value),
        _ => true,
    };
    listed("families", family) && listed("types", type_name)
}

/// An event envelope for `push_event`; add `parent_agent_id` or `run` by
/// indexing the returned value.
pub fn event(agent: Option<&str>, task: Option<&str>, kind: Value) -> Value {
    let mut event = json!({ "kind": kind });
    if let Some(agent) = agent {
        event["agent_id"] = json!(agent);
    }
    if let Some(task) = task {
        event["task_id"] = json!(task);
    }
    event
}

/// The Aether run of every `agent` payload.
pub const FAKE_RUN: &str = "run_0001";

/// An `Agent` payload with the fields Archon reads.
pub fn agent(id: &str, current_task: Option<&str>) -> Value {
    json!({
        "id": id, "role": "worker", "name": id,
        "scope": {}, "model": {"provider": "claude-code", "model": "claude-opus-5-5"},
        "status": {"state": "ready"}, "paused": false, "current_task": current_task,
        "run": FAKE_RUN,
    })
}

/// A `Task` payload; `status` is e.g. `{"state":"active"}`.
pub fn task(id: &str, owner: Option<&str>, run: &str, status: Value) -> Value {
    json!({
        "id": id, "objective": "fake objective", "owner": owner,
        "creator": {"type": "system"}, "status": status, "run": run,
        "created_at": "2026-09-26T10:00:00Z", "updated_at": "2026-09-26T10:00:00Z",
    })
}

/// A `MemoryRecord` an agent proposed in repository `repo`, with its text.
pub fn memory(id: &str, repo: &str, text: &str) -> Value {
    json!({
        "id": id, "scope": {"scope": "repository", "id": repo}, "kind": "constraint",
        "text": text, "importance": 0.5, "confidence": 0.7,
        "provenance": {"actor": {"actor": "agent", "id": "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV"},
                       "agent": "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV"},
        "created_at": "2026-09-01T08:00:00Z", "forgotten_at": null, "superseded_by": null,
        "pinned": false, "tags": [], "retrieval_count": 3,
    })
}

/// A feedback receipt for a record command, reusable for keyed replay tests.
pub fn memory_feedback(command: &Value, id: &str) -> Value {
    assert!(
        id.starts_with("mfb_"),
        "feedback receipts have their own ids"
    );
    assert_eq!(command["cmd"], "record_memory_feedback");
    json!({
        "id": id, "memory": command["memory"], "task": command["task"],
        "verdict": command["verdict"], "evidence": command["evidence"],
        "reason": command["reason"], "actor": {"actor": "user"},
        "created_at": "2026-10-15T09:00:00Z"
    })
}

/// One stored snapshot in Aether's context reply; no memory text is exposed.
pub fn memory_context(task: &str, memories: &[&str], truncated: bool) -> Value {
    assert!(task.starts_with("tsk_"), "context belongs to a task");
    assert!(memories.iter().all(|m| m.starts_with("mem_")));
    json!({"snapshots": [{
        "snapshot": "snp_01ARZ3NDEKTSV4RRFFQ69G5FAV", "invocations": [],
        "agent": "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV", "task": task,
        "turn": 1, "at": "2026-10-15T09:00:00Z", "memories": memories
    }], "truncated": truncated})
}
