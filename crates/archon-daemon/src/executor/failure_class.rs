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
        StageState::Failed { .. } if stage.infrastructure_failure => {
            Some(FailureClass::Environmental)
        }
        StageState::Failed { error } => Some(of_error(error)),
        StageState::Spawning | StageState::Running => None,
    }
}

/// The class of a failure known only by its recorded `error` text.
pub(crate) fn of_error(error: &str) -> FailureClass {
    let lower = error.to_ascii_lowercase();
    if ENVIRONMENTAL_WORDS.iter().any(|word| lower.contains(word)) {
        return FailureClass::Environmental;
    }
    if error.contains(StageOutcome::BLOCKED_PREFIX) {
        return FailureClass::External;
    }
    FailureClass::Structural
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
    assert!(
        line.contains(&class.to_string()),
        "the line names its class"
    );
    line
}

#[cfg(test)]
#[path = "failure_class/tests.rs"]
mod tests;
