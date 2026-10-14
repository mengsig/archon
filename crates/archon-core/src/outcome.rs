//! Outcomes: what one finished run cost and how it ended. Learned model
//! defaults and lesson confidence are computed from these.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QueueItemId, RepoId};
use crate::model::ModelChoice;

/// Changed lines up to which a diff is small, and up to which it is medium.
const SMALL_LINES: u64 = 50;
const MEDIUM_LINES: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSize {
    Small,
    Medium,
    Large,
}

impl DiffSize {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: u32,
    pub added: u32,
    pub deleted: u32,
}

impl DiffStat {
    pub fn size(self) -> DiffSize {
        match u64::from(self.added) + u64::from(self.deleted) {
            lines if lines <= SMALL_LINES => DiffSize::Small,
            lines if lines <= MEDIUM_LINES => DiffSize::Medium,
            _ => DiffSize::Large,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeResult {
    Succeeded,
    Failed,
    Cancelled,
}

impl OutcomeResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Why a failed run failed, as far as the daemon can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Budget,
    Timeout,
    /// Review rounds ran out without a passing verdict.
    Review,
    /// Tests or checks kept failing.
    Verification,
    Ship,
    /// The agent backend or the machine, not the work.
    Infrastructure,
    /// A stage said the task needs another pipeline (`ESCALATE:`).
    Escalated,
    /// A stage said only the operator can unblock it (`BLOCKED:`).
    Blocked,
    Other,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Timeout => "timeout",
            Self::Review => "review",
            Self::Verification => "verification",
            Self::Ship => "ship",
            Self::Infrastructure => "infrastructure",
            Self::Escalated => "escalated",
            Self::Blocked => "blocked",
            Self::Other => "other",
        }
    }
}

/// Someone had to step in after or during the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Correction {
    RetryAfterDone,
    GateRejected,
    Steered,
    PrClosed,
    ChangesRequested,
    OperatorContradiction,
}

