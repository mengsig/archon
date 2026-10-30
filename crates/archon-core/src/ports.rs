//! The seam between the daemon and Aether. `archon-aether` implements
//! [`AgentBackend`]; the daemon and tests depend only on this trait, so the
//! dispatcher and engine executor can be built and tested against a fake.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use crate::error::ArchonError;
use crate::model::{Effort, ModelChoice, ModelRef};
use crate::run::StageOutcome;

/// Aether's scope network knob. An explicit scope is `denied` unless it
/// says otherwise, and a child may not widen its parent's, so Archon states
/// it on every agent and persists what it stated: a request's frame must
/// not change between retries under one idempotency key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Denied,
    Allowed,
}

impl NetworkPolicy {
    /// The more restrictive of the two: what a child may state when it
    /// must widen neither.
    pub fn narrower(self, other: Self) -> Self {
        match (self, other) {
            (Self::Allowed, Self::Allowed) => Self::Allowed,
            _ => Self::Denied,
        }
    }
}

/// Everything needed to start one stage agent (ARCHITECTURE §4.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// The stage run id: Aether dedupes a retried create under it.
    pub idempotency_key: String,
    /// Resolved before the first create; old requests keep full context.
    #[serde(default)]
    pub host_context: crate::config::HostContext,
    pub role: String,
    pub name: String,
    /// Rendered stage prompt; becomes the agent's task objective.
    pub objective: String,
    /// Stage contract (§8.6), passed as `config.instructions`.
    pub instructions: String,
    pub choice: ModelChoice,
    pub workspace: PathBuf,
    pub repository: Option<String>,
    /// Frozen before creation; absent on requests using the registered name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_root: Option<PathBuf>,
    pub labels: BTreeMap<String, String>,
    /// Tool names hidden from the agent: Archon's (`mcp__archon__*`) and
    /// Aether's own that no stage agent gets.
    pub hidden_tools: Vec<String>,
    /// `false` when the agent may delegate: by its stage's
    /// `allow_delegation` or the item's `tools = yolo`.
    pub leaf: bool,
    /// Archon's grant, retained even when Aether cannot enforce a hard cap.
    pub cost_budget_usd: Option<f64>,
    /// Whether to send the grant as Aether's hard cap. Resolved and persisted
    /// before create_agent so capability changes cannot alter a keyed retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aether_cost_budget: Option<bool>,
    pub deadline: Option<Timestamp>,
    /// Attach to this Aether run (all stages of one pipeline run share one).
    pub attach_run: Option<AetherRunId>,
    /// The pipeline run's node (§4.5), set when the spawn is issued rather
    /// than reserved, so a stored request predating the node still loads.
    #[serde(default)]
    pub parent: Option<AetherAgentId>,
    /// The network stated in the agent's scope, set with `parent`: the
    /// narrower of its node's and Aether's default at that moment. `None`
    /// on a record from before it was persisted; such a request is sent as
    /// that daemon sent it, with Aether's default.
    #[serde(default)]
    pub network: Option<NetworkPolicy>,
    /// The agent outlives its task, so a later attempt of its slot can
    /// resume it by message instead of starting cold (§6.7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
}

/// A resumed attempt's outcome from its final message (§6.7): failed after
/// `FAILED:`, `BLOCKED:` or `ESCALATE:`, else its summary and `structured`.
pub fn resumed_reply_outcome(reply: &str) -> StageOutcome {
    const FAILED_PREFIX: &str = "FAILED:";
    let reply = reply.trim();
    assert!(!reply.is_empty(), "a silent turn is not an answer");
    if let Some(why) = reply.strip_prefix(FAILED_PREFIX) {
        let error = match why.trim() {
            "" => "the stage agent reported that it could not fix its work".to_owned(),
            why => why.to_owned(),
        };
        return StageOutcome::Failed {
            error,
            infra: false,
        };
    }
    if StageOutcome::ends_run(reply) {
        return StageOutcome::Failed {
            error: reply.to_owned(),
            infra: false,
        };
    }
    let (summary, structured) = handed_in(reply);
    StageOutcome::Completed {
        summary,
        structured,
    }
}

