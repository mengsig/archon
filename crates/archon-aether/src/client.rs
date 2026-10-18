//! One multiplexed connection to `aetherd` (ARCHITECTURE §4.1–4.3).
//!
//! Contract: one reader task routes replies to waiting requests by id and
//! events to their subscription's channel; a dropped connection fails every
//! waiter and closes every channel with the disconnect reason (the id-0
//! notice when the daemon sent one).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::protocol::McpHealth;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::connection::{BoxRead, BoxWrite, EventSink, Router, Shared, WRITE_QUEUE, run_writer};
use crate::events::AetherEvent;
use crate::github::{WatcherConfig, WatcherStatus};
use crate::wire::{
    ALLOWLIST_PROTOCOL_VERSION, Agent, AgentPatch, AgentQuery, CLIENT_NAME, Command, CreateAgent,
    DaemonDefaults, ErrorReply, EventQuery, Hello, MIN_PROTOCOL_VERSION, ModelInfo,
    NOTE_LEVEL_PROTOCOL_VERSION, NOTES_FEATURE, NoteLevel, OPERATOR_LOGINS_PROTOCOL_VERSION,
    PROTOCOL_VERSION, ProviderInfo, RawReply, Subscribed, Task, Welcome,
};

/// Default per-request timeout; nothing waits forever on a silent daemon.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Default events buffered per subscription before it is closed as overflowed.
pub const EVENT_CAPACITY: usize = 4096;

/// Aether's floor for a TCP token (`aether_protocol::auth::MIN_TOKEN_CHARS`).
const MIN_TOKEN_CHARS: usize = 32;

#[derive(Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    /// `host:port` and the token (at least 32 characters).
    Tcp {
        address: String,
        token: String,
    },
}

/// Manual so a `?endpoint` in a log line can never print the TCP token.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => f.debug_tuple("Unix").field(path).finish(),
            Self::Tcp { address, .. } => f
                .debug_struct("Tcp")
                .field("address", address)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl Endpoint {
    /// `[aether] address` + token env when set, else the configured socket,
    /// else Aether's own resolution (`AETHER_SOCKET`, runtime dir, data dir).
    pub fn from_config(config: &archon_core::config::AetherConfig) -> Result<Self, ArchonError> {
        if let Some(address) = &config.address {
            let token = std::env::var(&config.token_env).map_err(|_| {
                ArchonError::Config(format!(
                    "[aether] address is set but ${} holds no token",
                    config.token_env
                ))
            })?;
            if token.chars().count() < MIN_TOKEN_CHARS {
                return Err(ArchonError::Config(format!(
                    "${} must hold at least {MIN_TOKEN_CHARS} characters",
                    config.token_env
                )));
            }
            return Ok(Self::Tcp {
                address: address.clone(),
                token,
            });
        }
        Ok(Self::Unix(
            config.socket.clone().unwrap_or_else(default_socket_path),
        ))
    }
}

/// Where `aetherd` listens, resolved exactly as Aether resolves it.
pub fn default_socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("AETHER_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("aether").join("aetherd.sock");
    }
    let home_data = || std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"));
    let data = std::env::var_os("AETHER_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(home_data);
            base.map(|b| b.join("aether"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    data.join("aetherd.sock")
}

/// Tuning for one connection. The defaults are what `archond` runs with;
/// tests shrink them to exercise timeouts and overflow quickly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientOptions {
    pub request_timeout: Duration,
    /// Events buffered per subscription; overflow closes the subscription.
    pub event_capacity: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: REQUEST_TIMEOUT,
            event_capacity: EVENT_CAPACITY,
        }
    }
}

/// A live subscription; events arrive in `seq` order. When the stream ends
/// (disconnect, overflow) [`Subscription::next`] says why, so the pump resubscribes.
/// Dropping it unsubscribes.
#[derive(Debug)]
pub struct Subscription {
    pub id: u64,
    /// The seq the stream starts after: `after` when given, else the log's
    /// head. With `after: None`, persist it as the cursor before any event.
    pub last_seq: u64,
    pub events: mpsc::Receiver<AetherEvent>,
    closed: Arc<OnceLock<String>>,
    released: Arc<Notify>,
}

