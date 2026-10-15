//! Operator feedback on one stage of one finished run (`archon learning`):
//! the model choice it may penalize, frozen when it is recorded, and the
//! Aether memory it may ask to forget. Archon keeps ids and state, never text.

mod memory;

pub use memory::*;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::AetherMemoryId;
use crate::error::ArchonError;
use crate::id::{FeedbackId, PipelineRunId, QueueItemId, RepoId, StageRunId};
use crate::model::ModelChoice;
use crate::outcome::{CorrectionCause, RouteContext};

/// Longest correction reason, in bytes.
pub const MAX_REASON_BYTES: usize = 1000;
/// Feedback events kept per run, the convention of the legacy feedback list.
pub const MAX_FEEDBACK_PER_RUN: usize = 100;
/// How long a quality penalty lasts, and how far back a cohort with typed
/// feedback reads its routing samples.
pub const FEEDBACK_WINDOW_DAYS: i64 = 90;

pub fn feedback_window() -> chrono::Duration {
    chrono::Duration::days(FEEDBACK_WINDOW_DAYS)
}

/// `archon learning correct`: what the operator says went wrong, where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionRequest {
    pub item: QueueItemId,
    pub run: PipelineRunId,
    pub stage: String,
    pub cause: CorrectionCause,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forget_memory: Option<AetherMemoryId>,
}

impl CorrectionRequest {
    pub fn check(&self) -> Result<(), ArchonError> {
        let stage = self.stage.as_str();
        if stage.is_empty()
            || stage.len() > 100
            || stage.trim() != stage
            || stage.chars().any(char::is_control)
        {
            return Err(ArchonError::Invalid(format!(
                "{stage:?} is not a stage id: give the id of one stage of the run"
            )));
        }
        if self.reason().is_empty() || self.reason.len() > MAX_REASON_BYTES {
            return Err(ArchonError::Invalid(format!(
                "a correction needs a reason of 1 to {MAX_REASON_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The reason as recorded: without surrounding whitespace.
    pub fn reason(&self) -> &str {
        self.reason.trim()
    }

    /// Equal for the same request sent again, so a replay records nothing new.
    pub fn request_key(&self) -> String {
        assert!(
            !self.stage.contains('\u{1f}'),
            "checked stage ids hold no separator"
        );
        let memory = self
            .forget_memory
            .as_ref()
            .map_or("", AetherMemoryId::as_str);
        let key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{memory}\u{1f}{}",
            self.run,
            self.stage,
            self.cause.as_str(),
            self.reason()
        );
        assert!(
            key.starts_with(self.run.as_str()),
            "a key leads with its run"
        );
        key
    }
}

/// What the feedback froze about the run when it was recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    pub repo: Option<RepoId>,
    pub pipeline: String,
    pub context: Option<RouteContext>,
    /// The commit the run shipped, when it shipped one.
    pub delivery_commit: Option<String>,
    pub credit: Credit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "credit", rename_all = "snake_case")]
pub enum Credit {
    /// The one completed model choice that produced the stage.
    Model {
        stage_run: StageRunId,
        choice: ModelChoice,
    },
    /// No model is credited: the producing choice is ambiguous or unknown.
    Unavailable { why: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurationState {
    /// Recorded; Aether has not confirmed the forget.
    Pending,
    Applied,
    Failed,
    /// Restored through `archon learning restore-memory`.
    Restored,
}

impl CurationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Restored => "restored",
        }
    }
}

/// The memory's state when Archon first read it, before forgetting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBefore {
    Current,
    Forgotten,
    Superseded,
}

/// The forget a correction asked for, and Aether's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCuration {
    pub memory: AetherMemoryId,
    /// The Aether instance the forget is bound to when it is recorded, before
    /// Aether is called; it never changes.
    pub instance_id: String,
    pub state: CurationState,
    pub before: Option<MemoryBefore>,
    /// Aether's `forgotten_at` from the forget's reply.
    pub forgotten_at: Option<Timestamp>,
    pub restored_at: Option<Timestamp>,
    /// Why the last attempt did not complete.
    pub error: Option<String>,
    pub updated_at: Timestamp,
}

impl MemoryCuration {
    pub fn check(&self) -> Result<(), ArchonError> {
        let consistent = match self.state {
            CurationState::Pending => self.error.is_none() && self.forgotten_at.is_none(),
            CurationState::Failed => self.error.is_some() && self.forgotten_at.is_none(),
            CurationState::Applied => self.forgotten_at.is_some() && self.restored_at.is_none(),
            CurationState::Restored => self.forgotten_at.is_some() && self.restored_at.is_some(),
        };
        if !consistent || self.instance_id.is_empty() {
            return Err(ArchonError::Invalid(format!(
                "memory curation of {} is inconsistent in state {}",
                self.memory,
                self.state.as_str()
            )));
        }
        Ok(())
    }
}

/// One recorded correction, with mutable revocation, curation and delivery state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorFeedback {
    pub id: FeedbackId,
    pub request_key: String,
    pub item: QueueItemId,
    pub run: PipelineRunId,
    pub stage: String,
    pub cause: CorrectionCause,
    pub reason: String,
    pub attribution: Attribution,
    pub observed_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
    pub curation: Option<MemoryCuration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_feedback: Option<MemoryFeedback>,
}

impl OperatorFeedback {
    /// The stage run and choice a quality correction penalizes, if any.
    pub fn penalized(&self) -> Option<(&StageRunId, &ModelChoice)> {
        match (&self.cause, &self.attribution.credit) {
            (CorrectionCause::Quality, Credit::Model { stage_run, choice }) => {
                Some((stage_run, choice))
            }
            _ => None,
        }
    }

