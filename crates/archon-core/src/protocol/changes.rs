//! Protocol 24's change stream and item history (CONTRACTS.md §1.3, owner
//! AR-C): what `subscribe_changes`, `changes_page` and `item_history` carry.

use serde::{Deserialize, Serialize};

use super::WaitingReasonCode;
use crate::Timestamp;
use crate::id::{InitiativeId, PipelineRunId, QuestionId, QueueItemId, StageRunId};
use crate::initiative::InitiativeState;
use crate::protocol::github_links::LinkRelation;
use crate::queue::QueueState;
use crate::run::{GateDecision, RunState, StageState, Verdict};

/// The most changes or transitions one page may ask for.
pub const MAX_CHANGES_PAGE: u32 = 1000;

/// A queue item's state without its data: the names `QueueState::kind`
/// gives and `list_items` filters accept.
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
pub enum QueueStateKind {
    Queued,
    Dispatching,
    Running,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<&QueueState> for QueueStateKind {
    fn from(state: &QueueState) -> Self {
        match state {
            QueueState::Queued => Self::Queued,
            QueueState::Dispatching => Self::Dispatching,
            QueueState::Running { .. } => Self::Running,
            QueueState::Blocked { .. } => Self::Blocked,
            QueueState::Succeeded { .. } => Self::Succeeded,
            QueueState::Failed { .. } => Self::Failed,
            QueueState::Cancelled { .. } => Self::Cancelled,
        }
    }
}

/// A pipeline run's state without its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStateKind {
    Running,
    AwaitingOperator,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<&RunState> for RunStateKind {
    fn from(state: &RunState) -> Self {
        match state {
            RunState::Running { .. } => Self::Running,
            RunState::AwaitingOperator { .. } => Self::AwaitingOperator,
            RunState::Succeeded { .. } => Self::Succeeded,
            RunState::Failed { .. } => Self::Failed,
            RunState::Cancelled { .. } => Self::Cancelled,
        }
    }
}

/// A stage run's state without its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StageStateKind {
    Spawning,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl From<&StageState> for StageStateKind {
    fn from(state: &StageState) -> Self {
        match state {
            StageState::Spawning => Self::Spawning,
            StageState::Running => Self::Running,
            StageState::Completed => Self::Completed,
            StageState::Failed { .. } => Self::Failed,
            StageState::Cancelled => Self::Cancelled,
            StageState::TimedOut => Self::TimedOut,
        }
    }
}

/// One committed state change. `seq` is gapless and written in the same
/// transaction as the state it describes; the subject's fields sit beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Change {
    pub seq: u64,
    pub at: Timestamp,
    #[serde(flatten)]
    pub subject: ChangeSubject,
}

/// What changed; a client updates that entity and refetches it if shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "subject", rename_all = "snake_case")]
pub enum ChangeSubject {
    Item {
        item: QueueItemId,
        state: QueueStateKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        waiting_reason: Option<WaitingReasonCode>,
    },
    Dependency {
        item: QueueItemId,
        depends_on: QueueItemId,
        /// `false` when the dependency was removed.
        added: bool,
    },
    Run {
        run: PipelineRunId,
        item: QueueItemId,
        state: RunStateKind,
    },
    Stage {
        stage_run: StageRunId,
        run: PipelineRunId,
        item: QueueItemId,
        stage_id: String,
        attempt: u32,
        fan_index: u32,
        state: StageStateKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verdict: Option<Verdict>,
    },
    Gate {
        run: PipelineRunId,
        item: QueueItemId,
        stage_id: String,
        /// `None` while the gate waits for the operator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<GateDecision>,
    },
    Question {
        question: QuestionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<QueueItemId>,
        open: bool,
    },
    Initiative {
        initiative: InitiativeId,
        state: InitiativeState,
    },
    Link {
        item: QueueItemId,
        host: String,
        /// `owner/name`.
        repo: String,
        number: u64,
        relation: LinkRelation,
    },
}

/// Who made an item transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransitionActor {
    Daemon,
    Cli,
    Web,
    Mcp,
    Github,
}

/// One state transition of an item, as `item_history` lists them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ItemTransition {
    /// The change that recorded it.
    pub seq: u64,
    pub at: Timestamp,
    /// `None` for the item's creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<QueueStateKind>,
    pub to: QueueStateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub by: TransitionActor,
}

/// The range of change seqs still kept, for `ErrorCode::HistoryExpired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistoryRange {
    pub first_seq: u64,
    pub last_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kinds spell states exactly as `QueueState::kind` names them, so a
    /// change and a `list_items` filter use one vocabulary.
    #[test]
    fn queue_state_kinds_are_the_names_queue_state_gives() {
        let run = PipelineRunId::new();
        let states = [
            QueueState::Queued,
            QueueState::Dispatching,
            QueueState::Running { run: run.clone() },
            QueueState::Blocked { reason: "x".into() },
            QueueState::Succeeded {
                run: run.clone(),
                summary: "x".into(),
            },
            QueueState::Failed {
                run: None,
                error: "x".into(),
            },
            QueueState::Cancelled { reason: None },
        ];
        assert_eq!(states.len(), QueueState::KINDS.len());
        for state in &states {
            let kind = serde_json::to_value(QueueStateKind::from(state)).unwrap();
            assert_eq!(kind, state.kind(), "{state:?}");
        }
    }

    /// The subject's tag and fields sit beside `seq` and `at`, one flat
    /// object, and optional fields stay off the wire when unset.
    #[test]
    fn a_change_is_one_flat_object_tagged_by_subject() {
        let item = QueueItemId::new();
        let change = Change {
            seq: 7,
            at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            subject: ChangeSubject::Item {
                item: item.clone(),
                state: QueueStateKind::Running,
                stage: None,
                waiting_reason: None,
            },
        };
        let wire = serde_json::to_value(&change).unwrap();
        assert_eq!(wire["seq"], 7);
        assert_eq!(wire["subject"], "item");
        assert_eq!(wire["item"], item.as_str());
        assert_eq!(wire["state"], "running");
        assert!(wire.get("stage").is_none(), "{wire}");
        let back: Change = serde_json::from_value(wire).unwrap();
        assert_eq!(back, change);
    }

    #[test]
    fn run_and_stage_kinds_are_snake_case() {
        let awaiting = RunState::AwaitingOperator {
            stage_id: "plan".into(),
            attempt: 1,
            subject: crate::run::GateSubject::Proposal,
        };
        let kind = serde_json::to_value(RunStateKind::from(&awaiting)).unwrap();
        assert_eq!(kind, "awaiting_operator");
        let kind = serde_json::to_value(StageStateKind::from(&StageState::TimedOut)).unwrap();
        assert_eq!(kind, "timed_out");
    }
}