/// Hands the release to the connection's reader task, which unsubscribes;
/// closing first lets the reader see which stream was dropped. Never blocks.
impl Drop for Subscription {
    fn drop(&mut self) {
        self.events.close();
        self.released.notify_one();
    }
}

impl Subscription {
    /// The next event, or `Err` with the close reason once every event
    /// accepted before the close has been handed out.
    pub async fn next(&mut self) -> Result<AetherEvent, ArchonError> {
        match self.events.recv().await {
            Some(event) => Ok(event),
            None => Err(ArchonError::Unavailable(self.close_reason())),
        }
    }

    /// Why the stream ended; a generic reason while it is still open.
    pub fn close_reason(&self) -> String {
        self.closed
            .get()
            .cloned()
            .unwrap_or_else(|| "aether connection closed".to_owned())
    }
}

/// Background tasks of one connection, aborted together when it is dropped.
#[derive(Debug, Default)]
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub struct AetherClient {
    shared: Arc<Shared>,
    welcome: Welcome,
    options: ClientOptions,
    _tasks: Tasks,
}

/// How a hello went: welcomed, or refused as `incompatible` with Aether's
/// message, which names the window it serves.
enum Handshake {
    Welcomed(AetherClient),
    Incompatible(String),
}

impl AetherClient {
    /// Handshake at protocol 23, falling back to the newest served version down to 14.
    /// Incompatible refusals report both protocol windows.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, ArchonError> {
        Self::connect_with(endpoint, ClientOptions::default()).await
    }

    pub async fn connect_with(
        endpoint: &Endpoint,
        options: ClientOptions,
    ) -> Result<Self, ArchonError> {
        assert!(
            options.event_capacity > 0,
            "event_capacity must be positive"
        );
        let refusal = match Self::connect_at(endpoint, options, PROTOCOL_VERSION).await? {
            Handshake::Welcomed(client) => return Ok(client),
            Handshake::Incompatible(message) => message,
        };
        // Aether hangs up after refusing a hello, so the fallback reconnects.
        let Some(older) = fallback_version(&refusal) else {
            return Err(ArchonError::Aether(incompatible_message(&refusal)));
        };
        tracing::warn!(
            protocol = older,
            "aetherd serves an older Aether protocol; GitHub watcher allowlists need \
             {ALLOWLIST_PROTOCOL_VERSION} and operator logins {OPERATOR_LOGINS_PROTOCOL_VERSION}"
        );
        match Self::connect_at(endpoint, options, older).await? {
            Handshake::Welcomed(client) => Ok(client),
            Handshake::Incompatible(message) => {
                Err(ArchonError::Aether(incompatible_message(&message)))
            }
        }
    }

    /// One connection whose hello asks for protocol `version`.
    async fn connect_at(
        endpoint: &Endpoint,
        options: ClientOptions,
        version: u32,
    ) -> Result<Handshake, ArchonError> {
        let (read, write) = open(endpoint, options.request_timeout).await?;
        let (lines_tx, lines_rx) = mpsc::channel(WRITE_QUEUE);
        let shared = Arc::new(Shared::new(lines_tx, options.request_timeout));
        let mut tasks = Tasks::default();
        tasks
            .0
            .push(tokio::spawn(run_writer(write, lines_rx, shared.clone())));
        let router = Router::new(shared.clone(), options.event_capacity);
        tasks.0.push(tokio::spawn(router.run(read)));
        let token = match endpoint {
            Endpoint::Tcp { token, .. } => Some(token.clone()),
            Endpoint::Unix(_) => None,
        };
        let welcome = match handshake(&shared, token, version).await? {
            Ok(welcome) => welcome,
            Err(message) => return Ok(Handshake::Incompatible(message)),
        };
        if let Some(idle) = welcome.idle_timeout_secs {
            tasks.0.push(tokio::spawn(keepalive(
                shared.clone(),
                keepalive_period(idle),
            )));
        }
        tracing::info!(
            daemon = %welcome.daemon_version,
            instance = %welcome.instance_id,
            protocol = welcome.protocol_version,
            "connected to aetherd"
        );
        Ok(Handshake::Welcomed(Self {
            shared,
            welcome,
            options,
            _tasks: tasks,
        }))
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// `false` once the reader or writer saw the connection end.
    pub fn is_connected(&self) -> bool {
        self.shared.disconnect_reason().is_none()
    }

    /// Why the connection ended (the id-0 notice when Aether sent one).
    pub fn disconnect_reason(&self) -> Option<String> {
        self.shared.disconnect_reason()
    }

    /// Send one command and return the reply payload after checking its
    /// kind equals `expected` (e.g. `"agent"`, `"task"`, `"ok"`).
    pub async fn request(
        &self,
        command: Command,
        expected: &str,
    ) -> Result<serde_json::Value, ArchonError> {
        assert!(!expected.is_empty(), "every Aether reply has a kind");
        let raw = self.shared.call(&command, None).await??;
        check_kind(&raw, expected)?;
        Ok(raw.data)
    }

    async fn typed<T: DeserializeOwned>(
        &self,
        command: Command,
        expected: &str,
    ) -> Result<T, ArchonError> {
        let data = self.request(command, expected).await?;
        serde_json::from_value(data).map_err(|error| {
            ArchonError::Protocol(format!("`{expected}` reply did not decode: {error}"))
        })
    }

    /// Stream events with `seq > after` (`None`: live only). Deltas are
    /// never requested.
    pub async fn subscribe(
        &self,
        after: Option<u64>,
        query: EventQuery,
    ) -> Result<Subscription, ArchonError> {
        let (events_tx, events) = mpsc::channel(self.options.event_capacity);
        let closed = Arc::new(OnceLock::new());
        let sink = EventSink::new(events_tx, closed.clone());
        // Built before the request, so a caller cancelled as the reply
        // arrives still releases the stream the reader installs.
        let mut subscription = Subscription {
            id: 0,
            last_seq: 0,
            events,
            closed,
            released: self.shared.released(),
        };
        let command = Command::Subscribe {
            after,
            query,
            deltas: false,
        };
        let raw = self.shared.call(&command, Some(sink)).await??;
        check_kind(&raw, "subscribed")?;
        let subscribed: Subscribed = serde_json::from_value(raw.data)?;
        assert!(subscribed.id > 0, "the router refuses subscription id 0");
        subscription.id = subscribed.id;
        subscription.last_seq = subscribed.last_seq;
        Ok(subscription)
    }

    pub async fn ping(&self) -> Result<(), ArchonError> {
        self.request(Command::Ping, "ok").await.map(drop)
    }

    pub async fn create_agent(&self, request: CreateAgent) -> Result<Agent, ArchonError> {
        assert!(!request.role.is_empty(), "Aether requires a role");
        if let Some(config) = &request.config
            && config.host_context != self.welcome.effective_host_context(config.host_context)
        {
            return Err(ArchonError::Invalid(
                "Aether lacks host_context; negotiate full before creating an agent".into(),
            ));
        }
        if request.repository_root.is_some()
            && request
                .scope
                .as_ref()
                .is_some_and(|scope| scope.repository.is_some())
        {
            return Err(ArchonError::Invalid(
                "repository_root and scope.repository are exclusive".into(),
            ));
        }
        if request
            .repository_root
            .as_ref()
            .is_some_and(|root| !root.is_absolute())
        {
            return Err(ArchonError::Invalid(
                "repository_root must be a registered absolute checkout".into(),
            ));
        }
        if request.repository_root.is_some() && !self.welcome.supports_repository_root() {
            return Err(ArchonError::Unavailable(
                "Aether lacks repository_root required by the frozen creation request".into(),
            ));
        }
        let agent: Agent = self
            .typed(Command::CreateAgent(Box::new(request)), "agent")
            .await?;
        Ok(agent)
    }

    pub async fn get_agent(&self, id: &AetherAgentId) -> Result<Agent, ArchonError> {
        let agent: Agent = self
            .typed(Command::GetAgent { id: id.clone() }, "agent")
            .await?;
        ensure_same(id.as_str(), agent.id.as_str(), "get_agent")?;
        Ok(agent)
    }

    /// The agents `query` selects, as Aether holds them now.
    pub async fn list_agents(&self, query: AgentQuery) -> Result<Vec<Agent>, ArchonError> {
        self.typed(Command::ListAgents { query }, "agents").await
    }

    pub async fn get_task(&self, id: &AetherTaskId) -> Result<Task, ArchonError> {
        let task: Task = self
            .typed(Command::GetTask { id: id.clone() }, "task")
            .await?;
        ensure_same(id.as_str(), task.id.as_str(), "get_task")?;
        Ok(task)
    }

    pub async fn update_agent(
        &self,
        id: &AetherAgentId,
        patch: AgentPatch,
    ) -> Result<Agent, ArchonError> {
        let command = Command::UpdateAgent {
            id: id.clone(),
            patch,
        };
        let agent: Agent = self.typed(command, "agent").await?;
        ensure_same(id.as_str(), agent.id.as_str(), "update_agent")?;
        Ok(agent)
    }

    /// Add a display-only note to `agent`'s transcript; it starts no turn. A
    /// `level` is sent only to an aetherd that speaks protocol 24.
    pub async fn append_note(
        &self,
        agent: &AetherAgentId,
        text: &str,
        level: Option<NoteLevel>,
    ) -> Result<(), ArchonError> {
        assert!(!text.trim().is_empty(), "a note says something");
        let welcome = self.welcome();
        if !welcome.features.iter().any(|f| f == NOTES_FEATURE) {
            return Err(ArchonError::Unavailable(format!(
                "aetherd {} does not serve append_note (protocol {})",
                welcome.daemon_version, welcome.protocol_version
            )));
        }
        let level = level.filter(|_| welcome.protocol_version >= NOTE_LEVEL_PROTOCOL_VERSION);
        let command = Command::AppendNote {
            agent: agent.clone(),
            text: text.to_owned(),
            level,
        };
        self.request(command, "note_appended").await.map(drop)
    }

    pub async fn send_message(
        &self,
        to: &AetherAgentId,
        content: &str,
        idempotency_key: &str,
    ) -> Result<(), ArchonError> {
        assert!(
            !idempotency_key.is_empty(),
            "operator messages are always keyed"
        );
        let command = Command::SendMessage {
            to: to.clone(),
            content: content.to_owned(),
            subtree: false,
            idempotency_key: Some(idempotency_key.to_owned()),
        };
        self.request(command, "message_sent").await.map(drop)
    }

    /// One page of the events matching `query` with `seq > after`, oldest
    /// first; Aether caps a page at 5000.
    pub async fn events_after(
        &self,
        after: u64,
        query: EventQuery,
        limit: usize,
    ) -> Result<Vec<AetherEvent>, ArchonError> {
        assert!(limit > 0, "a page holds at least one event");
        let command = Command::GetEvents {
            after,
            limit,
            query,
        };
        let raw: Vec<serde_json::Value> = self.typed(command, "events").await?;
        let events = raw
            .into_iter()
            .map(crate::events::parse_event)
            .collect::<Result<Vec<AetherEvent>, _>>()
            .map_err(|e| ArchonError::Protocol(format!("`events` reply did not decode: {e}")))?;
        if events.len() > limit || events.iter().any(|e| e.seq <= after) {
            return Err(ArchonError::Protocol(format!(
                "`events` reply is not a page of at most {limit} events after {after}"
            )));
        }
        Ok(events)
    }

    pub async fn trigger(
        &self,
        agent: &AetherAgentId,
        payload: serde_json::Value,
        idempotency_key: &str,
    ) -> Result<(), ArchonError> {
        assert!(
            !idempotency_key.is_empty(),
            "archon triggers are always keyed"
        );
        let command = Command::Trigger {
            agent: agent.clone(),
            source: CLIENT_NAME.to_owned(),
            payload,
            idempotency_key: Some(idempotency_key.to_owned()),
        };
        self.request(command, "ok").await.map(drop)
    }

    /// Keyed by the question, so a retry after a lost acknowledgement gets
    /// the first delivery back instead of a refusal of an answered question.
    pub async fn reply_to_question(
        &self,
        to: &AetherAgentId,
        question_seq: u64,
        content: &str,
    ) -> Result<(), ArchonError> {
        assert!(question_seq > 0, "event seqs start at 1");
        let command = Command::ReplyToQuestion {
            to: to.clone(),
            question_seq,
            content: content.to_owned(),
            idempotency_key: Some(format!("archon-answer-{to}-{question_seq}")),
        };
        self.request(command, "message_sent").await.map(drop)
    }

    pub async fn cancel(&self, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError> {
        let command = Command::Cancel {
            agent: agent.clone(),
            reason: Some(reason.to_owned()),
        };
        self.request(command, "ok").await.map(drop)
    }

    /// Stop the turn `agent` is in. `false` when it was in none.
    pub async fn interrupt(&self, agent: &AetherAgentId) -> Result<bool, ArchonError> {
        #[derive(serde::Deserialize)]
        struct Interrupted {
            was_running: bool,
        }
        let command = Command::Interrupt {
            agent: agent.clone(),
        };
        let reply: Interrupted = self.typed(command, "interrupted").await?;
        Ok(reply.was_running)
    }

    /// Revive a failed agent in place; `Err` when Aether refuses.
    pub async fn wake(&self, agent: &AetherAgentId) -> Result<Agent, ArchonError> {
        let woken: Agent = self
            .typed(
                Command::Wake {
                    agent: agent.clone(),
                },
                "agent",
            )
            .await?;
        ensure_same(agent.as_str(), woken.id.as_str(), "wake")?;
        Ok(woken)
    }

    /// Recreate a failed or cancelled agent from its checkpoint as a new id.
    pub async fn restart(&self, agent: &AetherAgentId) -> Result<Agent, ArchonError> {
        let command = Command::Restart {
            agent: agent.clone(),
        };
        let successor: Agent = self.typed(command, "agent").await?;
        if successor.id == *agent {
            return Err(ArchonError::Protocol(format!(
                "restart of {agent} returned the same id, not a successor"
            )));
        }
        Ok(successor)
    }

    /// Pause (`true`) or resume (`false`) an agent and its whole subtree.
    pub async fn pause(&self, agent: &AetherAgentId, paused: bool) -> Result<(), ArchonError> {
        let agent = agent.clone();
        let (command, expected) = match paused {
            true => (
                Command::Pause {
                    agent,
                    subtree: true,
                },
                "paused",
            ),
            false => (
                Command::Resume {
                    agent,
                    subtree: true,
                },
                "agents",
            ),
        };
        self.request(command, expected).await.map(drop)
    }

    /// Aether's defaults for new roots, read from `status`.
    pub async fn daemon_defaults(&self) -> Result<DaemonDefaults, ArchonError> {
        self.typed(Command::Status, "status").await
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ArchonError> {
        self.typed(Command::ListModels, "models").await
    }

    pub async fn providers(&self) -> Result<Vec<ProviderInfo>, ArchonError> {
        self.typed(Command::Providers, "providers").await
    }

    /// Aether's view of the MCP server `server`, without contacting it.
    pub async fn mcp_health(&self, server: &str) -> Result<McpHealth, ArchonError> {
        self.mcp_servers(server, false).await
    }

    /// Ask Aether to reconnect the MCP server `server`; its health after.
    pub async fn refresh_mcp(&self, server: &str) -> Result<McpHealth, ArchonError> {
        self.mcp_servers(server, true).await
    }

    /// The GitHub watcher's config, revision and per-repository health.
    pub async fn github_watcher(&self) -> Result<WatcherStatus, ArchonError> {
        self.watcher(Command::GetGithubWatcher).await
    }

    /// One watcher command. Aether answers `incompatible` instead of a
    /// watcher this protocol cannot carry; that is restated with its cause.
    async fn watcher(&self, command: Command) -> Result<WatcherStatus, ArchonError> {
        let raw = match self.shared.call(&command, None).await? {
            Ok(raw) => raw,
            Err(refusal) => return Err(self.watcher_refusal(refusal)),
        };
        check_kind(&raw, "github_watcher")?;
        serde_json::from_value(raw.data).map_err(|error| {
            ArchonError::Protocol(format!("`github_watcher` reply did not decode: {error}"))
        })
    }

    /// Aether's refusal of a watcher command, naming operator logins when
    /// they are why: a list is set that this connection's protocol hides.
    fn watcher_refusal(&self, refusal: ErrorReply) -> ArchonError {
        let speaking = self.welcome.protocol_version;
        let hidden_list = refusal.code == "incompatible"
            && refusal.message.contains("operator_users")
            && speaking < OPERATOR_LOGINS_PROTOCOL_VERSION;
        if !hidden_list {
            return refusal.into();
        }
        ArchonError::Aether(format!(
            "aetherd {} has GitHub operator logins set, which Aether protocol {speaking} \
             cannot carry (they need {OPERATOR_LOGINS_PROTOCOL_VERSION}), so Archon can \
             neither read nor change the watcher without dropping them; clear the list \
             with `archon aether github-watcher operator-users --clear`. \
             Aether said: {}",
            self.welcome.daemon_version, refusal.message
        ))
    }

    /// Refuses unless aetherd speaks a protocol that stores GitHub watcher
    /// allowlists; protocol 14 would silently drop one.
    pub fn check_allowlists(&self) -> Result<(), ArchonError> {
        let speaking = self.welcome.protocol_version;
        assert!(speaking >= MIN_PROTOCOL_VERSION, "hello refuses older ones");
        if speaking >= ALLOWLIST_PROTOCOL_VERSION {
            return Ok(());
        }
        Err(ArchonError::Aether(format!(
            "aetherd {} speaks Aether protocol {speaking}, which cannot store a GitHub \
             watcher allowlist (it needs {ALLOWLIST_PROTOCOL_VERSION}); upgrade Archon's \
             aetherd, then set the allowlist again. Nothing was changed",
            self.welcome.daemon_version
        )))
    }

    /// Refuses unless aetherd speaks a protocol that stores GitHub operator
    /// logins; an older one cannot keep the list.
    pub fn check_operator_users(&self) -> Result<(), ArchonError> {
        let speaking = self.welcome.protocol_version;
        assert!(speaking >= MIN_PROTOCOL_VERSION, "hello refuses older ones");
        if speaking >= OPERATOR_LOGINS_PROTOCOL_VERSION {
            return Ok(());
        }
        Err(ArchonError::Aether(format!(
            "aetherd {} speaks Aether protocol {speaking}, which cannot store GitHub \
             operator logins (they need {OPERATOR_LOGINS_PROTOCOL_VERSION}); upgrade \
             Archon's aetherd (install the newer `aether`, then `archon daemon stop \
             --aether`; the next archon command starts it), then set the operator logins \
             again. Nothing was changed",
            self.welcome.daemon_version
        )))
    }

    /// Replace the watcher config if it is still at `expected_revision`;
    /// the new status. Aether allows this for local operators only. An
    /// allowlist or operator logins the protocol would drop are refused here.
    pub async fn configure_github_watcher(
        &self,
        config: WatcherConfig,
        expected_revision: u64,
    ) -> Result<WatcherStatus, ArchonError> {
        if config.has_allowlist() {
            self.check_allowlists()?;
        }
        if config.has_operator_users() {
            self.check_operator_users()?;
        }
        let command = Command::ConfigureGithubWatcher {
            config,
            expected_revision,
        };
        let status = self.watcher(command).await?;
        if status.revision <= expected_revision {
            return Err(ArchonError::Protocol(format!(
                "configure_github_watcher left revision {} (edited {expected_revision})",
                status.revision
            )));
        }
        Ok(status)
    }

    async fn mcp_servers(&self, server: &str, refresh: bool) -> Result<McpHealth, ArchonError> {
        assert!(!server.is_empty(), "ask about one named server, not all");
        let command = Command::McpServers {
            refresh,
            server: Some(server.to_owned()),
        };
        match self.request(command, "mcp_servers").await {
            Ok(servers) => crate::mcp::server_health(&servers, server),
            // Aether refuses to name a server it never loaded.
            Err(ArchonError::NotFound(_)) => Ok(McpHealth::NotLoaded),
            Err(error) => Err(error),
        }
    }
}

async fn open(endpoint: &Endpoint, limit: Duration) -> Result<(BoxRead, BoxWrite), ArchonError> {
    let connecting = async {
        match endpoint {
            Endpoint::Unix(path) => open_unix(path).await,
            Endpoint::Tcp { address, .. } => open_tcp(address).await,
        }
    };
    tokio::time::timeout(limit, connecting).await.map_err(|_| {
        ArchonError::Unavailable(format!("connecting to aetherd timed out after {limit:?}"))
    })?
}

async fn open_unix(path: &std::path::Path) -> Result<(BoxRead, BoxWrite), ArchonError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|e| ArchonError::io(format!("connecting to aetherd at {}", path.display()), e))?;
    let (read, write) = stream.into_split();
    Ok((Box::new(read), Box::new(write)))
}

