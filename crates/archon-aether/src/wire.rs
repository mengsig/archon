//! Aether protocol v23 wire subset, checked against its published schema.
//! Replies decode in two steps so unexpected reply kinds remain named errors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use archon_core::Timestamp;
use archon_core::aether_ids::{
    AetherAgentId, AetherMemoryFeedbackId, AetherMemoryId, AetherRunId, AetherTaskId,
};
use archon_core::attachment::ImageAttachment;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelRef};
use archon_core::model_tier::Tier;
use serde::{Deserialize, Serialize};

use crate::github::WatcherConfig;
use crate::memory::MemoryQuery;

/// Archon asks for this first, and falls back to the newest protocol an
/// older aetherd serves, down to [`MIN_PROTOCOL_VERSION`].
pub const PROTOCOL_VERSION: u32 = 24;
/// Aether's own floor: no supported aetherd serves an older protocol, and
/// from 17 on its GitHub watcher stores `allowed_users` and `operator_users`.
pub const MIN_PROTOCOL_VERSION: u32 = 17;
/// The first protocol whose `send_message` carries images.
pub const IMAGES_PROTOCOL_VERSION: u32 = 19;
/// The first protocol that serves `append_note`.
pub const NOTES_PROTOCOL_VERSION: u32 = 23;
/// The first protocol whose notes carry a `level`.
pub const NOTE_LEVEL_PROTOCOL_VERSION: u32 = 24;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const CLIENT_NAME: &str = "archon";
pub const HOST_CONTEXT_FEATURE: &str = "host_context";
pub const REPOSITORY_ROOT_FEATURE: &str = "repository_root";
pub const NOTES_FEATURE: &str = "notes";
/// The daemon serves `storage_gc` (host-only access).
pub const STORAGE_FEATURE: &str = "storage";
/// The daemon serves the notification center (`post_notification` and kin).
pub const NOTIFICATIONS_FEATURE: &str = "notifications";
/// The daemon applies `UpdateAgent.subtree`: a raised deadline or cost budget
/// also reaches the live descendants.
pub const SUBTREE_UPDATE_FEATURE: &str = "subtree_update";
/// Both sides handle the `history_expired` code; Archon lists it in its hello,
/// and an aetherd that lists it back reports expired history by that code.
pub const HISTORY_EXPIRED_FEATURE: &str = "history_expired";
/// What every aetherd's refusal says about expired history, with or without
/// the `history_expired` code: `...within sequence range 1..=2; ...`.
const HISTORY_EXPIRED_TEXT: &str = "event history expired within sequence range ";

/// How a note row is coloured in the TUI (protocol 24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteLevel {
    Applied,
    Refused,
}

/// One client frame: `{"req": id, "cmd": ..., ...fields}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientFrame<'a> {
    pub req: u64,
    #[serde(flatten)]
    pub command: &'a Command,
}

