//! The persistent backlog: items, their lifecycle, and the caps the
//! dispatcher honours.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::autonomy::Autonomy;
use crate::error::ArchonError;
use crate::github::GithubThread;
use crate::id::{InitiativeId, PipelineRunId, QueueItemId, RepoId};
use crate::model::{ModelPlan, ModelReasons};
use crate::pull_request::OntoPullRequest;

/// The longest brief an enqueue accepts, well under the protocol frame cap.
pub const MAX_BRIEF_BYTES: usize = 1024 * 1024;

/// Characters that reorder how the rest of a line displays (Trojan Source).
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Input checks shared by the CLI and daemon enqueue boundaries.
pub fn check_enqueue(
    title: &str,
    brief: &str,
    tags: &[String],
    budget: Option<f64>,
) -> Result<(), ArchonError> {
    check_title(title)?;
    if brief.trim().is_empty() {
        return Err(ArchonError::Invalid(
            "an item needs a nonempty brief".into(),
        ));
    }
    if brief.len() > MAX_BRIEF_BYTES {
        return Err(ArchonError::Invalid(format!(
            "the brief is {} bytes; a brief may be at most {MAX_BRIEF_BYTES} bytes",
            brief.len()
        )));
    }
    if brief.contains('\0') {
        return Err(ArchonError::Invalid("a brief must not contain NUL".into()));
    }
    check_tags(tags)?;
    check_item_budget(budget)
}

/// A title an enqueue or an update accepts: one nonempty line of at most 200
/// characters, with no control or bidirectional-override character.
pub fn check_title(title: &str) -> Result<(), ArchonError> {
    if title.trim().is_empty() {
        return Err(ArchonError::Invalid("an item needs a title".into()));
    }
    if title.contains(['\u{2028}', '\u{2029}']) || title.trim().chars().count() > 200 {
        return Err(ArchonError::Invalid(
            "titles must be one line of at most 200 characters".into(),
        ));
    }
    if title.chars().any(|c| c.is_control() || is_bidi_control(c)) {
        return Err(ArchonError::Invalid(
            "titles must not contain control or bidirectional-override characters".into(),
        ));
    }
    Ok(())
}

/// The tag that wakes the orchestrator when the item finishes, however it
/// ends; an untagged success only joins its next trigger (§5.4).
pub const NOTIFY_TAG: &str = "notify";

pub fn check_tags(tags: &[String]) -> Result<(), ArchonError> {
    if tags.iter().any(|tag| tag.trim().is_empty()) {
        return Err(ArchonError::Invalid(
            "tags must not be empty or whitespace-only".into(),
        ));
    }
    Ok(())
}

pub fn check_item_budget(budget: Option<f64>) -> Result<(), ArchonError> {
    if budget.is_some_and(|usd| !usd.is_finite() || usd <= 0.0) {
        return Err(ArchonError::Invalid(
            "a budget must be a finite amount of USD greater than zero".into(),
        ));
    }
    Ok(())
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

/// Lifecycle of a backlog item. Terminal states never leave their column
/// except through an explicit operator `retry`, which re-queues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum QueueState {
    /// Waiting for dependencies, capacity, or an unpause.
    Queued,
    /// Chosen by the dispatcher; worktree and first stage being prepared.
    Dispatching,
    /// A pipeline run is active.
    Running {
        run: PipelineRunId,
    },
    /// A dependency failed or was cancelled; needs an operator decision.
    Blocked {
        reason: String,
    },
    Succeeded {
        run: PipelineRunId,
        summary: String,
    },
    Failed {
        run: Option<PipelineRunId>,
        error: String,
    },
    Cancelled {
        reason: Option<String>,
    },
}