async fn open_tcp(address: &str) -> Result<(BoxRead, BoxWrite), ArchonError> {
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|e| ArchonError::io(format!("connecting to aetherd at {address}"), e))?;
    stream
        .set_nodelay(true)
        .map_err(|e| ArchonError::io("setting TCP_NODELAY", e))?;
    let (read, write) = stream.into_split();
    Ok((Box::new(read), Box::new(write)))
}

/// Say hello at `version`; `Ok(Err(message))` is an `incompatible` refusal.
async fn handshake(
    shared: &Shared,
    token: Option<String>,
    version: u32,
) -> Result<Result<Welcome, String>, ArchonError> {
    assert!(
        (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version),
        "Archon only asks for a protocol it speaks, not {version}"
    );
    let hello = Command::Hello(Hello {
        protocol_version: version,
        client_name: CLIENT_NAME.to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        features: Vec::new(),
        token,
    });
    let raw = match shared.call(&hello, None).await? {
        Ok(raw) => raw,
        Err(refusal) if refusal.code == "incompatible" => return Ok(Err(refusal.message)),
        Err(refusal) => return Err(refusal.into()),
    };
    check_kind(&raw, "welcome")?;
    let welcome: Welcome = serde_json::from_value(raw.data)?;
    if welcome.protocol_version != version {
        return Err(ArchonError::Aether(format!(
            "Archon asked for Aether protocol {version}; the daemon answered at {}",
            welcome.protocol_version
        )));
    }
    if welcome.instance_id.is_empty() {
        return Err(ArchonError::Protocol(
            "welcome carries no instance_id".into(),
        ));
    }
    Ok(Ok(welcome))
}