/// The summary of `reply` and the object of its last fenced `json` block: a
/// resumed agent's task is settled, so its message carries `structured`.
fn handed_in(reply: &str) -> (String, Option<serde_json::Value>) {
    const OPEN: &str = "```json";
    assert_eq!(reply, reply.trim(), "the reply comes trimmed");
    for (at, _) in reply.rmatch_indices(OPEN) {
        // A stream reads one value and leaves the closing fence unread.
        let block = &reply[at + OPEN.len()..];
        let mut values = serde_json::Deserializer::from_str(block).into_iter();
        let Some(Ok(data @ serde_json::Value::Object(_))) = values.next() else {
            continue;
        };
        let said = reply[..at].trim();
        let summary = if said.is_empty() { reply } else { said };
        return (summary.to_owned(), Some(data));
    }
    (reply.to_owned(), None)
}

/// The passive Aether agent under which one pipeline run's stage agents
/// sit (ARCHITECTURE §4.5). It is created but never started, so it takes no
/// turns and costs nothing; it exists so Aether's tree mirrors Archon's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunNodeRequest {
    /// `<pipeline run id>:node:<generation>`; a new generation replaces a
    /// node Aether lost.
    pub idempotency_key: String,
    /// Resolved before the first create; old requests keep full context.
    #[serde(default)]
    pub host_context: crate::config::HostContext,
    pub role: String,
    pub name: String,
    /// Inherited by children that state none; the first stage's model.
    pub model: ModelRef,
    /// The run's worktree: a child's scope must sit inside its parent's.
    pub workspace: PathBuf,
    pub repository: Option<String>,
    /// Frozen before creation; absent on requests using the registered name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_root: Option<PathBuf>,
    /// Aether's default for new roots when the node was requested. A
    /// stage agent under the node may state it or narrower, never wider.
    pub network: NetworkPolicy,
    pub labels: BTreeMap<String, String>,
    /// Attach to the run's Aether run when one exists already.
    pub attach_run: Option<AetherRunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedAgent {
    pub agent: AetherAgentId,
    pub task: AetherTaskId,
    pub run: AetherRunId,
}

/// A run node Aether created: its id, and the Aether run it and every
/// stage agent under it belong to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedNode {
    pub agent: AetherAgentId,
    pub run: AetherRunId,
}

/// Whether Aether still runs an agent beneath another, and how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescendantState {
    /// Not ended, or in a state Archon does not know: it may still act.
    Live,
    Completed,
    /// Failed, cancelled or imported: it ended without completing.
    Stopped,
}

/// An agent beneath another in Aether's tree, as Aether holds it now.
#[derive(Debug, Clone, PartialEq)]
pub struct Descendant {
    pub agent: AetherAgentId,
    /// The agent asked about for a direct child, a descendant otherwise.
    pub parent: AetherAgentId,
    pub model: ModelRef,
    /// `None` when its config states none Archon can read.
    pub effort: Option<Effort>,
    pub state: DescendantState,
    /// The task it was given, kept after it ends; `None` before it has one.
    pub task: Option<AetherTaskId>,
    /// When Aether stops it; `None` when it has no deadline.
    pub deadline: Option<Timestamp>,
    /// Aether's hard cap in dollars; `None` when it has none.
    pub cost_budget_usd: Option<f64>,
}

/// What Aether reports of an agent tree's pulse, for the stage watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreePulse {
    /// The lead agent's `engine_alive`, or `Some(true)` when any agent of
    /// the tree reports a live engine; `None` from an Aether that lacks it.
    pub engine_alive: Option<bool>,
    /// The latest `last_event_at` over the tree; `None` when none reports one.
    pub last_event_at: Option<Timestamp>,
    /// An agent of the tree waits for an approval or is `blocked` for any
    /// reason but inspection: silence is expected then.
    pub held: bool,
    /// Some hold is one Aether names a lasting cause for: an approval, a
    /// usage or quota limit, or credentials.
    pub held_for_cause: bool,
    /// An agent of the tree is `blocked` for inspection: a tool call it
    /// started has no known outcome, as after an Aether restart. Not `held`.
    pub inspection: bool,
    /// Aether holds an agent of the tree `running`, so it owes events.
    pub running: bool,
}

/// A change to a live agent; `None` leaves that part as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentUpdate {
    /// Model and effort together: the policy judges the pair that will run.
    pub choice: Option<ModelChoice>,
    /// Aether's hard cap in dollars, for an agent whose spawn sent one
    /// (`SpawnRequest::aether_cost_budget`).
    pub cost_budget_usd: Option<f64>,
    pub deadline: Option<Timestamp>,
}

