//! The GitHub check runs on one commit of the integration branch: when CI
//! covers the branch, they verify it and no full check runs here.

use std::path::Path;

use archon_core::error::ArchonError;
use serde::Deserialize;

use crate::executor::gh;
use crate::worktree::stack::is_commit_id;

/// Check runs read per commit; a branch's CI has far fewer.
const PER_PAGE: u32 = 100;
/// Conclusions of a check that ran and did not pass.
const FAILED: [&str; 6] = [
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
    "stale",
];

/// The GitHub repository an `origin` URL names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubRepo {
    host: String,
    /// `owner/repository`.
    slug: String,
}

/// `https://<host>/<owner>/<repo>`, `ssh://git@<host>/<owner>/<repo>` or
/// `git@<host>:<owner>/<repo>`, with or without `.git`; `None` for a local path.
pub(crate) fn github_repo(url: &str) -> Option<GithubRepo> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("ssh://"))
        .map(|rest| rest.split_once('/'))
        .unwrap_or_else(|| url.split_once(':').filter(|(host, _)| host.contains('@')))?;
    let (host, path) = rest;
    let host = host.rsplit('@').next()?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let plain = |s: &str| {
        s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    let named = plain(host) && host.contains('.') && plain(owner) && plain(repo);
    named.then(|| GithubRepo {
        host: host.to_owned(),
        slug: format!("{owner}/{repo}"),
    })
}

/// Where a commit's check runs stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitChecks {
    /// GitHub reports no check on it: CI does not cover the branch (yet).
    None,
    Pending,
    Passed,
    /// The names of the checks that failed.
    Failed(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct CheckRuns {
    check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct CheckRun {
    name: String,
    /// `queued`, `in_progress` or `completed`.
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}

/// The check runs GitHub holds for `commit` of `github`, asked with `gh`
/// in the registered repository at `repo`.
pub(crate) async fn commit_checks(
    binary: &Path,
    repo: &Path,
    github: &GithubRepo,
    commit: &str,
) -> Result<CommitChecks, ArchonError> {
    assert!(is_commit_id(commit), "a full commit id");
    let path = format!(
        "repos/{}/commits/{commit}/check-runs?per_page={PER_PAGE}",
        github.slug
    );
    let args = ["api", "--hostname", github.host.as_str(), path.as_str()];
    let raw = gh::text(binary, repo, &args).await?;
    let runs: CheckRuns = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid check runs of {commit}: {e}")))?;
    Ok(classify(&runs.check_runs))
}

fn classify(runs: &[CheckRun]) -> CommitChecks {
    if runs.is_empty() {
        return CommitChecks::None;
    }
    let failed: Vec<String> = runs
        .iter()
        .filter(|run| {
            run.conclusion
                .as_deref()
                .is_some_and(|c| FAILED.contains(&c))
        })
        .map(|run| run.name.clone())
        .collect();
    if !failed.is_empty() {
        return CommitChecks::Failed(failed);
    }
    match runs.iter().all(|run| run.status == "completed") {
        true => CommitChecks::Passed,
        false => CommitChecks::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(str::to_owned),
        }
    }

    #[test]
    fn origin_urls_name_their_github_repository() {
        let expected = Some(GithubRepo {
            host: "github.com".into(),
            slug: "mengsig/archon".into(),
        });
        for url in [
            "https://github.com/mengsig/archon",
            "https://github.com/mengsig/archon.git",
            "git@github.com:mengsig/archon.git",
            "ssh://git@github.com/mengsig/archon",
        ] {
            assert_eq!(github_repo(url), expected, "{url}");
        }
        for local in [
            "/srv/origin.git",
            "file:///srv/r",
            "./a:b",
            "https://github.com/o",
        ] {
            assert_eq!(github_repo(local), None, "{local}");
        }
        assert_eq!(
            github_repo("https://github.com/o/../x"),
            None,
            "no path tricks"
        );
    }

    #[test]
    fn a_failed_run_is_red_and_a_running_one_waits() {
        assert_eq!(classify(&[]), CommitChecks::None);
        let passed = [
            run("test", "completed", Some("success")),
            run("docs", "completed", Some("skipped")),
        ];
        assert_eq!(classify(&passed), CommitChecks::Passed);
        let running = [
            run("test", "in_progress", None),
            run("lint", "completed", Some("success")),
        ];
        assert_eq!(classify(&running), CommitChecks::Pending);
        let failed = [
            run("test", "completed", Some("failure")),
            run("lint", "queued", None),
        ];
        assert_eq!(classify(&failed), CommitChecks::Failed(vec!["test".into()]));
    }
}