/// The window in Aether's refusal "... this daemon speaks {min} to {max}".
fn served_window(message: &str) -> Option<(u32, u32)> {
    let (_, tail) = message.rsplit_once("speaks ")?;
    let mut numbers = tail
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(str::parse::<u32>);
    match (numbers.next(), numbers.next()) {
        (Some(Ok(min)), Some(Ok(max))) if min <= max => Some((min, max)),
        _ => None,
    }
}

/// The older protocol to retry at: the newest the daemon serves, when that
/// is one Archon still speaks. `None` for any other refusal.
pub(crate) fn fallback_version(message: &str) -> Option<u32> {
    let (_, max) = served_window(message)?;
    (MIN_PROTOCOL_VERSION..PROTOCOL_VERSION)
        .contains(&max)
        .then_some(max)
}

/// Aether's refusal restated as both windows. Unparseable text is passed
/// through whole.
pub(crate) fn incompatible_message(message: &str) -> String {
    let ours = format!("Archon speaks Aether protocol {MIN_PROTOCOL_VERSION}..{PROTOCOL_VERSION}");
    match served_window(message) {
        Some((min, max)) => format!("{ours}; the daemon serves {min}..{max}"),
        None => format!("{ours}; the daemon refused it: {message}"),
    }
}

fn check_kind(raw: &RawReply, expected: &str) -> Result<(), ArchonError> {
    if raw.reply == expected {
        return Ok(());
    }
    Err(ArchonError::Protocol(format!(
        "expected a `{expected}` reply, got `{}`",
        raw.reply
    )))
}

