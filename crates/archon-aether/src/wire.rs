//! Aether protocol v17 wire shapes, the subset Archon uses (ARCHITECTURE §4).
//! Checked against `aether-protocol` and `clients/protocol.schema.json`.
//! Replies are decoded in two steps (`RawReply`, then the expected payload)
//! so an unexpected reply kind is a named error, not a decode panic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelRef};
use serde::{Deserialize, Serialize};

use crate::github::WatcherConfig;

/// Archon asks for this first, and falls back to the newest protocol an
/// older aetherd serves, down to [`MIN_PROTOCOL_VERSION`].
pub const PROTOCOL_VERSION: u32 = 17;
pub const MIN_PROTOCOL_VERSION: u32 = 14;
/// The first protocol whose GitHub watcher stores `allowed_users`.
pub const ALLOWLIST_PROTOCOL_VERSION: u32 = 15;
/// The first protocol whose GitHub watcher stores `operator_users`.
pub const OPERATOR_LOGINS_PROTOCOL_VERSION: u32 = 17;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const CLIENT_NAME: &str = "archon";

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
    },
    SendMessage {
        to: AetherAgentId,
        content: String,
        subtree: bool,
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
    Cancel {
        agent: AetherAgentId,
        reason: Option<String>,
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
    /// `invalid_argument`, `unavailable`, `internal`.
    pub code: String,
    pub message: String,
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
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: ToolExposure,
    #[serde(default)]
    pub leaf: bool,
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
    fn from(reply: ErrorReply) -> Self {
        let ErrorReply { code, message } = reply;
        match code.as_str() {
            "not_found" => Self::NotFound(format!("aether: {message}")),
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
    fn agent_scope_reads_labels_without_requiring_unrelated_fields() {
        let mut value = crate::fake::agent("agt_scope", None);
        value["scope"] = serde_json::json!({"labels": {"archon.run": "prun_1"}});
        let agent: Agent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(agent.scope.unwrap().labels["archon.run"], "prun_1");
        value["scope"]["labels"] = serde_json::json!(["bad"]);
        assert!(serde_json::from_value::<Agent>(value).is_err());
    }

    #[test]
    fn an_internal_error_keeps_its_code_and_others_do_not_pass_for_it() {
        let reply = |code: &str| ErrorReply {
            code: code.into(),
            message: "sqlite: database or disk is full".into(),
        };
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
