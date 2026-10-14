//! The one way archond runs `gh` (ARCHITECTURE §8.7): in the registered
//! repository, never an agent's worktree, with no prompt and a time limit
//! that ends it and everything it started. The git `gh` runs itself is told
//! to run no hook or fsmonitor. Pull requests are read through one type,
//! checked against the branch they were asked about.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::dependencies::{is_plain_branch, is_plain_url};
use crate::worktree::daemon_git::no_program_settings;
use crate::worktree::stack::is_commit_id;

/// Most one `gh` command may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a tool's own words kept in an error.
const MAX_DETAIL_CHARS: usize = 1_500;
/// Most pull requests one listing returns: a repository's open ones, or a
/// branch's of any state.
const MAX_OPEN: &str = "1000";
const MAX_OF_BRANCH: &str = "20";
/// Everything archond reads of a pull request.
const FIELDS: &str =
    "number,url,state,headRefName,headRefOid,baseRefName,body,isDraft,reviewDecision";

/// What git or `gh` says when only a person can let the delivery through.
const NEEDS_OPERATOR: [&str; 14] = [
    "permission denied",
    "permission to",
    "authentication failed",
    "could not read username",
    "could not read password",
    "terminal prompts disabled",
    "repository not found",
    "gh auth login",
    "bad credentials",
    "http 401",
    "http 403",
    "resource not accessible",
    "could not resolve to a repository",
    "known github host",
];

/// A pull request as `gh` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequest {
    pub number: u64,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    pub base_ref_name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub is_draft: bool,
    /// `CHANGES_REQUESTED`, `APPROVED`, `REVIEW_REQUIRED`, or empty.
    #[serde(default)]
    pub review_decision: Option<String>,
}

impl PullRequest {
    pub(crate) fn is_open(&self) -> bool {
        self.state == "OPEN"
    }

    /// Refuse what GitHub could not have meant: a pull request of another
    /// branch, an unknown state or a URL that is not its own.
    fn check(&self, branch: Option<&str>) -> Result<(), ArchonError> {
        let shaped = self.number != 0
            && is_plain_url(&self.url)
            && self.url.ends_with(&format!("/pull/{}", self.number))
            && is_plain_branch(&self.head_ref_name)
            && is_plain_branch(&self.base_ref_name)
            && matches!(self.state.as_str(), "OPEN" | "CLOSED" | "MERGED")
            && self.head_ref_oid.as_deref().is_none_or(is_commit_id)
            && branch.is_none_or(|branch| branch == self.head_ref_name);
        if !shaped {
            return Err(ArchonError::Protocol(format!(
                "GitHub listed a pull request that does not match its branch, state or URL: {} \
                 ({}, head `{}`)",
                self.url, self.state, self.head_ref_name
            )));
        }
        Ok(())
    }
}

/// What a finished `gh` printed.
#[derive(Debug)]
pub(crate) struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The command's own account of a failure, for an error.
    pub(crate) fn detail(&self) -> String {
        let said = match self.stderr.trim() {
            "" => self.stdout.trim(),
            stderr => stderr,
        };
        said.chars().take(MAX_DETAIL_CHARS).collect()
    }
}

/// `BLOCKED:` and why, when `said` shows that only a person can help.
pub(crate) fn needs_operator(doing: &str, said: &str) -> Option<String> {
    let lower = said.to_lowercase();
    let blocked = NEEDS_OPERATOR.iter().any(|sign| lower.contains(sign));
    blocked.then(|| format!("BLOCKED: {doing} was refused for want of access: {said}"))
}

