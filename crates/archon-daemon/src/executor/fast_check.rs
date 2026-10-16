//! A repository's fast check for one change (issue #82, task AR-I2): the
//! command for the units the change affects, which stage agents are told,
//! and the bounded run the ship gate makes of it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::repo::affected::{self, Affected};
use archon_core::repo::{Repo, RepoTiers};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::worktree::daemon_git::DaemonGit;

/// The longest a ship gate's check may run before it counts as failed.
pub(crate) const RUN_LIMIT: Duration = Duration::from_secs(30 * 60);
/// The end of each output stream a run keeps.
const KEPT_BYTES: usize = 16 * 1024;
/// The last lines of each stream a refusal quotes.
const SHOWN_LINES: usize = 40;
/// At most this much of each stream's last lines.
const SHOWN_BYTES: usize = 4096;
/// How long the output of a killed check may take to close.
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// The fast check archond may run itself in `repo`: the registry's, else the
/// committed file's while the operator's confirmation names it word for
/// word. Never one read from an item's worktree.
pub(crate) fn trusted(repo: &Repo) -> Result<Option<String>, ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are absolute");
    if let Some(fast) = &repo.tiers.checks.fast {
        return Ok(Some(fast.clone()));
    }
    let Some(confirmed) = &repo.confirmed_fast_check else {
        return Ok(None);
    };
    let committed = RepoTiers::load(&repo.path)?.checks.fast;
    if committed.as_ref() != Some(confirmed) {
        tracing::warn!(
            repo = %repo.name,
            ?committed,
            "the committed fast check differs from the confirmed one; the ship gate skips it"
        );
        return Ok(None);
    }
    Ok(committed)
}

/// What the change in the work tree `tree` affects: the files its `HEAD`
/// changes since it forked from `base`, read with git running no program
/// the tree's config names.
pub(crate) fn affected_by(tree: &Path, base: &str) -> Result<Affected, ArchonError> {
    assert!(tree.is_absolute(), "run worktrees are absolute");
    assert!(!base.is_empty(), "a change has a base branch");
    let Some(base) = base_ref(tree, base)? else {
        return Err(ArchonError::Invalid(format!(
            "the base branch `{base}` exists neither locally nor on `origin`"
        )));
    };
    let git = || DaemonGit::sealed_in(tree);
    let fork = git().args(["merge-base", &base, "HEAD"]).text()?;
    assert!(crate::worktree::stack::is_commit_id(&fork), "a merge base");
    let args = ["diff", "-z", "--name-only", "--no-renames", &fork, "HEAD"];
    let ran = git().args(args).output()?;
    if !ran.status.success() {
        return Err(ran.failure());
    }
    let listed = std::str::from_utf8(&ran.stdout)
        .map_err(|_| ArchonError::Invalid("git diff named a non-UTF-8 path".into()))?;
    let changed: Vec<String> = listed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    affected::affected(tree, &changed)
}

/// The ref `base` names: the local branch, else `origin`'s.
fn base_ref(tree: &Path, base: &str) -> Result<Option<String>, ArchonError> {
    for candidate in [
        format!("refs/heads/{base}"),
        format!("refs/remotes/origin/{base}"),
    ] {
        let args = ["for-each-ref", "--format=%(refname)", &candidate];
        let listed = DaemonGit::sealed_in(tree).args(args).text()?;
        if listed.lines().any(|name| name == candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// `template` for what `affected` holds; `None` when it selects units and
/// none is affected, so there is nothing to run.
pub(crate) fn command_for(
    template: &str,
    affected: &Affected,
) -> Result<Option<String>, ArchonError> {
    affected::expand(template, &affected.units)
        .map_err(|why| ArchonError::Invalid(format!("fast check {template:?}: {why}")))
}

/// How a check run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    Passed,
    Failed(String),
}

/// The end of what a run printed, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ran {
    pub ended: Ended,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The last lines of both streams, for a refusal.
    pub(crate) fn shown(&self) -> String {
        let tail = |text: &str| {
            let lines: Vec<&str> = text.lines().collect();
            let last = lines[lines.len().saturating_sub(SHOWN_LINES)..].join("\n");
            let mut cut = last.len().saturating_sub(SHOWN_BYTES);
            while !last.is_char_boundary(cut) {
                cut += 1;
            }
            last[cut..].to_owned()
        };
        format!(
            "stdout (last lines):\n{}\nstderr (last lines):\n{}",
            tail(&self.stdout),
            tail(&self.stderr)
        )
    }
}

/// Kills the check's process group when dropped: on a timeout, and when the
/// task running it is aborted because its run ended.
struct Group(Option<rustix::process::Pid>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // Gone already is the usual case: the check exited.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

/// Run `command` with `sh -c` in `tree` with `envs` added, in its own process
/// group, for at most `limit`, keeping the end of its output.
pub(crate) async fn run(
    command: &str,
    tree: &Path,
    envs: &[(&str, &Path)],
    limit: Duration,
) -> Result<Ran, ArchonError> {
    assert!(tree.is_absolute(), "checks run in absolute work trees");
    assert!(!limit.is_zero(), "a time limit is positive");
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir(tree)
        .envs(envs.iter().map(|(key, value)| (*key, *value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ArchonError::io(format!("running the fast check `{command}`"), e))?;
    let group = Group(
        child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
    );
    let stdout = tokio::spawn(kept(child.stdout.take().expect("piped")));
    let stderr = tokio::spawn(kept(child.stderr.take().expect("piped")));
    let ended = match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => {
            let status = status.map_err(|e| ArchonError::io("waiting for the fast check", e))?;
            match status.success() {
                true => Ended::Passed,
                false => Ended::Failed(format!("it ended with {status}")),
            }
        }
        Err(_) => Ended::Failed(format!(
            "it did not finish within {} seconds",
            limit.as_secs()
        )),
    };
    // Whatever the check left running would hold its output open.
    drop(group);
    let stdout = drained(stdout).await?;
    let stderr = drained(stderr).await?;
    Ok(Ran {
        ended,
        stdout,
        stderr,
    })
}

/// The end of `stream`, at most [`KEPT_BYTES`] of it.
async fn kept(mut stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::with_capacity(KEPT_BYTES);
    let mut chunk = vec![0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > 2 * KEPT_BYTES {
            kept.drain(..kept.len() - KEPT_BYTES);
        }
    }
    let start = kept.len().saturating_sub(KEPT_BYTES);
    kept.drain(..start);
    assert!(kept.len() <= KEPT_BYTES, "the kept output is bounded");
    Ok(kept)
}

/// What a reader kept, once its stream closed; a stream some escaped process
/// keeps open is cut off after [`DRAIN_LIMIT`], and says so.
async fn drained(
    reader: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<String, ArchonError> {
    let abort = reader.abort_handle();
    let Ok(joined) = tokio::time::timeout(DRAIN_LIMIT, reader).await else {
        abort.abort();
        return Ok("(output still open after the check ended; not read)".into());
    };
    let read =
        joined.map_err(|e| ArchonError::io("fast check output", std::io::Error::other(e)))?;
    let bytes = read.map_err(|e| ArchonError::io("reading the fast check's output", e))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
pub(crate) mod tests;
