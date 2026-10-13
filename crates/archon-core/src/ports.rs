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
use crate::model::{ModelChoice, ModelRef};
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
    pub role: String,
    pub name: String,
    /// Rendered stage prompt; becomes the agent's task objective.
    pub objective: String,
    /// Stage contract (§8.6), passed as `config.instructions`.
    pub instructions: String,
    pub choice: ModelChoice,
    pub workspace: PathBuf,
    pub repository: Option<String>,
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
}

/// The passive Aether agent under which one pipeline run's stage agents
/// sit (ARCHITECTURE §4.5). It is created but never started, so it takes no
/// turns and costs nothing; it exists so Aether's tree mirrors Archon's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunNodeRequest {
    /// `<pipeline run id>:node:<generation>`; a new generation replaces a
    /// node Aether lost.
    pub idempotency_key: String,
    pub role: String,
    pub name: String,
    /// Inherited by children that state none; the first stage's model.
    pub model: ModelRef,
    /// The run's worktree: a child's scope must sit inside its parent's.
    pub workspace: PathBuf,
    pub repository: Option<String>,
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

#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Create and start a stage agent. Safe to retry with the same
    /// `idempotency_key`; implementations re-check the model policy.
    async fn spawn(&self, request: SpawnRequest) -> Result<SpawnedAgent, ArchonError>;

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

    /// Cancel an agent and its live descendants.
    async fn cancel(&self, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError>;

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
}

#[cfg(test)]
mod tests {
    use super::NetworkPolicy::{Allowed, Denied};

    #[test]
    fn the_narrower_network_denies_unless_both_allow() {
        assert_eq!(Allowed.narrower(Allowed), Allowed);
        assert_eq!(Allowed.narrower(Denied), Denied);
        assert_eq!(Denied.narrower(Allowed), Denied);
        assert_eq!(Denied.narrower(Denied), Denied);
    }
}
