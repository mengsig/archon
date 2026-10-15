//! A pull request's checks as `gh pr checks` reports them, told apart by
//! whether they ran (ARCHITECTURE §8.7): a GitHub Actions job that never
//! started (billing, no runner, a queue that never moved) is not a failure
//! of the change, and only the operator can fix it.

use std::path::Path;

use archon_core::error::ArchonError;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{run, succeeded};

/// What `gh pr checks` says when the head commit has no checks at all.
const NO_CHECKS: &str = "no checks reported";
/// Most failing jobs asked why they failed, per look.
const MAX_PROBES: usize = 8;
/// How GitHub words the annotation of a job it never started.
const NOT_STARTED: &str = "job was not started";

/// One check of the pull request's head commit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Check {
    pub name: String,
    /// `pass`, `fail`, `pending`, `skipping` or `cancel`.
    pub bucket: String,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub link: String,
}

impl Check {
    /// GitHub reports an unstarted check with no time or the zero time.
    fn started(&self) -> bool {
        self.started_at.is_some_and(|at| at.timestamp() > 0)
    }
}

/// Where a pull request's checks stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Checks {
    /// The head commit has no checks (yet).
    None,
    Pending,
    Passed,
    /// Some check ran and failed; `names` are the failing ones.
    Failed {
        names: Vec<String>,
    },
    /// No check failed by running; `names` never started, for `reason`.
    NotStarted {
        names: Vec<String>,
        reason: String,
    },
}

/// The checks of the pull request at `url`. Once `waited_long`, a check still
/// queued counts as never started.
pub(crate) async fn of_pull_request(
    binary: &Path,
    repo: &Path,
    url: &str,
    waited_long: bool,
) -> Result<Checks, ArchonError> {
    assert!(url.contains("/pull/"), "a pull request's URL");
    let fields = "name,bucket,startedAt,link";
    let args = ["pr", "checks", url, "--json", fields];
    let ran = run(binary, repo, &args, None).await?;
    // `gh pr checks` exits non-zero while checks fail or are pending, yet prints them.
    let checks: Vec<Check> = match serde_json::from_str(ran.stdout.trim()) {
        Ok(checks) => checks,
        Err(_) if ran.detail().to_lowercase().contains(NO_CHECKS) => return Ok(Checks::None),
        Err(e) => {
            let said = succeeded(ran, &format!("reading the checks of {url}"))?;
            let said: String = said.chars().take(200).collect();
            return Err(ArchonError::Protocol(format!(
                "invalid `gh pr checks` response ({e}): {said}"
            )));
        }
    };
    let failing: Vec<&Check> = checks.iter().filter(|c| failed(c)).collect();
    let mut not_started = Vec::new();
    let mut reason = None;
    for check in failing.iter().take(MAX_PROBES) {
        // A probe GitHub refuses leaves the check failed: a failure is never hidden.
        match never_started(binary, repo, check).await {
            Ok(Some(why)) => {
                not_started.push(check.name.clone());
                reason.get_or_insert(why);
            }
            Ok(None) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::warn!(check = %check.name, %error, "not asked why the check failed");
            }
        }
    }
    // A refusal such as billing stops every job: past the probes, so are the rest.
    if not_started.len() == MAX_PROBES {
        not_started.extend(failing.iter().skip(MAX_PROBES).map(|c| c.name.clone()));
    }
    let queued = if waited_long {
        unstarted(&checks)
    } else {
        Vec::new()
    };
    Ok(classify(&checks, not_started, reason, queued))
}

/// GitHub cancels a job left queued too long, so a cancelled one is probed too.
fn failed(check: &Check) -> bool {
    matches!(check.bucket.as_str(), "fail" | "cancel")
}

/// Pending checks that never started, by name.
fn unstarted(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c.bucket == "pending" && !c.started())
        .map(|c| c.name.clone())
        .collect()
}

/// Where `checks` stand, given the failing ones GitHub `not_started` for
/// `reason` and the pending ones `queued` past their limit.
pub(crate) fn classify(
    checks: &[Check],
    not_started: Vec<String>,
    reason: Option<String>,
    queued: Vec<String>,
) -> Checks {
    if checks.is_empty() {
        return Checks::None;
    }
    let names: Vec<String> = checks
        .iter()
        .filter(|c| failed(c) && !not_started.contains(&c.name))
        .map(|c| c.name.clone())
        .collect();
    if !names.is_empty() {
        return Checks::Failed { names };
    }
    if !not_started.is_empty() {
        let reason = reason.expect("a job never started for a reason");
        return Checks::NotStarted {
            names: not_started,
            reason,
        };
    }
    if !queued.is_empty() {
        let reason = "queued without ever starting: no runner picked the jobs up".to_owned();
        return Checks::NotStarted {
            names: queued,
            reason,
        };
    }
    match checks.iter().any(|c| c.bucket == "pending") {
        true => Checks::Pending,
        false => Checks::Passed,
    }
}