/// `gh <args>` in `repo` with `input` on its stdin, for at most [`TIMEOUT`].
pub(crate) async fn run(
    binary: &Path,
    repo: &Path,
    args: &[&str],
    input: Option<&str>,
) -> Result<Ran, ArchonError> {
    assert!(repo.is_absolute(), "gh runs in a registered repository");
    assert!(!args.is_empty(), "a gh command has arguments");
    let what = format!(
        "gh {}",
        args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
    );
    let mut command = command(binary, repo, args);
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .spawn()
        .map_err(|e| ArchonError::io(format!("running {what}"), e))?;
    let group = child.id().map(|pid| pid as i32);
    let work = async {
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    let Ok(output) = tokio::time::timeout(TIMEOUT, work).await else {
        kill_group(group);
        return Err(ArchonError::Unavailable(format!(
            "{what} timed out after {}s",
            TIMEOUT.as_secs()
        )));
    };
    let output = output.map_err(|e| ArchonError::io(what, e))?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn command(binary: &Path, repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let settings = no_program_settings();
    for (n, (key, value)) in settings.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    command
        .env("GIT_CONFIG_COUNT", settings.len().to_string())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GH_REPO")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    command
}

/// End `gh` and whatever it started, after it overran.
fn kill_group(group: Option<i32>) {
    let Some(pid) = group.and_then(rustix::process::Pid::from_raw) else {
        return;
    };
    // Already gone is fine: kill_on_drop reaps what is left.
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
}

/// `gh <args>`, which must succeed: its stdout. A failure is worth another
/// try unless it shows that only the operator can help.
pub(crate) async fn text(binary: &Path, repo: &Path, args: &[&str]) -> Result<String, ArchonError> {
    let ran = run(binary, repo, args, None).await?;
    succeeded(ran, &format!("gh {}", args.join(" ")))
}

/// The stdout of `ran`, which was `doing`, or why it failed.
fn succeeded(ran: Ran, doing: &str) -> Result<String, ArchonError> {
    if ran.ok {
        return Ok(ran.stdout);
    }
    let said = ran.detail();
    assert!(!doing.is_empty(), "a failure says what failed");
    match needs_operator(doing, &said) {
        Some(blocked) => Err(ArchonError::Invalid(blocked)),
        None => Err(ArchonError::Unavailable(format!("{doing} failed: {said}"))),
    }
}

/// `gh pr list <filter>`, which is `doing`, each pull request checked.
async fn list(
    binary: &Path,
    repo: &Path,
    filter: &[&str],
    branch: Option<&str>,
    doing: &str,
) -> Result<Vec<PullRequest>, ArchonError> {
    assert!(
        filter.contains(&"--limit"),
        "gh lists 30 unless told otherwise"
    );
    assert!(
        branch.is_none_or(|branch| filter.contains(&branch)),
        "only a listing of one branch is checked against it"
    );
    let args = [&["pr", "list"], filter, &["--json", FIELDS]].concat();
    let raw = succeeded(run(binary, repo, &args, None).await?, doing)?;
    let prs: Vec<PullRequest> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid `gh pr list` response: {e}")))?;
    for pr in &prs {
        pr.check(branch)?;
    }
    Ok(prs)
}

/// The pull requests whose head is `branch`, newest first: open ones only,
/// or of every state.
pub(crate) async fn of_branch(
    binary: &Path,
    repo: &Path,
    branch: &str,
    only_open: bool,
) -> Result<Vec<PullRequest>, ArchonError> {
    if !is_plain_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is no plain branch name"
        )));
    }
    let state = if only_open { "open" } else { "all" };
    let filter = ["--head", branch, "--state", state, "--limit", MAX_OF_BRANCH];
    let doing = format!("looking up the pull request of `{branch}`");
    let prs = list(binary, repo, &filter, Some(branch), &doing).await?;
    assert!(!only_open || prs.iter().all(PullRequest::is_open));
    Ok(prs)
}

/// The pull request of `branch`: its open one, else its newest.
pub(crate) async fn view(
    binary: &Path,
    repo: &Path,
    branch: &str,
) -> Result<PullRequest, ArchonError> {
    let prs = of_branch(binary, repo, branch, false).await?;
    let found = prs.iter().find(|pr| pr.is_open()).or(prs.first());
    let Some(pr) = found else {
        return Err(ArchonError::Unavailable(format!(
            "GitHub lists no pull request for `{branch}`"
        )));
    };
    assert_eq!(pr.head_ref_name, branch, "checked by list");
    Ok(pr.clone())
}

/// One pass's reading of GitHub: one listing of each repository's open pull
/// requests, then one lookup for each pull request asked about that it lacks.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    open: HashMap<PathBuf, Vec<PullRequest>>,
}