/// The commands Archon sends. Field names and optionality match Aether's
/// `Command` enum exactly; `skip_serializing_if` keeps absent fields absent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Hello(Hello),
    Status,
    Ping,
    /// Delete the workspaces of Aether's finished agents ([`STORAGE_FEATURE`]).
    /// `prune_build_output` is left to its default.
    StorageGc {
        dry_run: bool,
    },
    CreateAgent(Box<CreateAgent>),
    GetAgent {
        id: AetherAgentId,
    },
    ListAgents {
        query: AgentQuery,
    },
    UpdateAgent {
        id: AetherAgentId,
        patch: AgentPatch,
        /// Also raise every live descendant's `deadline` and `cost_budget` to
        /// the patched value where theirs is lower ([`SUBTREE_UPDATE_FEATURE`]).
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        subtree: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    SendMessage {
        to: AetherAgentId,
        content: String,
        subtree: bool,
        /// Images sent with the text (protocol 19); absent when there are none.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<ImageAttachment>,
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    ReplyToQuestion {
        to: AetherAgentId,
        question_seq: u64,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    Trigger {
        agent: AetherAgentId,
        source: String,
        payload: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// A display-only transcript row that wakes no turn (protocol 23).
    AppendNote {
        agent: AetherAgentId,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<NoteLevel>,
    },
    /// Post, or update the one `key` names; the `notifications` feature.
    PostNotification {
        source: String,
        notification: crate::notifications::NotificationPost,
    },
    ListNotifications {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        states: Vec<crate::notifications::NotificationState>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// By the `source` and `key` it was posted under.
    ResolveNotification {
        source: String,
        key: String,
    },
    Cancel {
        agent: AetherAgentId,
        reason: Option<String>,
    },
    /// Stops the running turn only; the agent waits for its next message.
    Interrupt {
        agent: AetherAgentId,
    },
    Wake {
        agent: AetherAgentId,
    },
    Restart {
        agent: AetherAgentId,
    },
    Pause {
        agent: AetherAgentId,
        subtree: bool,
    },
    Resume {
        agent: AetherAgentId,
        subtree: bool,
    },
    GetTask {
        id: AetherTaskId,
    },
    Subscribe {
        after: Option<u64>,
        query: EventQuery,
        deltas: bool,
    },
    Unsubscribe {
        id: u64,
    },
    GetEvents {
        after: u64,
        limit: usize,
        query: EventQuery,
    },
    ListModels,
    AuthStatus,
    Providers,
    McpServers {
        refresh: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        server: Option<String>,
    },
    GetGithubWatcher,
    /// Refused unless `expected_revision` is still current (Aether's CAS).
    ConfigureGithubWatcher {
        config: WatcherConfig,
        expected_revision: u64,
    },
    Memories {
        query: MemoryQuery,
    },
    ForgetMemory {
        id: AetherMemoryId,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    MemoriesInContext {
        task: AetherTaskId,
    },
    RecordMemoryFeedback(crate::memory_feedback::RecordMemoryFeedback),
    RetractMemoryFeedback {
        feedback: AetherMemoryFeedbackId,
        reason: String,
    },
    ResolveSubmission {
        command: crate::memory_feedback::FeedbackCommand,
        #[serde(skip_serializing_if = "Option::is_none")]
        horizon: Option<String>,
    },
    RestoreMemory {
        id: AetherMemoryId,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
    pub features: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Manual, like Aether's, so a logged `?command` never prints the TCP token.
impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("protocol_version", &self.protocol_version)
            .field("client_name", &self.client_name)
            .field("client_version", &self.client_version)
            .field("features", &self.features)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub daemon_version: String,
    pub instance_id: String,
    pub schema_version: u32,
    pub pid: u32,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub daemon_protocol_version: Option<u32>,
    #[serde(default)]
    pub min_protocol_version: Option<u32>,
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub submission_horizon: Option<String>,
}

impl Welcome {
    pub fn supports_subtree_update(&self) -> bool {
        self.features
            .iter()
            .any(|feature| feature == SUBTREE_UPDATE_FEATURE)
    }

    pub fn supports_history_expired(&self) -> bool {
        self.features
            .iter()
            .any(|feature| feature == HISTORY_EXPIRED_FEATURE)
    }

    pub fn supports_repository_root(&self) -> bool {
        self.features
            .iter()
            .any(|feature| feature == REPOSITORY_ROOT_FEATURE)
    }

    pub fn supports_storage(&self) -> bool {
        self.features
            .iter()
            .any(|feature| feature == STORAGE_FEATURE)
    }

    pub fn effective_host_context(
        &self,
        requested: archon_core::config::HostContext,
    ) -> archon_core::config::HostContext {
        if self
            .features
            .iter()
            .any(|feature| feature == HOST_CONTEXT_FEATURE)
        {
            requested
        } else {
            archon_core::config::HostContext::Full
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateAgent {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<AetherAgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<AgentConfig>,
    /// A root's approval policy; omitted, a root takes Aether's
    /// `[approval] default`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<AetherRunId>,
    pub start: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Aether's own default: `start` is true, so `..CreateAgent::default()`
/// never leaves an agent created but unstarted (Archon always sends `start`).
impl Default for CreateAgent {
    fn default() -> Self {
        Self {
            role: String::new(),
            name: None,
            objective: None,
            model: None,
            scope: None,
            repository_root: None,
            parent: None,
            config: None,
            approval: None,
            run: None,
            start: true,
            idempotency_key: None,
        }
    }
}

/// Which side-effect classes need approval; empty gates nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    pub require_for: Vec<String>,
}

impl ApprovalPolicy {
    /// Nothing gated: for headless agents no person watches.
    pub fn none() -> Self {
        Self::default()
    }
}

/// Aether's scope network knob, defined with the ports the daemon
/// persists it through.
pub use archon_core::ports::NetworkPolicy;

/// The part of Aether's `status` reply Archon reads.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonDefaults {
    /// Aether's `[network] default` for new roots; `allowed` when a daemon
    /// predates the field, as that is Aether's shipped default.
    #[serde(default = "shipped_network_default")]
    pub network_default: NetworkPolicy,
}

fn shipped_network_default() -> NetworkPolicy {
    NetworkPolicy::Allowed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub network: NetworkPolicy,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Aether's `AgentConfig`. Omitted fields take Aether's defaults, except
/// `tools`, which Archon always states.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "archon_core::config::HostContext::is_full")]
    pub host_context: archon_core::config::HostContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_budget: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Timestamp>,
    pub tools: ToolExposure,
    pub long_lived: bool,
    pub leaf: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolExposure {
    #[default]
    All,
    Only {
        names: BTreeSet<String>,
    },
    Except {
        names: BTreeSet<String>,
    },
}

impl ToolExposure {
    /// Whether an agent with this exposure sees the tool `name`.
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only { names } => names.contains(name),
            Self::Except { names } => !names.contains(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct AgentPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_budget: Option<f64>,
    /// `{"set": <rfc3339>}` or `"clear"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct AgentQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<AetherAgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<AetherRunId>,
    pub statuses: Vec<String>,
    pub live_only: bool,
    pub include_archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct EventQuery {
    /// `get_events` only: newest first, below this seq (`u64::MAX` for the
    /// latest). Unset reads oldest first after `after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AetherAgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<AetherTaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<AetherRunId>,
    pub families: Vec<String>,
    pub types: Vec<String>,
}

/// A daemon frame before its payload is interpreted.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Reply { id: u64, result: ReplyResult },
    Event { sub: u64, event: serde_json::Value },
    Delta { sub: u64 },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub enum ReplyResult {
    Ok(RawReply),
    Err(ErrorReply),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawReply {
    pub reply: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, thiserror::Error)]
#[error("aether {code}: {message}")]
pub struct ErrorReply {
    /// `incompatible`, `unauthorized`, `not_found`, `invalid_transition`,
    /// `invalid_argument`, `unavailable`, `internal`, `history_expired`.
    pub code: String,
    pub message: String,
    /// The expired span's bounds; set only with `history_expired`.
    #[serde(default)]
    pub first_seq: Option<u64>,
    #[serde(default)]
    pub last_seq: Option<u64>,
}

/// Event seqs `first..=last` that Aether's history retention deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiredSpan {
    pub first: u64,
    pub last: u64,
}

impl ExpiredSpan {
    /// `None` for bounds no store reports: seqs start at 1 and run forwards.
    pub fn new(first: u64, last: u64) -> Option<Self> {
        (first > 0 && first <= last).then_some(Self { first, last })
    }
}

impl std::fmt::Display for ExpiredSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..={}", self.first, self.last)
    }
}

impl ErrorReply {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            first_seq: None,
            last_seq: None,
        }
    }

    /// The span of a `history_expired` refusal, from its bounds or, should an
    /// aetherd leave them off, from its text.
    pub fn expired_span(&self) -> Option<ExpiredSpan> {
        if self.code != "history_expired" {
            return None;
        }
        match (self.first_seq, self.last_seq) {
            (Some(first), Some(last)) => ExpiredSpan::new(first, last),
            _ => span_in_text(&self.message),
        }
    }

    /// The span an aetherd without the `history_expired` feature names only
    /// in the text of a `not_found` refusal.
    pub fn expired_span_in_text(&self) -> Option<ExpiredSpan> {
        if self.code != "not_found" {
            return None;
        }
        span_in_text(&self.message)
    }
}

fn span_in_text(message: &str) -> Option<ExpiredSpan> {
    let (_, rest) = message.split_once(HISTORY_EXPIRED_TEXT)?;
    let range = rest.split([';', ' ']).next()?;
    let (first, last) = range.split_once("..=")?;
    ExpiredSpan::new(first.parse().ok()?, last.parse().ok()?)
}

/// The fields of Aether's `Agent` Archon reads; the rest are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Agent {
    pub id: AetherAgentId,
    #[serde(default)]
    pub scope: Option<AgentScope>,
    #[serde(default)]
    pub parent: Option<AetherAgentId>,
    pub role: String,
    pub name: String,
    pub model: ModelRef,
    pub status: serde_json::Value,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub current_task: Option<AetherTaskId>,
    /// The Aether run it belongs to; a child's is its parent's.
    #[serde(default)]
    pub run: Option<AetherRunId>,
    /// The agent `restart` created from this one (Aether's `restarted_as`).
    #[serde(default, rename = "restarted_as")]
    pub successor: Option<AetherAgentId>,
    #[serde(default)]
    pub config: AgentSettings,
    /// Whether a live engine drives the agent; absent from an Aether that
    /// does not report it.
    #[serde(default)]
    pub engine_alive: Option<bool>,
    /// When the agent last logged an event; absent or null when unknown.
    #[serde(default)]
    pub last_event_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct AgentScope {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// The fields of an agent's `config` Archon reads.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct AgentSettings {
    #[serde(default)]
    pub host_context: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: ToolExposure,
    #[serde(default)]
    pub leaf: bool,
    /// As Aether wrote it; it may name an effort Archon does not know.
    #[serde(default)]
    pub effort: Option<String>,
    /// Aether's hard cap in dollars; `None` when it has none.
    #[serde(default)]
    pub cost_budget: Option<f64>,
    /// When Aether stops it; `None` when it has no deadline.
    #[serde(default)]
    pub deadline: Option<Timestamp>,
}

impl AgentSettings {
    /// Whether the agent can create child agents with Aether's `delegate`.
    pub fn can_delegate(&self) -> bool {
        !self.leaf && self.tools.allows("delegate")
    }
}

/// The fields of Aether's `Task` Archon reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Task {
    pub id: AetherTaskId,
    #[serde(default)]
    pub owner: Option<AetherAgentId>,
    pub status: TaskStatus,
    pub run: AetherRunId,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Assigned,
    Active,
    Blocked {
        reason: String,
    },
    Completed {
        result: TaskResult,
    },
    Failed {
        error: String,
        #[serde(default)]
        structured: Option<serde_json::Value>,
    },
    Cancelled {
        #[serde(default)]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskResult {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub structured: Option<serde_json::Value>,
}

/// One row of `list_models`: Aether nests the provider-qualified model under
/// `model` and adds display and readiness fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelInfo {
    pub model: ModelRef,
    #[serde(default)]
    pub name: String,
    /// Whether credentials resolve for the provider right now.
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// Aether's capability tier; `None` when it places the model in none or
    /// names a tier this Archon does not know.
    #[serde(default, deserialize_with = "known_tier")]
    pub tier: Option<Tier>,
}

fn known_tier<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<Tier>, D::Error> {
    let tier = Option::<String>::deserialize(de)?;
    Ok(tier.and_then(|t| t.parse().ok()))
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ModelCapabilities {
    /// Pricing is not proof of engine cost reporting; consult ProviderInfo.
    #[serde(default)]
    pub cost: Option<serde_json::Value>,
}

/// The provider protocol, independent of the operator's provider name.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderInfo {
    pub provider: String,
    pub protocol: String,
    #[serde(default)]
    pub engine: Option<bool>,
    /// Quota left per usage window; `None` from an aetherd that predates it,
    /// which leaves routing to the weights alone.
    #[serde(default)]
    pub headroom: Option<archon_core::quota::ProviderHeadroom>,
}

/// The payload of a `subscribed` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Subscribed {
    pub id: u64,
    pub last_seq: u64,
}

impl Command {
    /// The wire tag (`"create_agent"`), for error messages about a request.
    pub fn name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| {
                v.get("cmd")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "command".to_owned())
    }
}

impl From<ErrorReply> for ArchonError {
    /// Keeps Aether's code where Archon has a matching variant, so callers
    /// can tell "no such agent" from "refused" without parsing text.
    /// `unavailable` (a provider or model not configured) is a refusal, so it
    /// maps to `Aether`: `ArchonError::Unavailable` always means no answer
    /// arrived, the one case where a keyed retry is right (ARCHITECTURE §6.3).
    /// `history_expired` stays `NotFound`, what an aetherd without it sends.
    fn from(reply: ErrorReply) -> Self {
        let ErrorReply { code, message, .. } = reply;
        match code.as_str() {
            "not_found" | "history_expired" => Self::NotFound(format!("aether: {message}")),
            "invalid_transition" => Self::InvalidTransition(format!("aether: {message}")),
            "invalid_argument" => Self::Invalid(format!("aether: {message}")),
            _ => Self::Aether(format!("{code}: {message}")),
        }
    }
}

/// Whether `error` is Aether's `internal` code, as [`ErrorReply`] maps it:
/// a failure inside Aether, not a verdict on the request.
pub fn is_internal(error: &ArchonError) -> bool {
    matches!(error, ArchonError::Aether(text) if text.starts_with("internal: "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expired_span_reads_from_its_bounds_or_its_text() {
        let text =
            "event history expired within sequence range 3..=9; obtain a fresh state snapshot";
        let coded = ErrorReply {
            first_seq: Some(2),
            last_seq: Some(5),
            ..ErrorReply::new("history_expired", text)
        };
        assert_eq!(coded.expired_span(), ExpiredSpan::new(2, 5), "bounds win");
        let unbounded = ErrorReply::new("history_expired", text);
        assert_eq!(unbounded.expired_span(), ExpiredSpan::new(3, 9));
        let older = ErrorReply::new("not_found", text);
        assert_eq!(older.expired_span(), None);
        assert_eq!(older.expired_span_in_text(), ExpiredSpan::new(3, 9));
        let ended = "event history expired within sequence range 7..=7";
        assert_eq!(
            ErrorReply::new("not_found", ended).expired_span_in_text(),
            ExpiredSpan::new(7, 7)
        );
        for garbled in [
            "no such agent",
            "event history expired within sequence range 9..=3; later first",
            "event history expired within sequence range 0..=3; seq 0",
            "event history expired within sequence range 3..9; no =",
            "event history expired within sequence range x..=9",
        ] {
            let reply = ErrorReply::new("not_found", garbled);
            assert_eq!(reply.expired_span_in_text(), None, "{garbled}");
        }
        assert!(matches!(ArchonError::from(coded), ArchonError::NotFound(_)));
    }

    #[test]
    fn agent_scope_reads_labels_without_requiring_unrelated_fields() {
        let mut value = crate::fake::agent("agt_scope", None);
        value["scope"] = serde_json::json!({"labels": {"archon.run": "prun_1"}});
        let agent: Agent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(agent.scope.unwrap().labels["archon.run"], "prun_1");
        value["scope"]["labels"] = serde_json::json!(["bad"]);
        assert!(serde_json::from_value::<Agent>(value).is_err());
    }

    #[test]
    fn provider_headroom_is_optional_and_read_as_aetherd_sends_it() {
        let old = serde_json::json!({"provider": "codex", "protocol": "codex", "engine": true});
        let old: ProviderInfo = serde_json::from_value(old).unwrap();
        assert_eq!(old.headroom, None, "an older aetherd reports no usage");
        let new = serde_json::json!({"provider": "codex", "protocol": "codex", "headroom": {
            "windows": [{"name":"primary", "window_minutes":300, "remaining_percent":62,
                "resets_at":"2030-01-02T15:30:00Z", "resets_in_seconds":900}],
            "observed_at":"2030-01-02T15:14:30Z", "age_seconds":30, "stale":false}});
        let new: ProviderInfo = serde_json::from_value(new).unwrap();
        let headroom = new.headroom.unwrap();
        assert!(!headroom.stale);
        assert_eq!(headroom.windows[0].window_minutes, Some(300));
        assert_eq!(headroom.windows[0].resets_in_seconds, Some(900));
    }

    #[test]
    fn a_model_s_tier_reads_leniently() {
        let tier = |tier: serde_json::Value| {
            let row = serde_json::json!({"model": "p/m", "tier": tier});
            serde_json::from_value::<ModelInfo>(row).unwrap().tier
        };
        assert_eq!(tier("strong".into()), Some(Tier::Strong));
        assert_eq!(tier("strongest".into()), Some(Tier::Strong));
        assert_eq!(tier("mythic".into()), None, "an unknown tier is untiered");
        assert_eq!(tier(serde_json::Value::Null), None);
        let untiered = serde_json::json!({"model": "p/m"});
        assert_eq!(
            serde_json::from_value::<ModelInfo>(untiered).unwrap().tier,
            None
        );
    }

    #[test]
    fn an_internal_error_keeps_its_code_and_others_do_not_pass_for_it() {
        let reply = |code: &str| ErrorReply::new(code, "sqlite: database or disk is full");
        let internal = ArchonError::from(reply("internal"));
        assert!(is_internal(&internal), "{internal}");
        assert!(!is_internal(&ArchonError::from(reply("unavailable"))));
        assert!(!is_internal(&ArchonError::from(reply("not_found"))));
        assert!(!is_internal(&ArchonError::Protocol("internal: x".into())));
    }

    #[test]
    fn create_agent_serializes_like_aether_expects() {
        let command = Command::CreateAgent(Box::new(CreateAgent {
            role: "archon-orchestrator".into(),
            model: Some("claude-code/claude-opus-5-5".parse().unwrap()),
            config: Some(AgentConfig {
                effort: Some(Effort::High),
                tools: ToolExposure::Except {
                    names: ["delegate".to_owned()].into(),
                },
                long_lived: true,
                ..AgentConfig::default()
            }),
            start: true,
            ..CreateAgent::default()
        }));
        let frame = serde_json::to_value(ClientFrame {
            req: 3,
            command: &command,
        })
        .unwrap();
        assert_eq!(frame["req"], 3);
        assert_eq!(frame["cmd"], "create_agent");
        assert_eq!(frame["model"]["provider"], "claude-code");
        assert_eq!(frame["config"]["tools"]["mode"], "except");
        assert_eq!(frame["config"]["effort"], "high");
        assert!(frame.get("parent").is_none());
    }

    #[test]
    fn a_default_create_agent_starts_the_agent_like_aether() {
        let minimal = Command::CreateAgent(Box::new(CreateAgent {
            role: "archon-orchestrator".into(),
            ..CreateAgent::default()
        }));
        let frame = serde_json::to_value(&minimal).unwrap();
        assert_eq!(frame["start"], true);
        assert!(frame.get("idempotency_key").is_none());
    }

    #[test]
    fn replies_decode_in_two_steps() {
        let line = r#"{"frame":"reply","id":4,"result":{"Ok":{"reply":"task","data":
            {"id":"tsk_1","run":"run_1","owner":"agt_1","status":{"state":"completed",
            "result":{"summary":"done VERDICT: PASS","artifacts":[]}},"objective":"x"}}}}"#;
        let ServerFrame::Reply { id, result } = serde_json::from_str(line).unwrap() else {
            panic!("expected a reply frame");
        };
        assert_eq!(id, 4);
        let ReplyResult::Ok(raw) = result else {
            panic!("expected Ok");
        };
        assert_eq!(raw.reply, "task");
        let task: Task = serde_json::from_value(raw.data).unwrap();
        assert!(matches!(task.status, TaskStatus::Completed { .. }));

        let err =
            r#"{"frame":"reply","id":0,"result":{"Err":{"code":"incompatible","message":"v9"}}}"#;
        let frame: ServerFrame = serde_json::from_str(err).unwrap();
        assert!(matches!(
            frame,
            ServerFrame::Reply {
                id: 0,
                result: ReplyResult::Err(_)
            }
        ));
    }
}