impl QueueState {
    /// Every [`kind`](Self::kind), in lifecycle order: the names state
    /// filters accept.
    pub const KINDS: [&'static str; 7] = [
        "queued",
        "dispatching",
        "running",
        "blocked",
        "succeeded",
        "failed",
        "cancelled",
    ];

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Dispatching | Self::Running { .. })
    }

    /// The name used in listings and filters.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatching => "dispatching",
            Self::Running { .. } => "running",
            Self::Blocked { .. } => "blocked",
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
        }
    }

    /// The legal edges. Everything else is a programming error surfaced as a
    /// refusal rather than a silent overwrite.
    pub fn can_transition_to(&self, next: &QueueState) -> bool {
        use QueueState::*;
        matches!(
            (self, next),
            (Queued, Dispatching | Blocked { .. } | Cancelled { .. })
                | (
                    Dispatching,
                    Running { .. } | Failed { .. } | Cancelled { .. }
                )
                | (
                    Running { .. },
                    Succeeded { .. } | Failed { .. } | Cancelled { .. }
                )
                | (Blocked { .. }, Queued | Cancelled { .. })
                | (Succeeded { .. } | Failed { .. } | Cancelled { .. }, Queued)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: QueueItemId,
    /// `None` only for pipelines with `requires_repo = false` (e.g. `ask`).
    pub repo: Option<RepoId>,
    pub pipeline: String,
    /// One line, shown in listings and used for the branch name.
    pub title: String,
    /// The full brief handed to stage prompts as `{{task.brief}}`.
    pub brief: String,
    pub priority: Priority,
    /// Items that must have succeeded before this one dispatches.
    pub dependencies: Vec<QueueItemId>,
    pub tags: Vec<String>,
    pub model_plan: ModelPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasons: Option<ModelReasons>,
    /// Dollar cap for this item's whole run; `None` falls back to config.
    pub budget_usd: Option<f64>,
    /// Stage id to the minutes that stage may run, instead of the pipeline's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stage_timeouts: BTreeMap<String, u32>,
    /// Resolved at enqueue, within the ceiling (ARCHITECTURE §18).
    #[serde(default)]
    pub autonomy: Autonomy,
    /// The GitHub thread that asked for it; its outcome is reported there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<GithubThread>,
    /// Fresh retries so far; each started the item again from the base
    /// branch, on a branch of its own (ARCHITECTURE §13).
    #[serde(default)]
    pub fresh_retries: u32,
    /// The provider `[models] provider_weights` routed its tier-only choices
    /// to, chosen once from its id; stages keep it unless their floor rules
    /// it out. `None` while no weights are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_provider: Option<String>,
    /// Why its tier-only choices last moved off the provider it held, and
    /// when: its quota ran out. `None` while it never moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_switch: Option<String>,
    /// The initiative it is a member of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiative: Option<InitiativeId>,
    /// The existing pull request it delivers into, frozen at enqueue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<Box<OntoPullRequest>>,
    /// The risk level its last delivery labelled its pull request with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<crate::risk::RiskLevel>,
    pub state: QueueState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl QueueItem {
    /// Whether whoever queued it asked to be told when it finishes.
    pub fn wants_notice(&self) -> bool {
        self.tags.iter().any(|tag| tag == NOTIFY_TAG)
    }

    /// Apply a transition, refusing illegal edges.
    pub fn transition(&mut self, next: QueueState, at: Timestamp) -> Result<(), ArchonError> {
        if !self.state.can_transition_to(&next) {
            return Err(ArchonError::InvalidTransition(format!(
                "queue item {} cannot go from {} to {}",
                self.id,
                self.state.kind(),
                next.kind()
            )));
        }
        debug_assert!(at >= self.updated_at, "clock went backwards on {}", self.id);
        self.state = next;
        self.updated_at = at;
        Ok(())
    }

    /// Append `text` to the brief under a heading dated `at`, so the stages
    /// of the item's next run read it in `{{task.brief}}`.
    pub fn amend_brief(&mut self, text: &str, at: Timestamp) -> Result<(), ArchonError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchonError::Invalid(
                "brief_append is empty; give the text to add to the brief".into(),
            ));
        }
        let before = self.brief.len();
        let mut brief = self.brief.trim_end().to_owned();
        if !brief.is_empty() {
            brief.push_str("\n\n");
        }
        let stamp = at.format("%Y-%m-%d %H:%M UTC");
        brief.push_str(&format!("## Amendment ({stamp})\n\n{text}\n"));
        assert!(brief.len() > before, "an amendment adds to the brief");
        assert!(
            brief.ends_with(&format!("{text}\n")),
            "the amendment comes last"
        );
        self.brief = brief;
        Ok(())
    }
}

