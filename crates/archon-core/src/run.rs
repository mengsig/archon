//! Pipeline runs and their stage runs: the durable record of what Archon
//! asked Aether to do for one queue item and what came back.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use crate::catalog::StageKind;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QuestionId, QueueItemId, ReportId, StageRunId};
use crate::model::ModelChoice;

/// What a review/test stage concluded, as [`Verdict::extract`] reads it from
/// the stage agent's result; anything unreadable is `Unknown` and fails a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Unknown,
}

/// `structured` keys a result may give its verdict under, in the order read.
const VERDICT_KEYS: [&str; 3] = ["verdict", "result", "status"];

impl Verdict {
    /// `structured.verdict`, else a readable `structured.result` or `status`,
    /// else the summary's last `VERDICT:` line.
    pub fn extract(summary: &str, structured: Option<&serde_json::Value>) -> Self {
        structured
            .and_then(Self::from_structured)
            .or_else(|| Self::from_summary(summary))
            .unwrap_or(Self::Unknown)
    }

    /// The verdict `structured` states: its `verdict` key whenever it has
    /// one, `Unknown` unless it reads as a verdict, else a `result` or
    /// `status` that reads as one.
    pub fn from_structured(structured: &serde_json::Value) -> Option<Self> {
        if let Some(given) = structured.get(VERDICT_KEYS[0]) {
            let text = given.as_str().unwrap_or_default();
            return Some(Self::from_summary(text).unwrap_or_else(|| Self::from_word(text)));
        }
        let read = |text: &str| match Self::from_summary(text) {
            Some(verdict) => Some(verdict),
            None if one_word(text) => Some(Self::from_word(text)),
            None => None,
        };
        VERDICT_KEYS[1..]
            .iter()
            .filter_map(|key| non_empty(structured.get(key)?.as_str()?))
            .filter_map(read)
            .find(|verdict| *verdict != Self::Unknown)
    }

    /// What the last `VERDICT: <word>` line of `text` says, in any letter
    /// case and with light markdown (`**Verdict:** PASS`); `Unknown` when
    /// that line names no verdict, even if an earlier one did.
    pub fn from_summary(text: &str) -> Option<Self> {
        let (_, value) = last_verdict_line(text)?;
        if value.to_ascii_lowercase().contains("verdict") {
            // `VERDICT: PASS` or `VERDICT: FAIL`: a howto quoted, not a verdict.
            return Some(Self::Unknown);
        }
        Some(Self::from_word(value))
    }

    /// The text a result gives as its verdict, readable or not, to quote
    /// when it is refused: a structured field, else a `VERDICT:` line.
    pub fn stated(summary: &str, structured: Option<&serde_json::Value>) -> Option<String> {
        let field = |key| structured?.get(key);
        let quote = |given: &serde_json::Value| match given.as_str() {
            Some(text) => text.trim().to_owned(),
            None => given.to_string(),
        };
        if let Some(given) = field(VERDICT_KEYS[0]) {
            return Some(quote(given));
        }
        let other = VERDICT_KEYS[1..]
            .iter()
            .find_map(|key| non_empty(field(key)?.as_str()?));
        let line = || last_verdict_line(summary).map(|(line, _)| line.trim());
        other.or_else(line).map(str::to_owned)
    }

    /// The first word of `text`, markdown and letter case aside.
    fn from_word(text: &str) -> Self {
        let word = text
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .split(|c: char| !c.is_alphanumeric())
            .next()
            .unwrap_or_default();
        match word.to_ascii_lowercase().as_str() {
            "pass" | "passed" | "passes" | "ok" | "approve" | "approved" => Self::Pass,
            "fail" | "failed" | "fails" | "reject" | "rejected" => Self::Fail,
            _ => Self::Unknown,
        }
    }
}

/// The last line of `text` that reads `VERDICT: …`, and its value. Lines in
/// fenced code or block quotes are examples, never a verdict.
fn last_verdict_line(text: &str) -> Option<(&str, &str)> {
    let mut fenced = false;
    let mut last = None;
    for line in text.lines() {
        let start = line.trim_start();
        if start.starts_with("```") || start.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || start.starts_with('>') {
            continue;
        }
        if let Some(value) = strip_label(line, "verdict") {
            last = Some((line, value));
        }
    }
    debug_assert!(last.is_none_or(|(line, _)| text.contains(line)));
    last
}

fn non_empty(text: &str) -> Option<&str> {
    Some(text.trim()).filter(|text| !text.is_empty())
}

/// Whether `text` is one word once markdown and punctuation around it are dropped.
fn one_word(text: &str) -> bool {
    let word = text.trim_matches(|c: char| !c.is_alphanumeric());
    !word.is_empty() && word.chars().all(char::is_alphanumeric)
}