impl Pass {
    /// The pull request of `branch` in `repo`, as [`view`] finds it.
    pub(crate) async fn pull_request(
        &mut self,
        binary: &Path,
        repo: &Path,
        branch: &str,
    ) -> Result<PullRequest, ArchonError> {
        if !self.open.contains_key(repo) {
            let filter = ["--state", "open", "--limit", MAX_OPEN];
            let doing = "listing the open pull requests";
            let listed = list(binary, repo, &filter, None, doing).await?;
            self.open.insert(repo.to_owned(), listed);
        }
        let listed = self.open.get(repo).expect("listed above");
        assert!(listed.iter().all(PullRequest::is_open), "an open listing");
        if let Some(pr) = listed
            .iter()
            .find(|pr| pr.head_ref_name == branch && pr.is_open())
        {
            return Ok(pr.clone());
        }
        view(binary, repo, branch).await
    }
}

/// `kv`: `pr_settled:<repository>:<branch>` → the `MERGED` or `CLOSED`
/// GitHub said the branch's pull request ended as; it is not asked again.
const SETTLED_PREFIX: &str = "pr_settled:";

fn settled_key(repo: &archon_core::id::RepoId, branch: &str) -> String {
    format!("{SETTLED_PREFIX}{repo}:{branch}")
}

/// How the pull request of `branch` ended, if it did.
pub(crate) fn settled(
    store: &archon_queue::Store,
    repo: &archon_core::id::RepoId,
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    let state = store.kv_get(&settled_key(repo, branch))?;
    assert!(
        state
            .as_deref()
            .is_none_or(|s| matches!(s, "MERGED" | "CLOSED"))
    );
    Ok(state)
}

/// Remember `pr` when it ended, so no later pass asks about it.
pub(crate) fn settle(
    store: &mut archon_queue::Store,
    repo: &archon_core::id::RepoId,
    pr: &PullRequest,
) -> Result<(), ArchonError> {
    if pr.is_open() {
        return Ok(());
    }
    assert!(
        matches!(pr.state.as_str(), "MERGED" | "CLOSED"),
        "checked by list"
    );
    store.kv_set(&settled_key(repo, &pr.head_ref_name), &pr.state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: &str, head: &str) -> PullRequest {
        PullRequest {
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
            state: state.into(),
            head_ref_name: head.into(),
            head_ref_oid: Some("a".repeat(40)),
            base_ref_name: "main".into(),
            body: String::new(),
            is_draft: false,
            review_decision: None,
        }
    }

    #[test]
    fn a_pull_request_of_another_branch_or_state_is_refused() {
        assert!(pr("OPEN", "a/b").check(Some("a/b")).is_ok());
        assert!(pr("MERGED", "a/b").check(None).is_ok());
        assert!(pr("OPEN", "a/c").check(Some("a/b")).is_err());
        assert!(pr("DRAFT", "a/b").check(None).is_err());
        let mut other = pr("OPEN", "a/b");
        other.url = "https://github.com/o/r/pull/8".into();
        assert!(
            other.check(None).is_err(),
            "the URL is another pull request's"
        );
    }

    #[test]
    fn only_missing_access_blocks_for_the_operator() {
        let denied = "remote: Permission to o/r.git denied to someone.\nfatal: HTTP 403";
        let blocked = needs_operator("pushing `b` to `origin`", denied).unwrap();
        assert!(blocked.starts_with("BLOCKED: pushing `b` to `origin` was refused"));
        let network = "fatal: unable to access 'https://x/': Could not resolve host: x";
        assert_eq!(
            needs_operator("pushing", network),
            None,
            "worth another try"
        );
    }

    #[test]
    fn a_failure_is_told_in_the_tool_s_own_words() {
        let ran = |stdout: &str, stderr: &str| Ran {
            ok: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        assert_eq!(ran("out\n", " err \n").detail(), "err");
        assert_eq!(
            ran(" out \n", " \n").detail(),
            "out",
            "gh prints some errors to stdout"
        );
        let long = ran("", &"x".repeat(MAX_DETAIL_CHARS * 2));
        assert_eq!(long.detail().chars().count(), MAX_DETAIL_CHARS);
    }
}