/// Concurrency and spend limits the dispatcher enforces. Zero means
/// "no limit" for every field, matching the config file's convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    /// Master switch for every dollar budget below, the items' own and the
    /// distillation reservation; spend is still recorded when off.
    #[serde(default = "budgets_on")]
    pub budgets_enabled: bool,
    pub max_parallel_agents: u32,
    pub max_per_repo: u32,
    pub max_per_pipeline: BTreeMap<String, u32>,
    /// Non-terminal pipeline runs; bounds worktrees and disk.
    pub max_active_runs: u32,
    pub budget_usd_total: f64,
    pub budget_usd_daily: f64,
    pub budget_usd_per_item: f64,
}

fn budgets_on() -> bool {
    true
}

/// A count cap as status and settings both show it: zero is no limit.
pub fn format_limit(value: u32) -> String {
    match value {
        0 => "unlimited".to_owned(),
        n => n.to_string(),
    }
}

/// A dollar cap as status and settings both show it: zero is no limit.
pub fn format_budget_usd(value: f64) -> String {
    assert!(value.is_finite(), "a budget is finite, got {value}");
    if value <= 0.0 {
        "unlimited".to_owned()
    } else {
        format!("${value:.2}")
    }
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            budgets_enabled: true,
            max_parallel_agents: 8,
            max_per_repo: 0,
            max_per_pipeline: BTreeMap::new(),
            max_active_runs: 0,
            budget_usd_total: 0.0,
            budget_usd_daily: 0.0,
            budget_usd_per_item: 0.0,
        }
    }
}