fn ensure_same(asked: &str, got: &str, command: &str) -> Result<(), ArchonError> {
    if asked == got {
        return Ok(());
    }
    Err(ArchonError::Protocol(format!(
        "{command} for {asked} answered about {got}"
    )))
}

/// Ping at a third of the idle timeout, so two lost pings still leave
/// margin before the daemon hangs up.
fn keepalive_period(idle_secs: u64) -> Duration {
    let period = Duration::from_millis(idle_secs.saturating_mul(1000) / 3);
    period.max(Duration::from_millis(100))
}

async fn keepalive(shared: Arc<Shared>, period: Duration) {
    assert!(!period.is_zero(), "a zero keepalive period would spin");
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let outcome = shared.call(&Command::Ping, None).await;
        if matches!(&outcome, Ok(Ok(raw)) if raw.reply == "ok") {
            continue;
        }
        // The reader owns the disconnect; a dead connection only stops the
        // pinging. A slow or odd answer does not: an unpinged TCP link idles out.
        if let Some(reason) = shared.disconnect_reason() {
            tracing::debug!(%reason, "aether connection closed; stopping keepalive");
            return;
        }
        tracing::warn!(?outcome, "aether keepalive ping failed; still pinging");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_endpoint_requires_a_long_token() {
        let config = archon_core::config::AetherConfig {
            address: Some("127.0.0.1:7777".into()),
            token_env: "ARCHON_TEST_SURELY_UNSET_TOKEN".into(),
            ..Default::default()
        };
        assert!(matches!(
            Endpoint::from_config(&config),
            Err(ArchonError::Config(_))
        ));
        let unix = Endpoint::from_config(&archon_core::config::AetherConfig {
            socket: Some("/run/a.sock".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(unix, Endpoint::Unix("/run/a.sock".into()));
    }

    #[test]
    fn debug_output_never_shows_the_token() {
        let endpoint = Endpoint::Tcp {
            address: "10.0.0.1:7777".into(),
            token: "s3cret-s3cret-s3cret-s3cret-s3cret".into(),
        };
        let shown = format!("{endpoint:?}");
        assert!(shown.contains("10.0.0.1:7777"));
        assert!(!shown.contains("s3cret"));
    }

    #[test]
    fn incompatible_refusals_are_restated_as_the_served_window() {
        let text = "client protocol 17 is not served: this daemon speaks 24 to 25";
        assert_eq!(
            incompatible_message(text),
            "Archon speaks Aether protocol 14..23; the daemon serves 24..25"
        );
        let odd = incompatible_message("go away");
        assert!(odd.starts_with("Archon speaks Aether protocol 14..23"));
        assert!(odd.ends_with("go away"));
    }

    #[test]
    fn an_older_daemon_is_retried_at_the_newest_protocol_it_serves() {
        let serving = |min: u32, max: u32| {
            fallback_version(&format!(
                "client protocol 17 is not served: this daemon speaks {min} to {max}"
            ))
        };
        assert_eq!(serving(15, 16), Some(16));
        assert_eq!(
            serving(14, 15),
            Some(15),
            "the aetherd before operator logins"
        );
        assert_eq!(serving(13, 14), Some(14));
        assert_eq!(serving(14, 14), Some(14));
        assert_eq!(serving(12, 13), None, "older than Archon speaks");
        assert_eq!(serving(24, 25), None, "newer: retrying older cannot help");
        assert_eq!(
            serving(16, 17),
            Some(17),
            "fall back to 17 before submissions could be resolved"
        );
        assert_eq!(fallback_version("unauthorized"), None);
    }

    #[test]
    fn keepalive_runs_at_a_third_of_the_idle_timeout() {
        assert_eq!(keepalive_period(300), Duration::from_secs(100));
        assert_eq!(keepalive_period(0), Duration::from_millis(100));
    }
}
