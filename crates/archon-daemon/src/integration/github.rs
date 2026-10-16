//! The GitHub check runs on one commit of the integration branch: when CI
//! covers the branch, they verify it and no full check runs here.

use std::path::Path;

use archon_core::error::ArchonError;
use serde::Deserialize;

use super::full::excerpt_of;
use crate::executor::gh;
use crate::worktree::stack::is_commit_id;

/// Check runs read per page, and the most pages read for one commit.
const PER_PAGE: u32 = 100;
const MAX_PAGES: u32 = 10;
/// Characters of a failing check's own summary kept, and summaries kept.
const MAX_SUMMARY_CHARS: usize = 300;
const MAX_SUMMARIES: usize = 10;
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
    Failed(Vec<FailedCheck>),
}

/// A check run that ran and did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedCheck {
    pub name: String,
    /// The check run's id: a GitHub Actions job's id too.
    id: u64,
    /// Run by GitHub Actions, so its job log can be read.
    actions: bool,
    /// The check's own title and summary of what failed.
    summary: String,
}

#[derive(Debug, Deserialize)]
struct CheckRuns {
    total_count: usize,
    check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct CheckRun {
    #[serde(default)]
    id: u64,
    name: String,
    /// `queued`, `in_progress` or `completed`.
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    app: Option<CheckApp>,
    #[serde(default)]
    output: Option<CheckOutput>,
}

#[derive(Debug, Deserialize)]
struct CheckApp {
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckOutput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// The check runs GitHub holds for `commit` of `github`, every page of
/// them, asked with `gh` in the registered repository at `repo`.
pub(crate) async fn commit_checks(
    binary: &Path,
    repo: &Path,
    github: &GithubRepo,
    commit: &str,
) -> Result<CommitChecks, ArchonError> {
    assert!(is_commit_id(commit), "a full commit id");
    let mut runs = Vec::new();
    for page in 1..=MAX_PAGES {
        let path = format!(
            "repos/{}/commits/{commit}/check-runs?per_page={PER_PAGE}&page={page}",
            github.slug
        );
        let read: CheckRuns = serde_json::from_str(&api(binary, repo, github, &path).await?)
            .map_err(|e| ArchonError::Protocol(format!("invalid check runs of {commit}: {e}")))?;
        let listed = read.check_runs.len();
        runs.extend(read.check_runs);
        if runs.len() >= read.total_count {
            return Ok(classify(&runs));
        }
        if listed == 0 {
            return Err(ArchonError::Unavailable(format!(
                "GitHub listed {} of the {} check runs of {commit}",
                runs.len(),
                read.total_count
            )));
        }
    }
    Err(ArchonError::Unavailable(format!(
        "{commit} has more than {} check runs",
        MAX_PAGES * PER_PAGE
    )))
}

/// `gh api <path>` on `github`'s host.
async fn api(
    binary: &Path,
    repo: &Path,
    github: &GithubRepo,
    path: &str,
) -> Result<String, ArchonError> {
    assert!(!path.starts_with('/'), "a path under the API root");
    gh::text(
        binary,
        repo,
        &["api", "--hostname", github.host.as_str(), path],
    )
    .await
}

fn classify(runs: &[CheckRun]) -> CommitChecks {
    if runs.is_empty() {
        return CommitChecks::None;
    }
    let failed: Vec<FailedCheck> = runs
        .iter()
        .filter(|run| {
            run.conclusion
                .as_deref()
                .is_some_and(|c| FAILED.contains(&c))
        })
        .map(failed_check)
        .collect();
    if !failed.is_empty() {
        return CommitChecks::Failed(failed);
    }
    match runs.iter().all(|run| run.status == "completed") {
        true => CommitChecks::Passed,
        false => CommitChecks::Pending,
    }
}

fn failed_check(run: &CheckRun) -> FailedCheck {
    let output = run.output.as_ref();
    let said = [
        output.and_then(|o| o.title.as_deref()),
        output.and_then(|o| o.summary.as_deref()),
    ];
    let summary: String = said
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect();
    let app = run.app.as_ref().and_then(|a| a.slug.as_deref());
    FailedCheck {
        name: run.name.clone(),
        id: run.id,
        actions: app == Some("github-actions") && run.id > 0,
        summary,
    }
}

/// What failed, for the fix item: each failing check's own summary, and the
/// end of the first failing GitHub Actions job's log, or why it is missing.
pub(crate) async fn failure_excerpt(
    binary: &Path,
    repo: &Path,
    github: &GithubRepo,
    failed: &[FailedCheck],
) -> String {
    assert!(!failed.is_empty(), "a red commit has a failing check");
    let Some(job) = failed.iter().find(|check| check.actions) else {
        return describe_failures(failed, None);
    };
    let path = format!("repos/{}/actions/jobs/{}/logs", github.slug, job.id);
    let log = api(binary, repo, github, &path).await;
    if let Err(error) = &log {
        tracing::warn!(check = %job.name, %error, "the failing job's log was not read");
    }
    describe_failures(failed, Some((job, log.map_err(|e| e.to_string()))))
}

fn describe_failures(
    failed: &[FailedCheck],
    log: Option<(&FailedCheck, Result<String, String>)>,
) -> String {
    let names: Vec<&str> = failed.iter().map(|check| check.name.as_str()).collect();
    let mut text = format!("failing GitHub checks: {}", names.join(", "));
    for check in failed.iter().take(MAX_SUMMARIES) {
        if !check.summary.is_empty() {
            text.push_str(&format!("\n{}: {}", check.name, check.summary));
        }
    }
    match log {
        Some((job, Ok(log))) => {
            text.push_str(&format!(
                "\n\nThe end of the log of `{}`:\n{}",
                job.name,
                excerpt_of(&log)
            ));
        }
        Some((job, Err(error))) => {
            text.push_str(&format!(
                "\n[the log of `{}` could not be read: {error}]",
                job.name
            ));
        }
        None => {}
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            id: 0,
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(str::to_owned),
            app: None,
            output: None,
        }
    }

