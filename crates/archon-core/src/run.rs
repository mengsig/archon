//! Pipeline runs and their stage runs: the durable record of what Archon
//! asked Aether to do for one queue item and what came back.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QuestionId, QueueItemId, ReportId, StageRunId};
use crate::model::ModelChoice;

/// What a review/test stage concluded. Parsed from the stage agent's
/// structured result (`{"verdict": "pass"}`) or a trailing `VERDICT: PASS`
/// line in its summary; anything else is `Unknown` and fails a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Unknown,
}

impl Verdict {
    /// Structured field first, then the summary's last `VERDICT:` line.
    pub fn extract(summary: &str, structured: Option<&serde_json::Value>) -> Self {
        if let Some(v) = structured
            .and_then(|s| s.get("verdict"))
            .and_then(|v| v.as_str())
        {
            return Self::from_word(v);
        }
        summary
            .lines()
            .rev()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("VERDICT:"))
            .map(|word| Self::from_word(word.trim()))
            .unwrap_or(Self::Unknown)
    }

    fn from_word(word: &str) -> Self {
        match word.trim().to_ascii_lowercase().as_str() {
            "pass" | "passed" | "ok" => Self::Pass,
            "fail" | "failed" => Self::Fail,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StageState {
    /// Agent creation requested but not yet acknowledged by Aether.
    Spawning,
    Running,
    Completed,
    Failed {
        error: String,
    },
    Cancelled,
    /// Wall-clock limit from the stage spec expired; Archon cancelled it.
    TimedOut,
}

/// How a stage agent's Aether task settled, as read from `get_task` or a
/// `task.*` event. Input to the pipeline engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StageOutcome {
    Completed {
        summary: String,
        structured: Option<serde_json::Value>,
    },
    Failed {
        error: String,
        /// Infrastructure failure (engine died, Aether restarted) rather than
        /// the agent giving up; eligible for `limits.infra_retries`.
        infra: bool,
    },
    Cancelled {
        reason: Option<String>,
    },
    TimedOut,
}

impl StageOutcome {
    /// The prefix a stage uses in `fail_task` to escalate to the operator.
    pub const BLOCKED_PREFIX: &'static str = "BLOCKED:";

    /// The prefix a stage uses in `fail_task` to hand its item back to the
    /// orchestrator: the work needs another shape than this run has.
    pub const ESCALATE_PREFIX: &'static str = "ESCALATE:";

    pub fn is_operator_blocker(&self) -> bool {
        matches!(self, Self::Failed { error, .. } if error.trim_start().starts_with(Self::BLOCKED_PREFIX))
    }

    /// Whether `error` ends its run at once: an operator blocker or an
    /// escalation, neither of which a fix loop or a retry can resolve.
    pub fn ends_run(error: &str) -> bool {
        let error = error.trim_start();
        error.starts_with(Self::BLOCKED_PREFIX) || error.starts_with(Self::ESCALATE_PREFIX)
    }
}

impl StageState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Spawning | Self::Running)
    }
}

/// One agent's worth of work: a stage, its attempt number, and for fan-out
/// stages which parallel slot it filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageRun {
    pub id: StageRunId,
    pub run: PipelineRunId,
    pub stage_id: String,
    /// 1-based; grows with each retry/fix loop iteration.
    pub attempt: u32,
    /// 0-based slot within a fan-out stage; 0 for single-agent stages.
    pub fan_index: u32,
    pub choice: ModelChoice,
    /// Aether identifiers, present once Aether acknowledged creation.
    pub aether_agent: Option<AetherAgentId>,
    pub aether_task: Option<AetherTaskId>,
    pub state: StageState,
    pub verdict: Option<Verdict>,
    pub summary: Option<String>,
    pub structured: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

/// The newest `pr_url` a completed stage reported (the ship stage's), from
/// stage runs in the order they started.
pub fn pr_url<'a>(stages: impl DoubleEndedIterator<Item = &'a StageRun>) -> Option<&'a str> {
    stages
        .rev()
        .filter(|s| s.state == StageState::Completed)
        .find_map(|s| s.structured.as_ref()?.get("pr_url")?.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Running {
        stage_id: String,
        attempt: u32,
    },
    /// An operator gate (ARCHITECTURE §18.3): the stage passed and the run
    /// waits for the operator to approve, revise or reject its output.
    AwaitingOperator {
        stage_id: String,
        attempt: u32,
        subject: GateSubject,
    },
    Succeeded {
        summary: String,
    },
    Failed {
        stage_id: String,
        error: String,
    },
    Cancelled {
        reason: Option<String>,
    },
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running { .. } | Self::AwaitingOperator { .. })
    }

    /// The stage a live run is at; `None` once it finished.
    pub fn live_stage(&self) -> Option<&str> {
        match self {
            Self::Running { stage_id, .. } | Self::AwaitingOperator { stage_id, .. } => {
                Some(stage_id)
            }
            _ => None,
        }
    }
}

