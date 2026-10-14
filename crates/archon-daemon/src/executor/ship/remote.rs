//! Delivery beyond the machine (ARCHITECTURE §8.7): the push of a run branch
//! and its pull request through `gh`. Nothing is ever forced, no repository
//! hook runs, and a pull request counts only as GitHub lists it.
//!
//! An inner `Err(String)` is a refusal no retry changes; the outer error is a
//! failure worth trying again (the network, GitHub, a timeout).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const PUSH_TIMEOUT: Duration = Duration::from_secs(180);
const GH_TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a tool's own words kept in an error.
const MAX_DETAIL_CHARS: usize = 1_500;

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

/// An open pull request as `gh pr list` reports it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    url: String,
    number: u64,
    head_ref_name: String,
    state: String,
    #[serde(default)]
    base_ref_name: Option<String>,
}

/// The pull request a run branch is delivered as.
#[derive(Debug, Clone, Copy)]
pub(super) struct Request<'a> {
    pub branch: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// Opened as a draft: a member of an atomic initiative, until all pass.
    pub draft: bool,
}

/// What a finished command printed.
struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// The command's own account of a failure, for an error.
    fn detail(&self) -> String {
        let said = match self.stderr.trim() {
            "" => self.stdout.trim(),
            stderr => stderr,
        };
        said.chars().take(MAX_DETAIL_CHARS).collect()
    }
}