    /// Whether it penalizes a sample `as_of`: unrevoked and at most 90 days old.
    pub fn penalizing(&self, as_of: Timestamp) -> bool {
        let live = self.revoked_at.is_none_or(|at| at > as_of);
        let fresh = self.observed_at <= as_of && as_of - self.observed_at < feedback_window();
        self.penalized().is_some() && live && fresh
    }

    pub fn check(&self) -> Result<(), ArchonError> {
        if self.stage.is_empty()
            || self.reason.trim().is_empty()
            || self.reason.len() > MAX_REASON_BYTES
        {
            return Err(ArchonError::Invalid(format!(
                "feedback {} needs a stage and a reason of at most {MAX_REASON_BYTES} bytes",
                self.id
            )));
        }
        if self.revoked_at.is_some_and(|at| at < self.observed_at) {
            return Err(ArchonError::Invalid(format!(
                "feedback {} is revoked before it was recorded",
                self.id
            )));
        }
        if let Credit::Unavailable { why } = &self.attribution.credit
            && why.trim().is_empty()
        {
            return Err(ArchonError::Invalid(format!(
                "feedback {} credits no model without saying why",
                self.id
            )));
        }
        self.curation
            .as_ref()
            .map_or(Ok(()), MemoryCuration::check)?;
        self.memory_feedback
            .as_ref()
            .map_or(Ok(()), MemoryFeedback::check)
    }
}

/// An Aether memory as Archon shows it: where it came from, never its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryView {
    pub id: AetherMemoryId,
    /// Aether's authenticated provenance: `operator`, `agent agt_…`, a file.
    pub origin: String,
    /// Where it applies: `global`, `repository <id>`, `role <name>`, …
    pub scope: String,
    pub kind: String,
    pub created_at: Timestamp,
    pub forgotten_at: Option<Timestamp>,
    pub superseded_by: Option<String>,
    pub pinned: bool,
}

/// One choice's routing samples in the feedback's cohort, read now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceStanding {
    pub choice: ModelChoice,
    pub runs: u32,
    pub passed: u32,
}

/// A feedback event with what it does now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackView {
    pub feedback: OperatorFeedback,
    #[serde(default)]
    pub memory_feedback: MemoryFeedbackCounts,
    /// Whether it masks a model sample as of the reply.
    pub penalizing: bool,
    /// The penalized choice's samples in its repository and context.
    pub corrected: Option<ChoiceStanding>,
    /// What a new default in that cohort selects now; `None` keeps the static fallback.
    pub selected: Option<ChoiceStanding>,
    /// The routing explanation kept for the stage when the item's plan was chosen.
    pub frozen_explanation: Option<String>,
    pub memory: Option<MemoryView>,
    /// Why the memory's current state could not be read.
    pub memory_error: Option<String>,
}

/// The reply of `learning correct`, `undo`, `restore-memory` and `retry-memory`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackReceipt {
    pub view: FeedbackView,
    /// The same correction was already recorded; nothing new was.
    pub replayed: bool,
}

/// `archon learning why <item>`: every correction of the item's runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningWhy {
    pub item: QueueItemId,
    pub feedback: Vec<FeedbackView>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effort, ModelRef};

    fn feedback(cause: CorrectionCause, observed_at: Timestamp) -> OperatorFeedback {
        OperatorFeedback {
            id: FeedbackId::new(),
            request_key: "k".into(),
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            stage: "implement".into(),
            cause,
            reason: "ignored the identity rule".into(),
            attribution: Attribution {
                repo: Some(RepoId::new()),
                pipeline: "direct".into(),
                context: None,
                delivery_commit: None,
                credit: Credit::Model {
                    stage_run: StageRunId::new(),
                    choice: ModelChoice {
                        model: ModelRef::new("codex", "gpt-6-luna"),
                        effort: Effort::Low,
                    },
                },
            },
            observed_at,
            revoked_at: None,
            curation: None,
            memory_feedback: None,
        }
    }

    #[test]
    fn only_a_fresh_unrevoked_quality_correction_penalizes() {
        let at: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let day = chrono::Duration::days(1);
        let mut quality = feedback(CorrectionCause::Quality, at);
        assert!(quality.check().is_ok());
        assert!(quality.penalizing(at));
        assert!(quality.penalizing(at + day * 89));
        assert!(!quality.penalizing(at + day * 90));
        assert!(!quality.penalizing(at - day), "not before it was recorded");
        quality.revoked_at = Some(at + day);
        assert!(quality.penalizing(at));
        assert!(!quality.penalizing(at + day));
        assert!(!feedback(CorrectionCause::Scope, at).penalizing(at));
        let mut unattributed = feedback(CorrectionCause::Quality, at);
        unattributed.attribution.credit = Credit::Unavailable {
            why: "fan-out".into(),
        };
        assert!(!unattributed.penalizing(at));
    }

    #[test]
    fn a_request_is_checked_and_keyed_by_its_content() {
        let request = CorrectionRequest {
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            stage: "implement".into(),
            cause: CorrectionCause::Quality,
            reason: "  wrong file  ".into(),
            forget_memory: None,
        };
        assert!(request.check().is_ok());
        let mut same = request.clone();
        same.reason = "wrong file".into();
        assert_eq!(request.request_key(), same.request_key());
        same.cause = CorrectionCause::Scope;
        assert_ne!(request.request_key(), same.request_key());
        for (stage, reason) in [
            ("", "r"),
            (" implement", "r"),
            ("implement", " "),
            ("implement", &"x".repeat(1001)),
        ] {
            let bad = CorrectionRequest {
                stage: stage.into(),
                reason: reason.into(),
                ..request.clone()
            };
            assert!(bad.check().is_err(), "{stage:?}");
        }
    }
}
