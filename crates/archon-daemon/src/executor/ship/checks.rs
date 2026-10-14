//! What archond proves about a run worktree before it delivers anything
//! (ARCHITECTURE §8.7): it is the run's own worktree, on the run branch,
//! with no uncommitted change, holding the tree the pipeline verified, ahead
//! of its base, and every commit it adds carries the repository's identity.
//! Blocking git: callers run it off the async runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::error::ArchonError;
use archon_pipeline::ShipRecord;

use crate::worktree::team::{check_worktree, common_dir};
use crate::worktree::{git, head};

/// The identity of the commits archond makes itself when git has none.
const DAEMON_EMAIL: &str = "archon@localhost";
/// Paths or commits named in one refusal.
const MAX_LISTED: usize = 5;
/// Set by a caller's shell, these would stand in for the repository's identity.
const IDENTITY_ENV: [&str; 5] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "EMAIL",
];

/// The worktree a run ships from and what it must hold.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub repo_path: PathBuf,
    pub workspace: PathBuf,
    pub branch: String,
    /// The branch the change is for: the stack parent's, else the repository's.
    pub base: String,
}

/// A worktree that may be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Checked {
    /// The run branch's head, which is what ships.
    pub commit: String,
}

/// Prove `target` holds what `record` names. The inner `Err` is a refusal no
/// retry changes, in words for the orchestrator; the outer one is git failing.
pub(super) fn check(
    target: &Target,
    record: &ShipRecord,
) -> Result<Result<Checked, String>, ArchonError> {
    assert!(target.workspace.is_absolute(), "run worktrees are absolute");
    assert!(!target.branch.is_empty() && !target.base.is_empty());
    let tree = &target.workspace;
    if let Err(refusal) = own_worktree(target)? {
        return Ok(Err(refusal));
    }
    let on = head(tree)?;
    if !on.is_branch(&target.branch) {
        return Ok(Err(format!(
            "the run worktree is on {on}, not on the run branch `{}`",
            target.branch
        )));
    }
    let dirty = git(tree, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Ok(Err(format!(
            "the run worktree has uncommitted changes, which would not ship: {}",
            listed(dirty.lines())
        )));
    }
    let commit = git(tree, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let found = git(tree, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if found != record.tree {
        return Ok(Err(format!(
            "the run branch holds tree {found} at commit {commit}, not the tree {} that was \
             verified at commit {}: it changed after the ship record was written",
            record.tree, record.commit
        )));
    }
    let Some(base) = base_ref(tree, &target.base)? else {
        return Ok(Err(format!(
            "the base branch `{}` exists neither in the repository nor on `origin`",
            target.base
        )));
    };
    let ahead = git(tree, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    if ahead == "0" {
        return Ok(Err(format!(
            "nothing to ship: `{}` has no commit beyond `{}`",
            target.branch, target.base
        )));
    }
    Ok(foreign_commits(tree, &base)?.map(|()| Checked { commit }))
}

/// Refuse a workspace that is not the worktree git registered for the run:
/// git run there could otherwise act on the operator's checkout.
fn own_worktree(target: &Target) -> Result<Result<(), String>, ArchonError> {
    let tree = &target.workspace;
    if !tree.is_dir() {
        return Ok(Err(format!("the run worktree {} is gone", tree.display())));
    }
    let common = common_dir(&target.repo_path)?;
    match check_worktree(tree, &common) {
        Ok(admin) => {
            assert!(admin.starts_with(&common), "a worktree's git directory");
            Ok(Ok(()))
        }
        Err(ArchonError::Invalid(why)) => Ok(Err(format!(
            "the run worktree cannot be shipped from: {why}"
        ))),
        Err(error) => Err(error),
    }
}

/// The ref `base` names: the local branch, else `origin`'s.
fn base_ref(tree: &Path, base: &str) -> Result<Option<String>, ArchonError> {
    assert!(!base.is_empty(), "a run has a base branch");
    for candidate in [
        format!("refs/heads/{base}"),
        format!("refs/remotes/origin/{base}"),
    ] {
        let listed = git(tree, &["for-each-ref", "--format=%(refname)", &candidate])?;
        if listed.lines().any(|name| name == candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Refuse a commit the run adds, one neither `base` nor a remote holds, that
/// was not made under the repository's git identity or the daemon's own.
fn foreign_commits(tree: &Path, base: &str) -> Result<Result<(), String>, ArchonError> {
    let allowed = allowed_emails(tree)?;
    assert!(allowed.iter().any(|email| email == DAEMON_EMAIL));
    let format = "--format=%H%x09%ae%x09%ce";
    let log = git(tree, &["log", format, "HEAD", "--not", base, "--remotes"])?;
    let mut foreign = Vec::new();
    for line in log.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [commit, author, committer] = fields.as_slice() else {
            return Err(ArchonError::Protocol(format!(
                "git log printed {line:?}, not a commit and its two e-mail addresses"
            )));
        };
        let known = |email: &&str| allowed.iter().any(|a| a == *email);
        if !known(author) || !known(committer) {
            foreign.push(format!(
                "{} (author <{author}>, committer <{committer}>)",
                &commit[..commit.len().min(12)]
            ));
        }
    }
    if foreign.is_empty() {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "{} commit(s) on the run branch were not made under this repository's git identity \
         (<{}>): {}. Commits must inherit the local git configuration",
        foreign.len(),
        allowed[0],
        listed(foreign.iter().map(String::as_str))
    )))
}

/// The e-mail addresses a commit made in `tree` under its git configuration
/// carries, then the daemon's fallback.
fn allowed_emails(tree: &Path) -> Result<Vec<String>, ArchonError> {
    let mut allowed = Vec::new();
    for ident in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        let mut command = Command::new("git");
        command.arg("-C").arg(tree).args(["var", ident]);
        for name in IDENTITY_ENV {
            command.env_remove(name);
        }
        let output = command
            .output()
            .map_err(|e| ArchonError::io(format!("running git in {}", tree.display()), e))?;
        // `git var` fails exactly when git has no identity to commit under.
        if !output.status.success() {
            continue;
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        let email = printed
            .split_once('<')
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(email, _)| email.to_owned());
        let Some(email) = email else {
            return Err(ArchonError::Protocol(format!(
                "git var {ident} printed no e-mail address: {printed:?}"
            )));
        };
        if !allowed.contains(&email) {
            allowed.push(email);
        }
    }
    if !allowed.iter().any(|email| email == DAEMON_EMAIL) {
        allowed.push(DAEMON_EMAIL.to_owned());
    }
    Ok(allowed)
}

/// The first few of `items` on one line, saying how many more there are.
fn listed<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.map(str::trim).collect();
    assert!(!items.is_empty(), "only a non-empty list is shown");
    let mut text = items[..items.len().min(MAX_LISTED)].join("; ");
    if items.len() > MAX_LISTED {
        text.push_str(&format!("; and {} more", items.len() - MAX_LISTED));
    }
    text
}
