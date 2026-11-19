//! The class of a stage attempt's failure and the next step it suggests to
//! the orchestrator (A12). Advice only: archond never raises an effort or
//! changes a model because of a failure; the orchestrator picks the retry's plan.

use std::fmt;

use archon_core::model::{Effort, ModelChoice};
use archon_core::run::{StageOutcome, StageRun, StageState, Verdict};

/// Where a failure came from, read from what the stage run recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The work itself fell short: a failed verdict or check, the review
    /// cap, a timeout, a missing workspace.
    Structural,
    /// The world around it failed: quota, an engine crash, the catalog.
    Environmental,
    /// Someone outside stopped it: a cancel, or a blocker only the operator clears.
    External,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Structural => "structural",
            Self::Environmental => "environmental",
            Self::External => "external",
        })
    }
}

/// Lowercase words of a recorded error that name an environmental cause.
const ENVIRONMENTAL_WORDS: [&str; 9] = [
    "quota",
    "usagelimit",
    "usage limit",
    "ratelimit",
    "rate limit",
    "overloaded",
    "unavailable",
    "catalog",
    "engine login",
];

/// The class of a failure recorded as `stage`; `None` for a stage that did
/// not fail (it completed without a failing verdict, or is still live).
pub(crate) fn of_stage(stage: &StageRun) -> Option<FailureClass> {
    match &stage.state {
        StageState::TimedOut => Some(FailureClass::Structural),
        StageState::Cancelled => Some(FailureClass::External),
        StageState::Completed => {
            (stage.verdict == Some(Verdict::Fail)).then_some(FailureClass::Structural)
        }
        // A recorded marker names the cause better than the infrastructure flag.
        StageState::Failed { error } => {
            Some(marked(error).unwrap_or(if stage.infrastructure_failure {
                FailureClass::Environmental
            } else {
                of_text(error)
            }))
        }
        StageState::Spawning | StageState::Running => None,
    }
}

/// The class of a failure known only by its recorded `error` text.
pub(crate) fn of_error(error: &str) -> FailureClass {
    marked(error).unwrap_or_else(|| of_text(error))
}

/// The class a control prefix or daemon marker of `error` records: the
/// review cap or a stage's escalation, an operator stop, a missing workspace.
fn marked(error: &str) -> Option<FailureClass> {
    let lead = error.trim_start();
    if lead.starts_with(StageOutcome::ESCALATE_PREFIX)
        || error.contains(StageOutcome::WORKSPACE_MISSING)
    {
        return Some(FailureClass::Structural);
    }
    lead.starts_with(StageOutcome::BLOCKED_PREFIX)
        .then_some(FailureClass::External)
}

/// The class of unmarked `error` text: a provider or catalog word makes it
/// environmental, anything else is the work's own failure.
fn of_text(error: &str) -> FailureClass {
    let lower = error.to_ascii_lowercase();
    match ENVIRONMENTAL_WORDS.iter().any(|word| lower.contains(word)) {
        true => FailureClass::Environmental,
        false => FailureClass::Structural,
    }
}

/// The orchestrator's line for a failure of `class` whose attempt ran
/// `ran`, when known: the class and a suggested next attempt.
pub(crate) fn advice(class: FailureClass, ran: Option<&ModelChoice>) -> String {
    let next = match (class, ran) {
        (FailureClass::Structural, Some(ran)) if ran.effort < Effort::High => format!(
            "consider one effort step up from {} on {}",
            ran.effort, ran.model
        ),
        (FailureClass::Structural, Some(ran)) => format!(
            "consider a stronger model than {} (it ran at {})",
            ran.model, ran.effort
        ),
        (FailureClass::Structural, None) => "consider one effort step up".to_owned(),
        (_, Some(ran)) => format!("same model and effort ({} at {})", ran.model, ran.effort),
        (_, None) => "same model and effort".to_owned(),
    };
    let line = format!(
        "Failure class: {class}. Suggested next attempt: {next}. Archon changes no model or \
         effort itself; a retry's model_plan chooses them."
    );
    // A model name is operator input: it must not break the line apart.
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[path = "failure_class/tests.rs"]
pub(super) mod tests;