/// Why GitHub never started the failing `check`, when it is an Actions job it
/// annotated so; any other check ran.
async fn never_started(
    binary: &Path,
    repo: &Path,
    check: &Check,
) -> Result<Option<String>, ArchonError> {
    let Some((host, slug, job)) = actions_job(&check.link) else {
        return Ok(None);
    };
    let path = format!("repos/{slug}/check-runs/{job}/annotations");
    let args = ["api", "--hostname", host, &path];
    let doing = format!("reading why check `{}` failed", check.name);
    let raw = succeeded(run(binary, repo, &args, None).await?, &doing)?;
    let annotations: Vec<Annotation> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid check annotations: {e}")))?;
    let found = annotations
        .into_iter()
        .find(|a| a.message.to_lowercase().contains(NOT_STARTED));
    Ok(found.map(|a| a.message.trim().to_owned()))
}

#[derive(Debug, Deserialize)]
struct Annotation {
    #[serde(default)]
    message: String,
}

/// The host, `owner/repository` and job id of an Actions job's check link:
/// `https://<host>/<owner>/<repository>/actions/runs/<run>/job/<job>`.
fn actions_job(link: &str) -> Option<(&str, String, &str)> {
    let rest = link.strip_prefix("https://")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [host, owner, repo, "actions", "runs", run, "job", job] = parts[..] else {
        return None;
    };
    let plain = |s: &str| {
        s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let shaped = plain(host) && plain(owner) && plain(repo) && numeric(run) && numeric(job);
    shaped.then(|| (host, format!("{owner}/{repo}"), job))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, bucket: &str, started: bool) -> Check {
        let at = if started {
            "2026-10-15T09:20:44Z"
        } else {
            "0001-01-01T00:00:00Z"
        };
        Check {
            name: name.into(),
            bucket: bucket.into(),
            started_at: Some(at.parse().unwrap()),
            link: String::new(),
        }
    }

    #[test]
    fn a_job_github_never_started_is_not_a_failure() {
        let checks = [check("lint", "fail", true), check("test", "fail", true)];
        let names = vec!["lint".to_owned(), "test".to_owned()];
        let why = "The job was not started because of billing".to_owned();
        assert_eq!(
            classify(&checks, names.clone(), Some(why.clone()), Vec::new()),
            Checks::NotStarted { names, reason: why }
        );
        let one = vec!["lint".to_owned()];
        assert_eq!(
            classify(&checks, one, Some("x".into()), Vec::new()),
            Checks::Failed {
                names: vec!["test".into()]
            },
            "a check that ran and failed is a failure"
        );
    }

    #[test]
    fn checks_pass_wait_or_sit_queued() {
        assert_eq!(classify(&[], Vec::new(), None, Vec::new()), Checks::None);
        let pending = [check("lint", "pass", true), check("test", "pending", false)];
        assert_eq!(
            classify(&pending, Vec::new(), None, Vec::new()),
            Checks::Pending
        );
        assert_eq!(unstarted(&pending), vec!["test".to_owned()]);
        let queued = classify(&pending, Vec::new(), None, unstarted(&pending));
        assert!(matches!(queued, Checks::NotStarted { names, .. } if names == ["test"]));
        let done = [
            check("lint", "pass", true),
            check("docs", "skipping", false),
        ];
        assert_eq!(
            classify(&done, Vec::new(), None, Vec::new()),
            Checks::Passed
        );
    }

    #[test]
    fn only_an_actions_job_link_is_probed() {
        let link = "https://github.com/o/r/actions/runs/38712245298/job/116346208996";
        assert_eq!(
            actions_job(link),
            Some(("github.com", "o/r".to_owned(), "116346208996"))
        );
        assert_eq!(actions_job("https://ci.example.com/build/7"), None);
        assert_eq!(
            actions_job("https://github.com/o/r/actions/runs/1/job/x"),
            None
        );
        assert_eq!(
            actions_job("https://github.com/o/../actions/runs/1/job/2"),
            None,
            "no path tricks"
        );
    }
}