/// `line` without a leading `label:`, in any letter case, with light
/// markdown (`**Label:**`, `- label:`, `## Label:`) around the label allowed;
/// a block quote (`> label:`) is no label.
pub fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    assert!(!label.is_empty(), "a label to strip");
    let line = line
        .trim()
        .trim_start_matches(['*', '-', '_', '`', '#', ' ']);
    let (head, rest) = line.split_at_checked(label.len())?;
    if !head.eq_ignore_ascii_case(label) {
        return None;
    }
    let rest = rest.trim_start_matches(['*', '_']);
    rest.strip_prefix(':')
        .map(|value| value.trim_start_matches(['*', '_', ' ']))
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
    /// The stage's kind in the pipeline this attempt ran in; `None` on stage
    /// runs recorded before Archon kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StageKind>,
    /// 1-based; grows with each retry/fix loop iteration.
    pub attempt: u32,
    /// 0-based slot within a fan-out stage; 0 for single-agent stages.
    pub fan_index: u32,
    pub choice: ModelChoice,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_segments: Vec<ModelSegment>,
    #[serde(default)]
    pub infrastructure_failure: bool,
    #[serde(default)]
    pub host_context: crate::config::HostContext,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSegment {
    pub delivery: String,
    pub choice: ModelChoice,
    pub started_at: Timestamp,
    pub cost_before_usd: f64,
}

/// The key under a result's `review` where Archon, and only Archon, writes
/// why it refused the review pass the result claims.
pub const REVIEW_REFUSED_KEY: &str = "unverified";

impl StageRun {
    pub fn current_choice(&self) -> &ModelChoice {
        self.model_segments
            .last()
            .map_or(&self.choice, |s| &s.choice)
    }

    /// Whether Archon refused the review pass this stage's result claims.
    pub fn review_refused(&self) -> bool {
        let review = self.structured.as_ref().and_then(|s| s.get("review"));
        let why = review.and_then(|review| review.get(REVIEW_REFUSED_KEY));
        why.and_then(serde_json::Value::as_str)
            .is_some_and(|why| !why.trim().is_empty())
    }
}

/// The newest `pr_url` a completed stage reported (the ship stage's), from
/// stage runs in the order they started.
pub fn pr_url<'a>(stages: impl DoubleEndedIterator<Item = &'a StageRun>) -> Option<&'a str> {
    stages
        .rev()
        .filter(|s| s.state == StageState::Completed)
        .find_map(|s| s.structured.as_ref()?.get("pr_url")?.as_str())
}

/// For stage runs given as `(stage, attempt, slot)` in the order they started,
/// how many earlier runs each slot had: 0 for its first run, `n` for its `n`th
/// retry (a refused swarm plan's correction, an infrastructure retry).
pub fn slot_retries<'a>(slots: impl IntoIterator<Item = (&'a str, u32, u32)>) -> Vec<u32> {
    let mut seen: std::collections::BTreeMap<(&str, u32, u32), u32> = Default::default();
    let retries: Vec<u32> = slots
        .into_iter()
        .map(|slot| {
            let runs = seen.entry(slot).or_insert(0);
            *runs += 1;
            *runs - 1
        })
        .collect();
    let total: u32 = seen.values().sum();
    assert_eq!(
        total as usize,
        retries.len(),
        "every stage run is counted once"
    );
    retries
}