impl AgentUpdate {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Create and start a stage agent. Safe to retry with the same
    /// `idempotency_key`; implementations re-check the model policy.
    async fn spawn(&self, request: SpawnRequest) -> Result<SpawnedAgent, ArchonError>;

    /// Resolve a new agent's context before storing its keyed request.
    async fn host_context(
        &self,
        _requested: crate::config::HostContext,
    ) -> Result<crate::config::HostContext, ArchonError> {
        Ok(crate::config::HostContext::Full)
    }

    /// Whether new runs can derive their memory scope from the registered checkout.
    async fn supports_repository_root(&self) -> Result<bool, ArchonError> {
        Ok(false)
    }

    /// Resolve budget enforcement before the request is persisted and sent.
    async fn prepare_spawn(
        &self,
        _request: &mut SpawnRequest,
        _count_estimated_cost: bool,
    ) -> Result<(), ArchonError> {
        Ok(())
    }

    /// Create, without starting, the node a pipeline run's stage agents are
    /// parented under. Safe to retry with the same `idempotency_key`.
    async fn create_run_node(&self, request: RunNodeRequest) -> Result<CreatedNode, ArchonError>;

    /// Aether's `[network] default` for new roots, as its `status` reports
    /// it now. A new run node states it; a newly bound stage agent states
    /// no wider.
    async fn network_default(&self) -> Result<NetworkPolicy, ArchonError>;

    /// Whether Aether still holds `agent` in a non-terminal state; an agent
    /// Aether no longer knows is `false`, not an error.
    async fn is_live(&self, agent: &AetherAgentId) -> Result<bool, ArchonError>;

    /// Revive the failed `agent` in place, with its transcript and session;
    /// `Err` when Aether refuses, as it does for an agent that did not fail.
    async fn wake(&self, agent: &AetherAgentId) -> Result<(), ArchonError>;

    /// Ask Aether to cancel an agent and its live descendants. Aether cancels
    /// cooperatively: `Ok` means accepted, not that any of them has ended.
    async fn cancel(&self, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError>;

    /// Change a live agent in place; it keeps its transcript. Implementations
    /// re-check the model policy. Aether decides when a new model takes over.
    async fn update_agent(
        &self,
        agent: &AetherAgentId,
        update: AgentUpdate,
    ) -> Result<(), ArchonError>;

    /// [`Self::update_agent`] with Aether's idempotency `key`, so a repeat
    /// applies once.
    async fn update_agent_keyed(
        &self,
        agent: &AetherAgentId,
        update: AgentUpdate,
        key: &str,
    ) -> Result<(), ArchonError>;

    /// Whether [`Self::update_subtree`] is served (Aether's `subtree_update`).
    async fn supports_subtree_update(&self) -> Result<bool, ArchonError> {
        Ok(false)
    }

    /// [`Self::update_agent`], in one call that also raises the deadline and
    /// budget of every live agent beneath `agent` whose own is lower.
    async fn update_subtree(
        &self,
        agent: &AetherAgentId,
        _update: AgentUpdate,
    ) -> Result<(), ArchonError> {
        Err(ArchonError::Unavailable(format!(
            "this backend cannot update the agents beneath {agent} in one call"
        )))
    }

    /// Stop the turn `agent` is in; it then waits for its next message.
    /// `false` when it was in no turn.
    async fn interrupt(&self, agent: &AetherAgentId) -> Result<bool, ArchonError>;

    /// Every agent beneath `agent` at any depth, ended ones included; none
    /// when Aether no longer knows `agent`.
    async fn descendants(&self, agent: &AetherAgentId) -> Result<Vec<Descendant>, ArchonError>;

    /// The effort `agent` was created with; `None` when its config states
    /// none Archon can read, or Aether no longer knows it.
    async fn agent_effort(&self, agent: &AetherAgentId) -> Result<Option<Effort>, ArchonError>;

    /// The pulse of `agent` and everything beneath it; `None` when Aether no
    /// longer knows `agent`. A backend that cannot tell reports an empty pulse.
    async fn pulse(&self, _agent: &AetherAgentId) -> Result<Option<TreePulse>, ArchonError> {
        Ok(Some(TreePulse::default()))
    }

    /// Resume every agent of `lead`'s tree that Aether blocked for
    /// inspection, each told `note` first; the agents resumed. `Err` when
    /// Aether refuses or does not answer.
    async fn clear_inspection(
        &self,
        _lead: &AetherAgentId,
        _note: &str,
        _idempotency_key: &str,
    ) -> Result<Vec<AetherAgentId>, ArchonError> {
        Err(ArchonError::Invalid(
            "this backend cannot resume an agent blocked for inspection".into(),
        ))
    }

    /// `None` while the task is still open; the settled outcome otherwise.
    async fn task_outcome(&self, task: &AetherTaskId) -> Result<Option<StageOutcome>, ArchonError>;

    /// Put `content` in `to`'s inbox. Safe to retry with the same
    /// `idempotency_key` and content: Aether delivers it once.
    async fn send_message(
        &self,
        to: &AetherAgentId,
        content: &str,
        idempotency_key: &str,
    ) -> Result<(), ArchonError>;

    /// How the turn ended in which a kept agent took `message` (§6.7): `None`
    /// while it works, an `infra` failure when it is gone or said nothing.
    async fn resumed_outcome(
        &self,
        agent: &AetherAgentId,
        message: &str,
    ) -> Result<Option<StageOutcome>, ArchonError>;
}

#[cfg(test)]
mod tests {
    use super::NetworkPolicy::{Allowed, Denied};