/// What an operator gate asks the operator to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateSubject {
    /// The `propose` stage's direction, before implementing.
    Proposal,
    /// The change report, before shipping.
    Report,
}

impl GateSubject {
    pub fn noun(self) -> &'static str {
        match self {
            Self::Proposal => "direction",
            Self::Report => "change report",
        }
    }
}

/// The operator's answer to a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    Approve {
        note: Option<String>,
    },
    /// Loop back with `feedback` in the prompt.
    Revise {
        feedback: String,
    },
    /// Cancel the item.
    Reject {
        reason: String,
    },
}

impl GateDecision {
    /// Read an operator's free-text answer: `yes`/`approve` approve (text
    /// after the word is a note), `reject` or a bare `no` rejects, and
    /// anything else (`no, use X` included) is revision feedback.
    pub fn from_answer(text: &str) -> Result<Self, ArchonError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchonError::Invalid("a gate answer needs text".into()));
        }
        let (word, rest) = match text.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (text, ""),
        };
        let word = word
            .trim_end_matches(|c: char| c.is_ascii_punctuation())
            .to_ascii_lowercase();
        let note = (!rest.is_empty()).then(|| rest.to_owned());
        let decision = match word.as_str() {
            "yes" | "y" | "approve" | "approved" | "ok" | "lgtm" => Self::Approve { note },
            "no" | "n" if note.is_none() => Self::Reject {
                reason: "rejected by the operator".into(),
            },
            "reject" | "rejected" | "cancel" => Self::Reject {
                reason: note.unwrap_or_else(|| "rejected by the operator".into()),
            },
            _ => Self::Revise {
                feedback: text.to_owned(),
            },
        };
        Ok(decision)
    }

    pub fn word(&self) -> &'static str {
        match self {
            Self::Approve { .. } => "approved",
            Self::Revise { .. } => "revision requested",
            Self::Reject { .. } => "rejected",
        }
    }
}

/// One operator gate of a run, keyed by `(run, stage_id, attempt)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorGate {
    pub run: PipelineRunId,
    pub item: QueueItemId,
    pub stage_id: String,
    pub attempt: u32,
    pub subject: GateSubject,
    /// The gated stage's summary.
    pub summary: String,
    /// The HTML page the stage wrote: an absolute path in the run's workspace.
    pub page: Option<String>,
    /// Lavish URL or file path the operator was shown.
    pub location: Option<String>,
    pub report: Option<ReportId>,
    pub question: Option<QuestionId>,
    pub decision: Option<GateDecision>,
    /// Why the gate closed undecided: its run ended first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn: Option<String>,
    pub opened_at: Timestamp,
    /// When it was decided or withdrawn.
    pub decided_at: Option<Timestamp>,
}