/// What follows `attempt N slot M` in a stage run's line: its retry, if any.
pub fn retry_label(retry: u32) -> String {
    match retry {
        0 => String::new(),
        n => format!(" retry {n}"),
    }
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
    use serde_json::json;

    /// A refused swarm plan's correction reruns the same slot; each rerun is
    /// numbered so `attempt 1 slot 0` is not shown twice alike.
    #[test]
    fn each_rerun_of_a_slot_is_numbered_in_start_order() {
        let slots = [
            ("plan", 1, 0),
            ("plan", 1, 0),
            ("build", 1, 0),
            ("build", 1, 1),
            ("plan", 1, 0),
            ("plan", 2, 0),
        ];
        assert_eq!(slot_retries(slots), [0, 1, 0, 0, 2, 0]);
        assert_eq!(retry_label(0), "");
        assert_eq!(retry_label(2), " retry 2");
    }

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
    fn verdict_reads_markdown_any_case_synonyms_and_other_structured_keys() {
        let findings_only = serde_json::json!({"findings": []});
        let passed = [
            ("**Verdict:** PASS — all 214 tests pass", None),
            ("tests ran\n\n**VERDICT: PASS**", None),
            ("Verdict: passed\nhead: abc1234", None),
            ("## verdict: Approved", None),
            (
                "**Verdict:** PASS — all 214 tests pass",
                Some(&findings_only),
            ),
        ];
        for (summary, structured) in passed {
            assert_eq!(
                Verdict::extract(summary, structured),
                Verdict::Pass,
                "{summary}"
            );
        }
        let in_result = serde_json::json!({"result": "**Passed**"});
        assert_eq!(Verdict::extract("", Some(&in_result)), Verdict::Pass);
        let in_status = serde_json::json!({"result": "done", "status": "FAILED"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&in_status)),
            Verdict::Fail
        );
        let rejected = serde_json::json!({"verdict": "Rejected: two findings"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&rejected)),
            Verdict::Fail
        );
    }

    #[test]
    fn verdict_stays_unknown_without_a_readable_word_and_quotes_what_was_said() {
        let unclear = serde_json::json!({"verdict": "lgtm"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&unclear)),
            Verdict::Unknown
        );
        assert_eq!(Verdict::stated("", Some(&unclear)).as_deref(), Some("lgtm"));
        for malformed in [
            json!({"verdict": ""}),
            json!({"verdict": null}),
            json!({"verdict": true}),
        ] {
            let read = Verdict::extract("VERDICT: PASS", Some(&malformed));
            assert_eq!(read, Verdict::Unknown, "{malformed}");
        }
        let blank = json!({"verdict": " ", "status": "pass"});
        assert_eq!(Verdict::extract("", Some(&blank)), Verdict::Unknown);
        assert_eq!(
            Verdict::stated("", Some(&json!({"verdict": null}))).as_deref(),
            Some("null")
        );
        let prose = serde_json::json!({"result": "passing most tests, two fail"});
        assert_eq!(Verdict::extract("", Some(&prose)), Verdict::Unknown);
        assert_eq!(
            Verdict::extract("Not passed: VERDICT: PASS", None),
            Verdict::Unknown
        );
        assert_eq!(
            Verdict::extract("VERDICT: not a pass", None),
            Verdict::Unknown
        );
        assert_eq!(
            Verdict::stated("work done\nVERDICT: maybe\n", None).as_deref(),
            Some("VERDICT: maybe")
        );
        assert_eq!(Verdict::stated("no verdict at all", None), None);
    }

    #[test]
    fn only_the_last_verdict_line_outside_quotes_and_code_counts() {
        let quoted = "Example:\n> VERDICT: PASS\n\nVERDICT: not passed";
        assert_eq!(Verdict::extract(quoted, None), Verdict::Unknown);
        let fenced = "```\nVERDICT: PASS\n```\nThree findings remain.";
        assert_eq!(Verdict::extract(fenced, None), Verdict::Unknown);
        let howto = "VERDICT: FAIL\n- `VERDICT: PASS` or `VERDICT: FAIL`";
        assert_eq!(Verdict::extract(howto, None), Verdict::Unknown);
        let earlier = "VERDICT: PASS\nthen a regression\nVERDICT: FAIL";
        assert_eq!(Verdict::extract(earlier, None), Verdict::Fail);
        let after_quote = "> VERDICT: FAIL\nVERDICT: PASS";
        assert_eq!(Verdict::extract(after_quote, None), Verdict::Pass);
    }

    #[test]
    fn blockers_and_escalations_end_a_run_and_other_failures_do_not() {
        assert!(StageOutcome::ends_run("  BLOCKED: no push access"));
        assert!(StageOutcome::ends_run(
            "ESCALATE: needs a review; use adaptive"
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
            kind: Some(StageKind::Ship),
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            choice: ModelChoice {
                model: "claude-code/opus".parse().unwrap(),
                effort: crate::model::Effort::Low,
                tier: None,
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
    fn a_review_is_refused_only_with_archons_reason_on_record() {
        let review = |review: serde_json::Value| Some(serde_json::json!({"review": review}));
        let refused = review(serde_json::json!({"verdict": "pass", REVIEW_REFUSED_KEY: "why"}));
        assert!(stage(StageState::Completed, refused).review_refused());
        let checked = review(serde_json::json!({"verdict": "pass", "reviewer_agents": ["agt_r"]}));
        assert!(!stage(StageState::Completed, checked).review_refused());
        let blank = review(serde_json::json!({REVIEW_REFUSED_KEY: "  "}));
        assert!(!stage(StageState::Completed, blank).review_refused());
        assert!(!stage(StageState::Completed, None).review_refused());
    }

    #[test]
    fn a_run_finishes_once() {
        let mut run = PipelineRun {
            id: PipelineRunId::new(),
            item: QueueItemId::new(),
            pipeline: "direct".into(),
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