    #[test]
    fn a_resumed_reply_is_a_summary_unless_it_starts_with_a_failure_prefix() {
        use super::resumed_reply_outcome;
        use crate::run::StageOutcome;
        let done = resumed_reply_outcome("  Fixed the null check in src/a.rs; tests pass.\n");
        let summary = "Fixed the null check in src/a.rs; tests pass.".to_owned();
        let structured = None;
        assert_eq!(
            done,
            StageOutcome::Completed {
                summary,
                structured
            }
        );
        let failed = resumed_reply_outcome("FAILED: the API this needs does not exist");
        let error = "the API this needs does not exist".to_owned();
        assert_eq!(
            failed,
            StageOutcome::Failed {
                error,
                infra: false
            }
        );
        let blocked = resumed_reply_outcome("BLOCKED: needs a deploy key");
        assert!(blocked.is_operator_blocker(), "{blocked:?}");
        let escalated = resumed_reply_outcome("ESCALATE: this needs a schema migration first");
        assert!(
            matches!(&escalated, StageOutcome::Failed { error, .. } if StageOutcome::ends_run(error)),
            "{escalated:?}"
        );
        let bare = resumed_reply_outcome("FAILED:");
        assert!(matches!(bare, StageOutcome::Failed { ref error, .. } if !error.is_empty()));
    }

    /// A resumed agent of the stage whose result archond ships hands the
    /// ship record in with its message: its task took `structured` once.
    #[test]
    fn a_resumed_reply_hands_in_the_object_of_its_last_json_block() {
        use super::resumed_reply_outcome;
        use crate::run::StageOutcome;
        let body = "Shown as:\n```json\n{\"a\": 1}\n```";
        let record = serde_json::json!({"commit": "c0ffee", "pr": {"body": body}});
        let reply = format!("Fixed the null check.\n\n```json\n{record}\n```\nThat is all.");
        let StageOutcome::Completed {
            summary,
            structured,
        } = resumed_reply_outcome(&reply)
        else {
            panic!("a reply with a record completes");
        };
        assert_eq!(summary, "Fixed the null check.");
        assert_eq!(structured, Some(record), "fences inside a string are text");

        for plain in [
            "Fixed it.",
            "Fixed it.\n```json\nnot json\n```",
            "```json\n[1]\n```",
        ] {
            let StageOutcome::Completed {
                summary,
                structured,
            } = resumed_reply_outcome(plain)
            else {
                panic!("{plain} completes");
            };
            assert_eq!((summary.as_str(), structured), (plain, None));
        }
        let only = resumed_reply_outcome("```json\n{\"a\": 1}\n```");
        assert!(matches!(only, StageOutcome::Completed { ref summary, .. } if !summary.is_empty()));
    }

    #[test]
    fn the_narrower_network_denies_unless_both_allow() {
        assert_eq!(Allowed.narrower(Allowed), Allowed);
        assert_eq!(Allowed.narrower(Denied), Denied);
        assert_eq!(Denied.narrower(Allowed), Denied);
        assert_eq!(Denied.narrower(Denied), Denied);
    }
}
