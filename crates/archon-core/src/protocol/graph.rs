//! Protocol 24's task graph and run plans (CONTRACTS.md §1.3, owner AR-G).

use serde::{Deserialize, Serialize};

use super::WaitingReasonCode;
use super::changes::QueueStateKind;
use super::github_links::{GithubRef, PrRef};
use crate::Timestamp;
use crate::catalog::StageSummary;
use crate::id::{InitiativeId, PipelineRunId, QueueItemId, RepoId};
use crate::initiative::InitiativeState;
use crate::queue::Priority;

/// Nodes a `task_graph` returns unless asked otherwise.
pub const TASK_GRAPH_LIMIT: u32 = 500;
/// The most nodes one `task_graph` may ask for.
pub const MAX_TASK_GRAPH_LIMIT: u32 = 2000;

/// Which items a `task_graph` holds; an absent field does not filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<RepoId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<QueueStateKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipelines: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiative: Option<InitiativeId>,
    /// Only the connected component holding this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_of: Option<QueueItemId>,
    /// Terminal items are left out unless they changed at or after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_terminal_since: Option<Timestamp>,
}

/// One item in the graph: a subset of its summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskNode {
    pub id: QueueItemId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoId>,
    pub title: String,
    pub pipeline: String,
    pub priority: Priority,
    pub state: QueueStateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReasonCode>,
    pub spent_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiative: Option<InitiativeId>,
    pub tags: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<GithubRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrRef>,
    pub agents_running: u32,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskEdgeKind {
    DependsOn,
    InitiativeAfter,
    StackedOn,
}

/// `from` is the prerequisite and `to` the item that waits for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskEdge {
    pub from: QueueItemId,
    pub to: QueueItemId,
    pub kind: TaskEdgeKind,
    pub satisfied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GraphInitiative {
    pub id: InitiativeId,
    pub title: String,
    pub state: InitiativeState,
    pub atomic: bool,
}

/// Nodes ordered by `(created_at, id)` and edges by `(from, to, kind)`, so
/// the same store gives the same bytes. `as_of` is the last change seq
/// committed in the read transaction the rows came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskGraph {
    pub nodes: Vec<TaskNode>,
    pub edges: Vec<TaskEdge>,
    pub initiatives: Vec<GraphInitiative>,
    pub as_of: u64,
    pub truncated: bool,
}

/// A run's frozen pipeline with the steer edits it ran under applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunPlan {
    pub run: PipelineRunId,
    pub pipeline: String,
    pub revision: String,
    pub stages: Vec<StageSummary>,
    /// Whether a steer changed the plan the run was frozen with.
    pub edited: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty filter is an empty object, the shape every reader accepts.
    #[test]
    fn an_empty_filter_encodes_as_an_empty_object() {
        let wire = serde_json::to_value(TaskFilter::default()).unwrap();
        assert_eq!(wire, serde_json::json!({}));
        let back: TaskFilter = serde_json::from_value(wire).unwrap();
        assert_eq!(back, TaskFilter::default());
    }

    #[test]
    fn edge_kinds_are_snake_case() {
        let edge = TaskEdge {
            from: QueueItemId::new(),
            to: QueueItemId::new(),
            kind: TaskEdgeKind::InitiativeAfter,
            satisfied: false,
        };
        let wire = serde_json::to_value(&edge).unwrap();
        assert_eq!(wire["kind"], "initiative_after");
        assert_eq!(serde_json::from_value::<TaskEdge>(wire).unwrap(), edge);
    }
}
