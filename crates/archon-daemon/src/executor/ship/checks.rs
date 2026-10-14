//! What archond proves about a run worktree before it delivers anything
//! (ARCHITECTURE §8.7): it is the run's own worktree, on the run branch,
//! with no uncommitted change, holding the tree the pipeline verified, ahead
//! of its base, and every commit it adds carries the repository's identity.
//! Blocking git, none of it running a program the worktree's config names:
//! callers run it off the async runtime.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_pipeline::ShipRecord;

use crate::worktree::daemon_git::{Config, DaemonGit};
use crate::worktree::head;
use crate::worktree::shadow::Shadow;
use crate::worktree::team::{check_worktree, common_dir};

/// Paths or commits named in one refusal.
const MAX_LISTED: usize = 5;

/// The worktree a run ships from and what it must hold.
#[derive(Debug, Clone)]
pub(super) struct Target {
    /// Where the shadow the clean check reads through lives.
    pub data_dir: PathBuf,
    pub repo_path: PathBuf,
    pub workspace: PathBuf,
    pub branch: String,
    /// The branch the change is for: the stack parent's, else the repository's.
    pub base: String,
    /// The addresses a commit the run adds may carry, recorded at registration.
    pub allowed: Vec<String>,
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
    let commit = git(tree, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let dirty = changed_files(target, &commit)?;
    if !dirty.is_empty() {
        return Ok(Err(format!(
            "the run worktree has uncommitted changes, which would not ship: {}",
            listed(dirty.lines())
        )));
    }
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
    Ok(foreign_commits(tree, &base, &target.allowed)?.map(|()| Checked { commit }))
}

/// Git in the run worktree under the repository's config, running none of
/// the programs it names; its stdout.
fn git(tree: &Path, args: &[&str]) -> Result<String, ArchonError> {
    DaemonGit::sealed_in(tree).args(args.iter().copied()).text()
}

/// The tracked files of the run worktree that differ from `commit`, compared
/// through the shadow in an index of its own: no filter the worktree's config
/// names runs, and nothing in the worktree is written.
fn changed_files(target: &Target, commit: &str) -> Result<String, ArchonError> {
    assert!(crate::worktree::stack::is_commit_id(commit), "a commit id");
    let shadow = Shadow::of(&target.data_dir, &target.repo_path)?;
    let index = shadow.scratch_index();
    let compared = compare(&shadow, &target.workspace, &index, commit);
    match std::fs::remove_file(&index) {
        Ok(()) => compared,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => compared,
        Err(e) => compared.and(Err(ArchonError::io(
            format!("removing {}", index.display()),
            e,
        ))),
    }
}

fn compare(
    shadow: &Shadow,
    tree: &Path,
    index: &Path,
    commit: &str,
) -> Result<String, ArchonError> {
    let index = index
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", index.display())))?;
    let git = || -> Result<DaemonGit, ArchonError> {
        Ok(shadow
            .git(Some(tree), Config::Operator)?
            .env("GIT_INDEX_FILE", index))
    };
    git()?.args(["read-tree", commit]).text()?;
    git()?.args(["update-index", "-q", "--refresh"]).text()?;
    let changed = git()?.args(["diff-files", "--name-only"]).text()?;
    assert!(!changed.contains('\0'), "one path per line");
    Ok(changed)
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
fn foreign_commits(
    tree: &Path,
    base: &str,
    allowed: &[String],
) -> Result<Result<(), String>, ArchonError> {
    assert!(
        !allowed.is_empty(),
        "the daemon's own address is always allowed"
    );
    assert!(base.starts_with("refs/"), "a full ref name");
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
         (<{}>): {}. Commits must inherit the local git configuration; Archon records it at \
         `archon repo add`, and again at `archon repo set` after a deliberate change",
        foreign.len(),
        allowed[0],
        listed(foreign.iter().map(String::as_str))
    )))
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