impl Correction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetryAfterDone => "retry_after_done",
            Self::GateRejected => "gate_rejected",
            Self::Steered => "steered",
            Self::PrClosed => "pr_closed",
            Self::ChangesRequested => "changes_requested",
            Self::OperatorContradiction => "operator_contradiction",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionCause {
    Quality,
    Scope,
    Infrastructure,
    Budget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrectionEvent {
    pub item: QueueItemId,
    pub stage: String,
    pub cause: CorrectionCause,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteContext {
    pub revision: String,
    pub review: String,
    pub task_class: String,
}

/// One stage of the run under one model choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeStage {
    pub stage: String,
    pub choice: ModelChoice,
    pub cost_usd: f64,
    pub wall_secs: u64,
    pub attempts: u32,
    /// Completed attributable work; None for interrupted or infrastructure segments.
    pub passed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub run: PipelineRunId,
    pub item: QueueItemId,
    pub repo: Option<RepoId>,
    pub pipeline: String,
    pub diff: Option<DiffStat>,
    pub stages: Vec<OutcomeStage>,
    pub review_rounds: u32,
    pub result: OutcomeResult,
    pub failure: Option<FailureClass>,
    /// Appended as they happen, also after the run finished.
    #[serde(default)]
    pub corrections: Vec<Correction>,
    #[serde(default)]
    pub feedback: Vec<CorrectionEvent>,
    #[serde(default)]
    pub context: Option<RouteContext>,
    /// `None` until the pull request is merged or closed.
    pub merged: Option<bool>,
    pub cost_usd: f64,
    #[serde(default = "provisional")]
    pub cost_provisional: bool,
    #[serde(default = "provisional")]
    pub count_estimates: bool,
    pub wall_secs: u64,
    pub finished_at: Timestamp,
}

fn provisional() -> bool {
    true
}

impl Outcome {
    /// Refuses a record that would skew what is learned from it.
    pub fn check(&self) -> Result<(), ArchonError> {
        if self.failure.is_some() != (self.result == OutcomeResult::Failed) {
            return Err(ArchonError::Invalid(format!(
                "outcome of {}: exactly a failed run has a failure class",
                self.run
            )));
        }
        let costs = self
            .stages
            .iter()
            .map(|s| s.cost_usd)
            .chain([self.cost_usd]);
        if let Some(bad) = costs.into_iter().find(|usd| !usd.is_finite() || *usd < 0.0) {
            return Err(ArchonError::Invalid(format!(
                "outcome of {}: {bad} is not a cost",
                self.run
            )));
        }
        if self.pipeline.trim().is_empty() || self.stages.iter().any(|s| s.stage.is_empty()) {
            return Err(ArchonError::Invalid(format!(
                "outcome of {}: the pipeline and every stage are named",
                self.run
            )));
        }
        let mut choices = std::collections::HashSet::new();
        if self
            .stages
            .iter()
            .any(|s| s.attempts == 0 || !choices.insert((&s.stage, &s.choice)))
        {
            return Err(ArchonError::Invalid(
                "outcome stages need positive attempts and distinct stage/model choices".into(),
            ));
        }
        Ok(())
    }

    pub fn size(&self) -> Option<DiffSize> {
        self.diff.map(DiffStat::size)
    }

    /// A success nobody had to correct; what confirms the lessons it used.
    pub fn is_win(&self) -> bool {
        self.result == OutcomeResult::Succeeded && self.corrections.is_empty()
    }
}

/// A model choice the evidence makes the default for one kind of stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedRoute {
    pub repo: Option<RepoId>,
    pub pipeline: String,
    pub stage: String,
    pub size: Option<DiffSize>,
    pub choice: ModelChoice,
    pub runs: u32,
    pub passed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effort, ModelRef};

    fn outcome() -> Outcome {
        Outcome {
            run: PipelineRunId::new(),
            item: QueueItemId::new(),
            repo: Some(RepoId::new()),
            pipeline: "quick-pr".into(),
            diff: Some(DiffStat {
                files: 2,
                added: 30,
                deleted: 20,
            }),
            stages: vec![OutcomeStage {
                stage: "implement".into(),
                choice: ModelChoice {
                    model: ModelRef::new("anthropic", "claude-sonnet-5-5"),
                    effort: Effort::Medium,
                },
                cost_usd: 0.4,
                wall_secs: 90,
                attempts: 1,
                passed: None,
            }],
            review_rounds: 0,
            result: OutcomeResult::Succeeded,
            failure: None,
            corrections: Vec::new(),
            feedback: Vec::new(),
            context: None,
            merged: None,
            cost_usd: 0.4,
            cost_provisional: true,
            count_estimates: true,
            wall_secs: 95,
            finished_at: crate::now(),
        }
    }

    #[test]
    fn diff_sizes_have_fixed_edges() {
        let lines = |added, deleted| DiffStat {
            files: 1,
            added,
            deleted,
        };
        assert_eq!(lines(0, 0).size(), DiffSize::Small);
        assert_eq!(lines(30, 20).size(), DiffSize::Small);
        assert_eq!(lines(30, 21).size(), DiffSize::Medium);
        assert_eq!(lines(400, 0).size(), DiffSize::Medium);
        assert_eq!(lines(u32::MAX, u32::MAX).size(), DiffSize::Large);
    }

    #[test]
    fn an_uncorrected_success_is_a_win_and_round_trips() {
        let mut clean = outcome();
        clean.check().unwrap();
        assert!(clean.is_win());
        assert_eq!(clean.size(), Some(DiffSize::Small));
        let json = serde_json::to_value(&clean).unwrap();
        assert_eq!(json["result"], "succeeded");
        assert_eq!(serde_json::from_value::<Outcome>(json).unwrap(), clean);
        clean.corrections.push(Correction::ChangesRequested);
        assert!(!clean.is_win());
    }

    #[test]
    fn check_refuses_records_that_would_skew_learning() {
        let mut unexplained = outcome();
        unexplained.result = OutcomeResult::Failed;
        assert!(unexplained.check().is_err());
        unexplained.failure = Some(FailureClass::Review);
        unexplained.check().unwrap();
        assert!(!unexplained.is_win());

        let mut classed_success = outcome();
        classed_success.failure = Some(FailureClass::Other);
        assert!(classed_success.check().is_err());

        let mut negative = outcome();
        negative.stages[0].cost_usd = -0.1;
        assert!(negative.check().is_err());
        let mut unnamed = outcome();
        unnamed.stages[0].stage.clear();
        assert!(unnamed.check().is_err());
    }
}