/// What is paused right now. Global pause stops all dispatch; the others
/// stop only their slice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseState {
    pub global: bool,
    pub repos: Vec<RepoId>,
    pub pipelines: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(title: &str, brief: &str) -> String {
        check_enqueue(title, brief, &[], None)
            .expect_err("refused")
            .to_string()
    }

    #[test]
    fn titles_with_control_or_bidi_characters_are_refused() {
        for title in [
            "\u{1b}[31mred",
            "a\0b",
            "a\u{202e}b",
            "a\u{85}b",
            "a\tb",
            "a\nb",
        ] {
            assert!(refused(title, "b").contains("title"), "{title:?}");
        }
        for title in ["café", "日本語", "ship it 🚀"] {
            check_enqueue(title, "b", &[], None).expect(title);
        }
    }

    #[test]
    fn briefs_with_nul_or_over_the_cap_are_refused_naming_the_cap() {
        assert!(refused("t", "a\0b").contains("NUL"));
        let big = "x".repeat(MAX_BRIEF_BYTES + 1);
        assert!(refused("t", &big).contains(&MAX_BRIEF_BYTES.to_string()));
        check_enqueue("t", &big[..MAX_BRIEF_BYTES], &[], None).expect("at the cap");
    }

    fn item() -> QueueItem {
        QueueItem {
            onto: None,
            id: QueueItemId::new(),
            repo: Some(RepoId::new()),
            pipeline: "direct".into(),
            title: "t".into(),
            brief: "b".into(),
            priority: Priority::Normal,
            dependencies: vec![],
            tags: vec![],
            model_plan: ModelPlan::default(),
            model_reasons: None,
            budget_usd: None,
            stage_timeouts: BTreeMap::new(),
            autonomy: Autonomy::default(),
            origin: None,
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            risk: None,
            state: QueueState::Queued,
            created_at: crate::now(),
            updated_at: crate::now(),
        }
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        let mut it = item();
        let run = PipelineRunId::new();
        it.transition(QueueState::Dispatching, crate::now())
            .unwrap();
        it.transition(QueueState::Running { run: run.clone() }, crate::now())
            .unwrap();
        it.transition(
            QueueState::Succeeded {
                run,
                summary: "done".into(),
            },
            crate::now(),
        )
        .unwrap();
        assert!(it.state.is_terminal());
    }

    #[test]
    fn illegal_edges_are_refused() {
        let mut it = item();
        let err = it
            .transition(
                QueueState::Succeeded {
                    run: PipelineRunId::new(),
                    summary: String::new(),
                },
                crate::now(),
            )
            .unwrap_err();
        assert!(matches!(err, ArchonError::InvalidTransition(_)));
        assert_eq!(it.state, QueueState::Queued);
    }

    /// One state of each kind. The match stops compiling when a state is
    /// added, so the new one gets a place here and in `KINDS`.
    fn one_of_each_state() -> Vec<QueueState> {
        let run = PipelineRunId::new();
        let states = vec![
            QueueState::Queued,
            QueueState::Dispatching,
            QueueState::Running { run: run.clone() },
            QueueState::Blocked { reason: "r".into() },
            QueueState::Succeeded {
                run,
                summary: "s".into(),
            },
            QueueState::Failed {
                run: None,
                error: "e".into(),
            },
            QueueState::Cancelled { reason: None },
        ];
        for state in &states {
            match state {
                QueueState::Queued
                | QueueState::Dispatching
                | QueueState::Running { .. }
                | QueueState::Blocked { .. }
                | QueueState::Succeeded { .. }
                | QueueState::Failed { .. }
                | QueueState::Cancelled { .. } => {}
            }
        }
        states
    }

    #[test]
    fn kinds_lists_every_state_once_as_the_wire_names_it() {
        let states = one_of_each_state();
        let kinds: Vec<&str> = states.iter().map(QueueState::kind).collect();
        assert_eq!(kinds, QueueState::KINDS);
        for state in &states {
            let wire = serde_json::to_value(state).unwrap();
            assert_eq!(wire["state"], state.kind(), "{state:?}");
        }
    }

    #[test]
    fn amendments_follow_the_brief_under_dated_headings() {
        let mut it = item();
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-29T14:03:00Z")
            .unwrap()
            .to_utc();
        it.amend_brief("  The reviewer found X; do Y.\n", at)
            .unwrap();
        assert_eq!(
            it.brief,
            "b\n\n## Amendment (2026-09-29 14:03 UTC)\n\nThe reviewer found X; do Y.\n"
        );
        it.amend_brief("Also Z.", at).unwrap();
        assert!(
            it.brief
                .ends_with("do Y.\n\n## Amendment (2026-09-29 14:03 UTC)\n\nAlso Z.\n")
        );

        let mut empty = item();
        empty.brief = String::new();
        empty.amend_brief("first words", at).unwrap();
        assert!(empty.brief.starts_with("## Amendment"), "{}", empty.brief);
    }

    #[test]
    fn a_blank_amendment_is_refused_and_changes_nothing() {
        let mut it = item();
        let err = it.amend_brief(" \n\t", crate::now()).unwrap_err();
        assert!(matches!(err, ArchonError::Invalid(_)), "{err:?}");
        assert_eq!(it.brief, "b");
    }

    #[test]
    fn items_stored_before_fresh_retries_read_as_none() {
        let mut stored = serde_json::to_value(item()).unwrap();
        stored.as_object_mut().unwrap().remove("fresh_retries");
        let read: QueueItem = serde_json::from_value(stored).unwrap();
        assert_eq!(read.fresh_retries, 0);
    }

    #[test]
    fn priority_orders_critical_first_when_reversed() {
        let mut p = vec![Priority::Low, Priority::Critical, Priority::Normal];
        p.sort_by(|a, b| b.cmp(a));
        assert_eq!(p, vec![Priority::Critical, Priority::Normal, Priority::Low]);
    }
}
