//! An open pull request's review decision and checks (vNext D3), which the
//! issues API does not carry: read with `gh pr view --json`, which only
//! reads. GitHub's review decision exists only there, branch rules applied.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::protocol::GithubRef;
use archon_core::protocol::github_links::{ChecksState, ChecksSummary, ReviewDecision};
use archon_core::repo::{GH_HOST, Repo};
use serde::Deserialize;

use crate::executor::gh;

/// The only fields a read asks for.
pub(crate) const FIELDS: &str = "reviewDecision,statusCheckRollup";

/// What a read of one pull request found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrState {
    /// `None` where no review decides it.
    pub review_decision: Option<ReviewDecision>,
    pub checks: ChecksSummary,
}

/// The `gh` arguments that read `reference`'s review and checks.
pub(crate) fn view_args(reference: &GithubRef) -> Vec<String> {
    assert_eq!(
        reference.host, GH_HOST,
        "only github.com pull requests are read"
    );
    let url = format!(
        "https://github.com/{}/pull/{}",
        reference.repo, reference.number
    );
    let args: Vec<String> = ["pr", "view", &url, "--json", FIELDS]
        .map(String::from)
        .into();
    assert_eq!(&args[..2], ["pr", "view"], "a read, never an edit");
    args
}

/// Read `reference`'s review decision and checks as `repo`'s login.
pub(crate) async fn read(
    binary: &Path,
    repo: &Repo,
    reference: &GithubRef,
) -> Result<PrState, ArchonError> {
    let args = view_args(reference);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = gh::run(binary, repo, &args, None).await?;
    if !ran.ok {
        return Err(ArchonError::Unavailable(format!(
            "gh pr view of {}#{} failed: {}",
            reference.repo,
            reference.number,
            ran.detail()
        )));
    }
    parse(&ran.stdout).map_err(|e| {
        ArchonError::Protocol(format!(
            "gh pr view of {}#{} did not parse: {e}",
            reference.repo, reference.number
        ))
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    status_check_rollup: Vec<Rollup>,
}

/// One check run (`status`, `conclusion`) or commit status (`state`).
#[derive(Debug, Deserialize)]
struct Rollup {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pending,
    Success,
    Neutral,
    Failed,
}

impl Rollup {
    fn outcome(&self) -> Outcome {
        if let Some(state) = &self.state {
            return match state.as_str() {
                "SUCCESS" => Outcome::Success,
                "FAILURE" | "ERROR" => Outcome::Failed,
                _ => Outcome::Pending,
            };
        }
        if self.status.as_deref() != Some("COMPLETED") {
            return Outcome::Pending;
        }
        match self.conclusion.as_deref() {
            Some("SUCCESS") => Outcome::Success,
            Some(
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
                | "STALE",
            ) => Outcome::Failed,
            _ => Outcome::Neutral,
        }
    }
}

fn review_decision(raw: Option<&str>) -> Result<Option<ReviewDecision>, String> {
    match raw {
        None | Some("") => Ok(None),
        Some("APPROVED") => Ok(Some(ReviewDecision::Approved)),
        Some("CHANGES_REQUESTED") => Ok(Some(ReviewDecision::ChangesRequested)),
        Some("REVIEW_REQUIRED") => Ok(Some(ReviewDecision::ReviewRequired)),
        Some(other) => Err(format!("review decision {other:?}")),
    }
}

/// The checks rolled up: any failure fails them, else any pending one
/// keeps them pending, else one success passes them; none is neutral.
fn summarize(outcomes: &[Outcome]) -> Result<ChecksSummary, String> {
    let count = |o: Outcome| outcomes.iter().filter(|x| **x == o).count();
    let total = u32::try_from(outcomes.len()).map_err(|_| "too many checks".to_owned())?;
    let failed = u32::try_from(count(Outcome::Failed)).expect("at most the total");
    let state = if failed > 0 {
        ChecksState::Failure
    } else if count(Outcome::Pending) > 0 {
        ChecksState::Pending
    } else if count(Outcome::Success) > 0 {
        ChecksState::Success
    } else {
        ChecksState::Neutral
    };
    assert!(failed <= total, "failed checks are some of them");
    Ok(ChecksSummary {
        state,
        total,
        failed,
    })
}

fn parse(stdout: &str) -> Result<PrState, String> {
    let view: View = serde_json::from_str(stdout.trim()).map_err(|e| e.to_string())?;
    let outcomes: Vec<Outcome> = view
        .status_check_rollup
        .iter()
        .map(Rollup::outcome)
        .collect();
    Ok(PrState {
        review_decision: review_decision(view.review_decision.as_deref())?,
        checks: summarize(&outcomes)?,
    })
}

#[cfg(test)]
mod tests {
    use archon_core::protocol::GithubKind;

    use super::*;

    #[test]
    fn a_read_is_gh_pr_view_of_two_fields() {
        let reference = GithubRef {
            host: GH_HOST.into(),
            repo: "o/r".into(),
            number: 6,
            kind: GithubKind::PullRequest,
        };
        assert_eq!(
            view_args(&reference),
            [
                "pr",
                "view",
                "https://github.com/o/r/pull/6",
                "--json",
                FIELDS
            ]
        );
    }

    #[test]
    fn the_rollup_fails_on_any_failure_and_waits_on_any_pending_check() {
        let failing = r#"{"reviewDecision":"APPROVED","statusCheckRollup":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
            {"__typename":"StatusContext","state":"ERROR"}]}"#;
        let state = parse(failing).unwrap();
        assert_eq!(state.review_decision, Some(ReviewDecision::Approved));
        let failure = ChecksSummary {
            state: ChecksState::Failure,
            total: 3,
            failed: 1,
        };
        assert_eq!(state.checks, failure);
        let pending = r#"{"reviewDecision":"","statusCheckRollup":[
            {"status":"COMPLETED","conclusion":"SKIPPED"},{"state":"PENDING"}]}"#;
        let state = parse(pending).unwrap();
        assert_eq!(state.review_decision, None);
        assert_eq!(state.checks.state, ChecksState::Pending);
        let none = parse(r#"{"reviewDecision":"REVIEW_REQUIRED","statusCheckRollup":[]}"#).unwrap();
        let neutral = ChecksSummary {
            state: ChecksState::Neutral,
            total: 0,
            failed: 0,
        };
        assert_eq!(
            (none.review_decision, none.checks),
            (Some(ReviewDecision::ReviewRequired), neutral)
        );
        assert!(parse(r#"{"reviewDecision":"MAYBE"}"#).is_err());
    }
}