impl OperatorGate {
    pub fn is_open(&self) -> bool {
        self.decision.is_none() && self.withdrawn.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    pub id: PipelineRunId,
    pub item: QueueItemId,
    pub pipeline: String,
    /// A ship-kind stage in the frozen pipeline completed before this run succeeded.
    /// Older runs have no proof of shipping and keep normal workspace retention.
    #[serde(default)]
    pub shipped: bool,
    /// Aether run id shared by every stage agent, once the first exists.
    pub aether_run: Option<AetherRunId>,
    /// Worktree the stages share; for a team pipeline, `<team_root>/main`.
    pub workspace: std::path::PathBuf,
    /// Directory holding the run worktree and its team workers' worktrees,
    /// set only for pipelines with a team stage; the run node's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_root: Option<std::path::PathBuf>,
    pub branch: String,
    pub state: RunState,
    pub cost_usd: f64,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

impl PipelineRun {
    pub fn finish(&mut self, state: RunState, at: Timestamp) -> Result<(), ArchonError> {
        if self.state.is_terminal() {
            return Err(ArchonError::InvalidTransition(format!(
                "run {} already finished",
                self.id
            )));
        }
        assert!(state.is_terminal(), "finish requires a terminal state");
        self.state = state;
        self.finished_at = Some(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_prefers_structured_then_summary_line() {
        let structured = serde_json::json!({"verdict": "fail"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&structured)),
            Verdict::Fail
        );
        assert_eq!(
            Verdict::extract("looks good\n\nVERDICT: PASS\n", None),
            Verdict::Pass
        );
        assert_eq!(Verdict::extract("no verdict here", None), Verdict::Unknown);
    }

    #[test]
    fn blockers_and_escalations_end_a_run_and_other_failures_do_not() {
        assert!(StageOutcome::ends_run("  BLOCKED: no push access"));
        assert!(StageOutcome::ends_run(
            "ESCALATE: needs a review; use default-pr-review"
        ));
        assert!(!StageOutcome::ends_run(
            "tests failed: ESCALATE: is not a prefix here"
        ));
        let escalated = StageOutcome::Failed {
            error: "ESCALATE: too large".into(),
            infra: false,
        };
        assert!(
            !escalated.is_operator_blocker(),
            "the operator is not asked"
        );
    }

    #[test]
    fn gate_answers_read_as_approve_reject_or_feedback() {
        assert_eq!(
            GateDecision::from_answer("yes").unwrap(),
            GateDecision::Approve { note: None }
        );
        assert_eq!(
            GateDecision::from_answer("Approve, but keep the old flag").unwrap(),
            GateDecision::Approve {
                note: Some("but keep the old flag".into())
            }
        );
        assert!(matches!(
            GateDecision::from_answer("no").unwrap(),
            GateDecision::Reject { .. }
        ));
        assert_eq!(
            GateDecision::from_answer("reject: too risky").unwrap(),
            GateDecision::Reject {
                reason: "too risky".into()
            }
        );
        assert!(matches!(
            GateDecision::from_answer("no, split it in two").unwrap(),
            GateDecision::Revise { .. }
        ));
        assert_eq!(
            GateDecision::from_answer("use a trait instead").unwrap(),
            GateDecision::Revise {
                feedback: "use a trait instead".into()
            }
        );
        assert!(GateDecision::from_answer("  ").is_err());
    }

    #[test]
    fn an_awaiting_run_is_live() {
        let waiting = RunState::AwaitingOperator {
            stage_id: "propose".into(),
            attempt: 1,
            subject: GateSubject::Proposal,
        };
        assert!(!waiting.is_terminal());
        assert_eq!(waiting.live_stage(), Some("propose"));
        let done = RunState::Succeeded {
            summary: "ok".into(),
        };
        assert_eq!(done.live_stage(), None);
    }

    fn stage(state: StageState, structured: Option<serde_json::Value>) -> StageRun {
        StageRun {
            id: StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: "ship".into(),
            attempt: 1,
            fan_index: 0,
            choice: ModelChoice {
                model: "claude-code/opus".parse().unwrap(),
                effort: crate::model::Effort::Low,
            },
            aether_agent: None,
            aether_task: None,
            state,
            verdict: None,
            summary: None,
            structured,
            cost_usd: 0.0,
            started_at: crate::now(),
            finished_at: None,
        }
    }

    #[test]
    fn the_pr_url_is_the_newest_a_completed_stage_reported() {
        let url = |n: u32| Some(serde_json::json!({"pr_url": format!("https://x/pull/{n}")}));
        let stages = [
            stage(StageState::Completed, url(1)),
            stage(StageState::Completed, url(2)),
            stage(StageState::Failed { error: "e".into() }, url(3)),
            stage(
                StageState::Completed,
                Some(serde_json::json!({"verdict": "pass"})),
            ),
        ];
        assert_eq!(pr_url(stages.iter()), Some("https://x/pull/2"));
        assert_eq!(
            pr_url(stages[2..].iter()),
            None,
            "only completed stages count"
        );
        assert_eq!(pr_url([].iter()), None);
    }

    #[test]
    fn a_run_finishes_once() {
        let mut run = PipelineRun {
            id: PipelineRunId::new(),
            item: QueueItemId::new(),
            pipeline: "quick-pr".into(),
            shipped: false,
            aether_run: None,
            workspace: "/archon/worktrees/x".into(),
            team_root: None,
            branch: "archon/x".into(),
            state: RunState::Running {
                stage_id: "implement".into(),
                attempt: 1,
            },
            cost_usd: 0.0,
            started_at: crate::now(),
            finished_at: None,
        };
        let mut legacy = serde_json::to_value(&run).unwrap();
        legacy.as_object_mut().unwrap().remove("shipped");
        let legacy: PipelineRun = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.shipped, "old records cannot prove shipping");
        run.finish(
            RunState::Succeeded {
                summary: "ok".into(),
            },
            crate::now(),
        )
        .unwrap();
        assert!(
            run.finish(RunState::Cancelled { reason: None }, crate::now())
                .is_err()
        );
    }
}