    fn names(checks: CommitChecks) -> Vec<String> {
        match checks {
            CommitChecks::Failed(failed) => failed.into_iter().map(|c| c.name).collect(),
            other => panic!("not failed: {other:?}"),
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
        assert_eq!(names(classify(&failed)), ["test"]);
    }

    /// A fake `gh` serving `dir`'s files: two pages of check runs, the
    /// second holding the failure, and the failing job's log.
    fn fake_gh(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let passed = r#"{"id":1,"name":"lint","status":"completed","conclusion":"success"}"#;
        let first = vec![passed; 100].join(",");
        std::fs::write(
            dir.join("page1"),
            format!(r#"{{"total_count":101,"check_runs":[{first}]}}"#),
        )
        .unwrap();
        let failed = r#"{"id":7,"name":"test","status":"completed","conclusion":"failure",
            "app":{"slug":"github-actions"},"output":{"title":"Tests failed","summary":"1 failing"}}"#;
        std::fs::write(
            dir.join("page2"),
            format!(r#"{{"total_count":101,"check_runs":[{failed}]}}"#),
        )
        .unwrap();
        let log: String = (1..=200).map(|n| format!("{n}\n")).collect();
        std::fs::write(dir.join("log"), format!("{log}error: assertion failed\n")).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$4\" in\n*page=1) cat {0}/page1;;\n*page=2) cat {0}/page2;;\n\
             */actions/jobs/7/logs) cat {0}/log;;\n*) exit 1;;\nesac\n",
            dir.display()
        );
        let gh = dir.join("gh");
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        gh
    }

    /// A failure on the second page is found, and the fix item gets the end
    /// of the failing job's log, or why it is missing.
    #[tokio::test]
    async fn every_page_is_read_and_the_failing_job_s_log_is_excerpted() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let gh = fake_gh(&dir);
        let github = github_repo("https://github.com/o/r").unwrap();
        let checks = commit_checks(&gh, &dir, &github, COMMIT).await.unwrap();
        let CommitChecks::Failed(failed) = checks else {
            panic!("the second page's failure is missed: {checks:?}");
        };
        assert_eq!(failed.len(), 1);
        let excerpt = failure_excerpt(&gh, &dir, &github, &failed).await;
        assert!(
            excerpt.starts_with("failing GitHub checks: test\ntest: Tests failed: 1 failing"),
            "{excerpt}"
        );
        assert!(
            excerpt.ends_with("\n200\nerror: assertion failed"),
            "{excerpt}"
        );
        assert!(
            !excerpt.contains("\n120\n"),
            "only the log's end: {excerpt}"
        );

        std::fs::remove_file(dir.join("log")).unwrap();
        let excerpt = failure_excerpt(&gh, &dir, &github, &failed).await;
        assert!(
            excerpt.contains("[the log of `test` could not be read"),
            "{excerpt}"
        );
    }
}