/// Run `command` with `input` on its stdin, for at most `limit`.
async fn run(
    mut command: Command,
    what: &str,
    input: Option<&str>,
    limit: Duration,
) -> Result<Ran, ArchonError> {
    assert!(!what.is_empty() && !limit.is_zero());
    let stdin = match input {
        Some(_) => Stdio::piped(),
        None => Stdio::null(),
    };
    command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let work = async {
        let mut child = command.spawn()?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(limit, work)
        .await
        .map_err(|_| {
            ArchonError::Unavailable(format!("{what} timed out after {}s", limit.as_secs()))
        })?
        .map_err(|e| ArchonError::io(what.to_owned(), e))?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `git` in `workspace` as the daemon runs it for a delivery: no hook, no
/// fsmonitor and no prompt, so nothing a run configured runs as the daemon.
fn git(workspace: &Path, args: &[&str]) -> Command {
    assert!(workspace.is_absolute() && !args.is_empty());
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn gh(binary: &Path, workspace: &Path, args: &[&str]) -> Command {
    assert!(workspace.is_absolute() && !args.is_empty());
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(workspace)
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("GH_REPO");
    command
}

/// `BLOCKED:` and why, when `said` shows that only a person can help.
fn needs_operator(doing: &str, said: &str) -> Option<String> {
    let lower = said.to_lowercase();
    let blocked = NEEDS_OPERATOR.iter().any(|sign| lower.contains(sign));
    blocked.then(|| format!("BLOCKED: {doing} was refused for want of access: {said}"))
}

/// Push `branch` to `origin` as it is, never forced. Pushing what `origin`
/// has already is a success, so a repeated delivery pushes nothing twice.
pub(super) async fn push(
    workspace: &Path,
    branch: &str,
) -> Result<Result<(), String>, ArchonError> {
    assert!(!branch.is_empty(), "a run has a branch");
    let remote = git(workspace, &["remote", "get-url", "origin"]);
    if !run(remote, "git remote get-url", None, GH_TIMEOUT)
        .await?
        .ok
    {
        return Ok(Err(
            "BLOCKED: the repository has no `origin` remote to push to; add one, or set the \
             repository's ship mode to `local`"
                .into(),
        ));
    }
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let args = ["push", "--porcelain", "--set-upstream", "origin", &refspec];
    let pushed = run(git(workspace, &args), "git push", None, PUSH_TIMEOUT).await?;
    if pushed.ok {
        return Ok(Ok(()));
    }
    let doing = format!("pushing `{branch}` to `origin`");
    let said = pushed.detail();
    match push_refusal(branch, &pushed).or_else(|| needs_operator(&doing, &said)) {
        Some(refusal) => Ok(Err(refusal)),
        None => Err(ArchonError::Unavailable(format!(
            "git push of `{branch}` failed: {said}"
        ))),
    }
}

/// Why `origin` will not take the branch, read from `git push --porcelain`.
fn push_refusal(branch: &str, pushed: &Ran) -> Option<String> {
    let line = pushed.stdout.lines().find(|line| line.starts_with('!'))?;
    let why = line.rsplit('\t').next().unwrap_or(line).trim();
    if why.contains("[remote rejected]") {
        return Some(format!(
            "BLOCKED: `origin` refused the push of `{branch}`: {why}. Its branch rules or your \
             access decide this"
        ));
    }
    Some(format!(
        "`origin` already has a `{branch}` that holds commits this run does not: {why}. Archon \
         never force-pushes; delete or rename the remote branch, then retry the item"
    ))
}

/// The open pull requests whose head is `branch`, as GitHub lists them.
async fn open_prs(
    binary: &Path,
    workspace: &Path,
    branch: &str,
) -> Result<Result<Vec<PullRequest>, String>, ArchonError> {
    let fields = "url,number,headRefName,state,baseRefName";
    let args = [
        "pr", "list", "--head", branch, "--state", "open", "--limit", "2", "--json", fields,
    ];
    let listed = run(
        gh(binary, workspace, &args),
        "GitHub PR lookup",
        None,
        GH_TIMEOUT,
    )
    .await?;
    if !listed.ok {
        let said = listed.detail();
        return match needs_operator("looking up the pull request", &said) {
            Some(blocked) => Ok(Err(blocked)),
            None => Err(ArchonError::Unavailable(format!(
                "GitHub PR lookup failed: {said}"
            ))),
        };
    }
    let prs: Vec<PullRequest> = serde_json::from_str(&listed.stdout)
        .map_err(|e| ArchonError::Protocol(format!("invalid GitHub PR lookup response: {e}")))?;
    for pr in &prs {
        let shaped = pr.head_ref_name == branch
            && pr.state == "OPEN"
            && pr.number != 0
            && pr.url.starts_with("https://")
            && pr.url.ends_with(&format!("/pull/{}", pr.number));
        if !shaped {
            return Err(ArchonError::Protocol(
                "GitHub returned a PR that does not match the branch, open state or URL".into(),
            ));
        }
    }
    Ok(Ok(prs))
}

/// The URL of the one open pull request of `branch`; `Err` when GitHub lists
/// none or several. What a pipeline's own ship agent claims is checked here.
pub(in crate::executor) async fn open_pr(
    binary: &Path,
    workspace: &Path,
    branch: &str,
) -> Result<String, ArchonError> {
    let prs = open_prs(binary, workspace, branch)
        .await?
        .map_err(ArchonError::Unavailable)?;
    let [pr] = prs.as_slice() else {
        return Err(ArchonError::Unavailable(format!(
            "expected one open PR, found {}",
            prs.len()
        )));
    };
    Ok(pr.url.clone())
}

/// The open pull request of `request.branch`, reused (moved onto
/// `request.base`) when GitHub lists one, else opened: never a second.
pub(super) async fn pull_request(
    binary: &Path,
    workspace: &Path,
    request: Request<'_>,
) -> Result<Result<String, String>, ArchonError> {
    assert!(!request.title.is_empty() && !request.body.is_empty());
    assert_ne!(
        request.branch, request.base,
        "a change is for another branch"
    );
    let found = match open_prs(binary, workspace, request.branch).await? {
        Ok(found) => found,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match found.as_slice() {
        [] => create(binary, workspace, request).await,
        [pr] => {
            let on = pr.base_ref_name.as_deref();
            if on.is_some_and(|base| base != request.base) {
                retarget(binary, workspace, &pr.url, request.base).await?;
            }
            Ok(Ok(pr.url.clone()))
        }
        several => Ok(Err(format!(
            "GitHub lists {} open pull requests for `{}`; close all but one, then retry the item",
            several.len(),
            request.branch
        ))),
    }
}

/// Open `request` and return its URL as GitHub then lists it. A refused
/// creation is no failure when the pull request is there all the same.
async fn create(
    binary: &Path,
    workspace: &Path,
    request: Request<'_>,
) -> Result<Result<String, String>, ArchonError> {
    let mut args = vec![
        "pr",
        "create",
        "--base",
        request.base,
        "--head",
        request.branch,
        "--title",
        request.title,
        "--body-file",
        "-",
    ];
    if request.draft {
        args.push("--draft");
    }
    let command = gh(binary, workspace, &args);
    let created = run(command, "gh pr create", Some(request.body), GH_TIMEOUT).await?;
    let listed = match open_prs(binary, workspace, request.branch).await? {
        Ok(listed) => listed,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let [pr] = listed.as_slice() {
        return Ok(Ok(pr.url.clone()));
    }
    let said = created.detail();
    if let Some(blocked) = needs_operator("opening the pull request", &said) {
        return Ok(Err(blocked));
    }
    let outcome = match created.ok {
        true => "succeeded",
        false => "failed",
    };
    Err(ArchonError::Unavailable(format!(
        "`gh pr create` {outcome}, and GitHub lists {} open pull request(s) for `{}`: {said}",
        listed.len(),
        request.branch
    )))
}

/// Point the pull request at `url` to `base`, the branch its change is for.
async fn retarget(
    binary: &Path,
    workspace: &Path,
    url: &str,
    base: &str,
) -> Result<(), ArchonError> {
    assert!(url.starts_with("https://") && !base.is_empty());
    let command = gh(binary, workspace, &["pr", "edit", url, "--base", base]);
    let edited = run(command, "gh pr edit", None, GH_TIMEOUT).await?;
    if edited.ok {
        return Ok(());
    }
    Err(ArchonError::Unavailable(format!(
        "moving {url} onto `{base}` failed: {}",
        edited.detail()
    )))
}

/// The merge methods a repository allows, as `gh repo view` reports them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeMethods {
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
}

/// Ask GitHub to merge the pull request at `url` once its checks and reviews
/// allow, by the first method the repository permits. `Err` says why not.
pub(super) async fn auto_merge(binary: &Path, workspace: &Path, url: &str) -> Result<(), String> {
    assert!(url.starts_with("https://"), "a verified pull request URL");
    let fields = "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed";
    let view = gh(binary, workspace, &["repo", "view", "--json", fields]);
    let viewed = run(view, "gh repo view", None, GH_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if !viewed.ok {
        return Err(viewed.detail());
    }
    let allowed: MergeMethods = serde_json::from_str(&viewed.stdout)
        .map_err(|e| format!("invalid `gh repo view` response: {e}"))?;
    let method = match allowed {
        MergeMethods {
            squash_merge_allowed: true,
            ..
        } => "--squash",
        MergeMethods {
            merge_commit_allowed: true,
            ..
        } => "--merge",
        MergeMethods {
            rebase_merge_allowed: true,
            ..
        } => "--rebase",
        _ => return Err("the repository allows no merge method".into()),
    };
    let merge = gh(binary, workspace, &["pr", "merge", url, "--auto", method]);
    let merged = run(merge, "gh pr merge", None, GH_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    match merged.ok {
        true => Ok(()),
        false => Err(merged.detail()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(stdout: &str, stderr: &str) -> Ran {
        Ran {
            ok: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn a_rejected_push_is_never_forced_and_says_what_holds_it() {
        let moved = ran(
            "To github.com:o/r.git\n!\trefs/heads/a/b:refs/heads/a/b\t[rejected] (fetch first)\nDone\n",
            "error: failed to push some refs",
        );
        let refusal = push_refusal("a/b", &moved).unwrap();
        assert!(refusal.contains("[rejected] (fetch first)"), "{refusal}");
        assert!(refusal.contains("never force-pushes") && !refusal.starts_with("BLOCKED:"));

        let ruled = ran(
            "!\trefs/heads/a/b:refs/heads/a/b\t[remote rejected] (protected branch hook declined)\n",
            "",
        );
        assert!(push_refusal("a/b", &ruled).unwrap().starts_with("BLOCKED:"));
        assert_eq!(
            push_refusal("a/b", &ran("", "fatal: unable to access")),
            None
        );
    }

    #[test]
    fn only_missing_access_blocks_for_the_operator() {
        let denied = "remote: Permission to o/r.git denied to someone.\nfatal: HTTP 403";
        let blocked = needs_operator("pushing `b` to `origin`", denied).unwrap();
        assert!(blocked.starts_with("BLOCKED: pushing `b` to `origin` was refused"));
        assert!(blocked.contains("denied to someone"), "{blocked}");
        let login = "To get started with GitHub CLI, please run:  gh auth login";
        assert!(needs_operator("opening the pull request", login).is_some());
        let network = "fatal: unable to access 'https://x/': Could not resolve host: x";
        assert_eq!(
            needs_operator("pushing", network),
            None,
            "worth another try"
        );
    }

    #[test]
    fn a_failure_is_told_in_the_tool_s_own_words() {
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
