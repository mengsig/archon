//! Team stage worktrees (ARCHITECTURE §13.1): one worktree per role beside
//! the run worktree, each on its own branch, and the merge of those
//! branches onto the run branch once every role passed.
//!
//! Roles control their worktrees, and work stages the run worktree, so
//! the daemon runs git in one only once `identity::check_worktree`
//! proved it is the worktree git registered there, and never with
//! repository hooks. Git that enters submodules runs only once
//! `submodules::scan` found none git must not enter; a role's worktree
//! holding one is moved aside (`aside`), never entered, as is one whose
//! `HEAD` names no commit. The git directory of one deleted by hand is
//! kept (`orphan`).

mod aside;
mod git_dirs;
pub(crate) mod identity;
mod leftovers;
mod orphan;
mod submodules;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_pipeline::team::{
    MergeOutcome, MergePlan, RoleBranch, team_root, worker_branch_role, worker_dir,
};

use super::{Head, branch_exists, git_failure, head, path_arg};
use aside::set_aside;
pub use aside::{GIT_SET_ASIDE_DIR, SET_ASIDE_DIR, SetAside};
pub(crate) use identity::{check_worktree, common_dir};
use leftovers::{commit_leftovers, keep_detached_head, operation_in_progress, quit_operations};
pub use orphan::OrphanedGitDir;
use orphan::set_aside_orphan;
use submodules::scan;

/// Files a role left uncommitted that the daemon committed for it. The
/// operator is told, as they may hold what should never ship: a `.env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedWork {
    pub worktree: PathBuf,
    /// What they were committed on: a branch, or `a detached HEAD`.
    pub on: String,
    pub commit: String,
    pub files: Vec<String>,
}

impl SavedWork {
    /// One line for the run's progress.
    pub fn describe(&self) -> String {
        let files: Vec<&str> = self.files.iter().map(String::as_str).collect();
        format!(
            "Committed {} file(s) a role left uncommitted in `{}` on {} as {}: {}",
            files.len(),
            self.worktree.display(),
            self.on,
            self.commit,
            listed(&files)
        )
    }
}

/// What merging a team came to, with what the daemon committed for roles
/// that left work uncommitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMerge {
    pub outcome: MergeOutcome,
    pub saved: Vec<SavedWork>,
}

/// What [`ensure_worker`] did with what an earlier attempt or run left in
/// a role's worktree, for the run's progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeptWork {
    /// Committed where it was left.
    Committed(SavedWork),
    /// Moved aside unentered, as it held submodules git must not enter or
    /// its `HEAD` named no commit.
    SetAside(SetAside),
    /// The git directory of a worktree deleted by hand, moved aside.
    Orphaned(OrphanedGitDir),
}

impl KeptWork {
    /// One line for the run's progress.
    pub fn describe(&self) -> String {
        match self {
            KeptWork::Committed(saved) => saved.describe(),
            KeptWork::SetAside(aside) => aside.describe(),
            KeptWork::Orphaned(orphan) => orphan.describe(),
        }
    }
}

/// How [`leave_worktree`] moved a worktree off an earlier branch.
enum Left {
    /// Switched in place, with what was committed for the role first.
    Switched(Option<SavedWork>),
    SetAside(SetAside),
}

/// Make `path` a worktree with `branch` checked out, forking `branch` from
/// the run branch `start` when it does not exist yet. A retried agent
/// reuses the worktree as is. A new attempt, or a new run of the item,
/// forks a new branch, which needs the run worktree `run_tree` committed.
/// An existing worktree moves to the new branch, or is set aside when it
/// holds submodules git must not enter or its `HEAD` names no commit; the
/// git directory of one deleted by hand is set aside. An existing branch
/// is reused only when it holds the run branch. Returns what was done
/// with what an earlier role left there.
pub fn ensure_worker(
    repo_path: &Path,
    run_tree: &Path,
    path: &Path,
    branch: &str,
    start: &str,
) -> Result<Option<KeptWork>, ArchonError> {
    assert!(repo_path.is_absolute(), "repository paths are canonical");
    assert!(path.is_absolute(), "worker worktrees are absolute");
    assert!(
        !path.starts_with(run_tree),
        "workers are never nested in the run worktree"
    );
    check_worker_branch(branch, start)?;
    let common = common_dir(repo_path)?;
    let fork = !branch_exists(repo_path, branch)?;
    if fork {
        check_run_tree_committed(run_tree, &common, start)?;
    }
    let mut kept = None;
    if present(path)? {
        let admin = check_worktree(path, &common)?;
        let checked_out = head(path)?;
        if checked_out.is_branch(branch) && !fork {
            return Ok(None);
        }
        check_reused_branch(repo_path, branch, start, fork)?;
        match leave_worktree(&common, &admin, path, &checked_out, branch, start, fork)? {
            Left::Switched(saved) => return Ok(saved.map(KeptWork::Committed)),
            Left::SetAside(aside) => kept = Some(KeptWork::SetAside(aside)),
        }
    } else {
        check_reused_branch(repo_path, branch, start, fork)?;
    }
    let orphan = add_worker(repo_path, &common, path, branch, start, fork)
        .map_err(|error| after_kept(error, kept.as_ref()))?;
    assert!(
        kept.is_none() || orphan.is_none(),
        "a worktree set aside is registered nowhere"
    );
    Ok(kept.or(orphan.map(KeptWork::Orphaned)))
}

/// `error`, prefixed with what was done with an earlier role's worktree
/// before it, so that the operator still learns where that went. The
/// variant is kept, so callers still tell a transient failure apart.
fn after_kept(error: ArchonError, kept: Option<&KeptWork>) -> ArchonError {
    let Some(kept) = kept else {
        return error;
    };
    let done = format!("{}; then adding the fresh worktree failed", kept.describe());
    prefixed(error, &done)
}

/// `error` of the same variant, its text led by `prefix`.
fn prefixed(error: ArchonError, prefix: &str) -> ArchonError {
    assert!(!prefix.is_empty(), "a prefix says something");
    let with = |why: String| format!("{prefix}: {why}");
    match error {
        ArchonError::Io { context, cause } => ArchonError::Io {
            context: with(context),
            cause,
        },
        ArchonError::Config(why) => ArchonError::Config(with(why)),
        ArchonError::Invalid(why) => ArchonError::Invalid(with(why)),
        ArchonError::NotFound(why) => ArchonError::NotFound(with(why)),
        ArchonError::InvalidTransition(why) => ArchonError::InvalidTransition(with(why)),
        ArchonError::Store(why) => ArchonError::Store(with(why)),
        ArchonError::Corrupt(why) => ArchonError::Corrupt(with(why)),
        ArchonError::DiskFull(why) => ArchonError::DiskFull(with(why)),
        ArchonError::Aether(why) => ArchonError::Aether(with(why)),
        ArchonError::Pipeline(why) => ArchonError::Pipeline(with(why)),
        ArchonError::Policy(why) => ArchonError::Policy(with(why)),
        ArchonError::Unavailable(why) => ArchonError::Unavailable(with(why)),
        ArchonError::Forbidden(why) => ArchonError::Forbidden(with(why)),
        ArchonError::Recovering(why) => ArchonError::Recovering(with(why)),
        ArchonError::Protocol(why) => ArchonError::Protocol(with(why)),
    }
}

/// Refuse to reuse an existing worker `branch` (not `fork`) that does not
/// hold the run branch `start`: it is no fork of this run's.
fn check_reused_branch(
    repo_path: &Path,
    branch: &str,
    start: &str,
    fork: bool,
) -> Result<(), ArchonError> {
    if fork {
        return Ok(());
    }
    let (from, to) = (
        format!("refs/heads/{start}"),
        format!("refs/heads/{branch}"),
    );
    if is_ancestor(repo_path, &from, &to)? {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "worker branch {branch:?} exists but does not hold the run branch {start:?}, so it \
         is not this run's; rename or delete it"
    )))
}

/// Move the worker worktree `path`, proven registered with `admin` and
/// with `checked_out` rather than `branch`, onto `branch`, or set it aside
/// when that cannot be done without running git that must not run.
fn leave_worktree(
    common: &Path,
    admin: &Path,
    path: &Path,
    checked_out: &Head,
    branch: &str,
    start: &str,
    fork: bool,
) -> Result<Left, ArchonError> {
    assert!(
        !checked_out.is_branch(branch) || fork,
        "a retried agent stays"
    );
    let born = head_born(path, checked_out)?;
    let submodules = scan(path, common)?;
    if born && submodules.foreign.is_empty() {
        let saved = switch_worker(path, checked_out, branch, start, fork)?;
        return Ok(Left::Switched(saved));
    }
    let aside = set_aside(common, admin, path, checked_out, submodules, !born, false)?;
    debug_assert_eq!(aside.unborn, !born);
    Ok(Left::SetAside(aside))
}

/// Move the item's run worktree `path` aside for a fresh retry of the item,
/// committing and switching nothing, so the next run adds a fresh one from
/// the base branch. Its files, index and commits are all kept.
pub fn set_aside_for_fresh_retry(repo_path: &Path, path: &Path) -> Result<SetAside, ArchonError> {
    assert!(repo_path.is_absolute(), "repository paths are canonical");
    assert!(present(path)?, "only an existing worktree is set aside");
    let common = common_dir(repo_path)?;
    let admin = check_worktree(path, &common)?;
    let checked_out = head(path)?;
    let born = head_born(path, &checked_out)?;
    let submodules = scan(path, &common)?;
    let aside = set_aside(&common, &admin, path, &checked_out, submodules, !born, true)?;
    assert!(!present(path)?, "the run worktree was moved");
    Ok(aside)
}

/// Whether the reference `HEAD` names in the worktree `path` has a commit;
/// a detached one always has.
fn head_born(path: &Path, checked_out: &Head) -> Result<bool, ArchonError> {
    if *checked_out == Head::Detached {
        return Ok(true);
    }
    let args = ["rev-parse", "-q", "--verify", "HEAD^{commit}"];
    let output = git_output(path, &args)?;
    // `rev-parse -q --verify` exits 1, silently, when there is no commit.
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(path, &args, &output)),
    }
}

/// Refuse a `branch` that is not a worker branch of the run branch `start`.
fn check_worker_branch(branch: &str, start: &str) -> Result<(), ArchonError> {
    for name in [branch, start] {
        if name.is_empty() || name.starts_with('-') {
            return Err(ArchonError::Invalid(format!(
                "{name:?} is not a branch name"
            )));
        }
    }
    if worker_branch_role(start, branch).is_none() {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is not a worker branch of the run branch {start:?}"
        )));
    }
    Ok(())
}

/// Add the worktree `path` on `branch`, first forking it from the run
/// branch `start` when `fork`. Nothing is at `path`; the git directory of
/// a worktree deleted there by hand is set aside first, and returned.
fn add_worker(
    repo_path: &Path,
    common: &Path,
    path: &Path,
    branch: &str,
    start: &str,
    fork: bool,
) -> Result<Option<OrphanedGitDir>, ArchonError> {
    let parent = path.parent().expect("worker paths have a parent");
    std::fs::create_dir_all(parent)
        .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
    let orphan = set_aside_orphan(repo_path, common, path)?;
    let path_text = path_arg(path)?;
    // By its full name: a tag named like the run branch is never forked.
    let start = format!("refs/heads/{start}");
    let args: Vec<&str> = match fork {
        false => vec!["worktree", "add", path_text, branch],
        true => vec!["worktree", "add", "-b", branch, path_text, &start],
    };
    git(repo_path, &args)?;
    debug_assert!(head(path)?.is_branch(branch), "git added the worker");
    Ok(orphan)
}

/// Roles fork from the run branch's last commit, so uncommitted changes to
/// tracked files there would be invisible to them and would block the
/// merge. Refusing the fork fails the slot, which `on_fail` can fix.
fn check_run_tree_committed(
    run_tree: &Path,
    common: &Path,
    start: &str,
) -> Result<(), ArchonError> {
    assert!(run_tree.is_absolute(), "the run worktree is absolute");
    check_tree(run_tree, common)?;
    let checked_out = head(run_tree)?;
    if !checked_out.is_branch(start) {
        return Err(ArchonError::Invalid(format!(
            "run worktree {} has {checked_out} checked out, not the run branch {start:?}",
            run_tree.display()
        )));
    }
    let changed = nul_paths(run_tree, &["diff", "--name-only", "HEAD"])?;
    if changed.is_empty() {
        return Ok(());
    }
    let files: Vec<&str> = changed.iter().map(String::as_str).collect();
    Err(ArchonError::Invalid(format!(
        "the run worktree {} has uncommitted changes to {}; roles fork from the last \
         commit on the run branch {start:?}, so commit them there (or discard them) first",
        run_tree.display(),
        listed(&files)
    )))
}

/// A worker worktree, proven registered and free of submodules git must
/// not enter, with `checked_out` rather than `branch`: move it from the
/// branch of an earlier attempt or run onto `branch`. What was left there
/// is saved first: an operation in progress is ended without touching any
/// file, leftovers are committed, and a detached `HEAD` is kept under
/// `refs/archon/saved/`.
fn switch_worker(
    path: &Path,
    checked_out: &Head,
    branch: &str,
    start: &str,
    fork: bool,
) -> Result<Option<SavedWork>, ArchonError> {
    assert!(
        !checked_out.is_branch(branch),
        "a worktree on its branch stays as it is"
    );
    quit_operations(path)?;
    let left_on = leave_foreign_branch(path, checked_out, branch, start)?;
    let message = format!("archon: save uncommitted work left on {left_on}");
    let leftovers = commit_leftovers(path, &left_on, &message)?;
    if !leftovers.kept.is_empty() {
        tracing::warn!(worktree = %path.display(), paths = ?leftovers.kept, "left in place: git cannot commit it on a branch");
    }
    if head(path)? == Head::Detached {
        keep_detached_head(path)?;
    }
    let start = format!("refs/heads/{start}");
    match fork {
        false => git(path, &["switch", branch])?,
        true => git(path, &["switch", "-c", branch, &start])?,
    };
    let now = head(path)?;
    if !now.is_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "{} has {now} checked out after switching it to {branch:?}",
            path.display()
        )));
    }
    Ok(leftovers.saved)
}

/// Where leftovers in the worktree `path`, which has `checked_out`, are
/// committed: on a branch of the role `branch` belongs to, or on a
/// detached `HEAD`. Any other branch (the operator's `main`, another
/// role's) or reference never moves: `HEAD` is detached from it, files and
/// index kept.
fn leave_foreign_branch(
    path: &Path,
    checked_out: &Head,
    branch: &str,
    start: &str,
) -> Result<String, ArchonError> {
    let role = worker_branch_role(start, branch);
    assert!(role.is_some(), "ensure_worker checked the worker branch");
    let name = match checked_out {
        Head::Detached => return Ok("a detached HEAD".to_owned()),
        Head::Branch(name) if worker_branch_role(start, name) == role => {
            return Ok(name.clone());
        }
        Head::Branch(name) | Head::Other(name) => name,
    };
    let commit = git(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let reason = format!("archon: leave {name}, not a branch of this role");
    git(
        path,
        &["update-ref", "--no-deref", "-m", &reason, "HEAD", &commit],
    )?;
    let now = head(path)?;
    if now != Head::Detached {
        return Err(ArchonError::Invalid(format!(
            "{} has {now} checked out after detaching its HEAD",
            path.display()
        )));
    }
    tracing::warn!(worktree = %path.display(), branch = name, "detached a role's worktree from a branch not its own");
    Ok(format!("a detached HEAD, off `{name}`"))
}

/// Git options for a commit the daemon makes itself: a fallback identity
/// when git has none. Hooks are off in every command ([`git`]).
fn daemon_commit_options(tree: &Path) -> Result<Vec<&'static str>, ArchonError> {
    let mut options = Vec::new();
    for ident in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        // `git var` fails exactly when git would refuse to commit for want
        // of an identity.
        if !git_output(tree, &["var", ident])?.status.success() {
            options.extend([
                "-c",
                "user.name=Archon",
                "-c",
                "user.email=archon@localhost",
            ]);
            break;
        }
    }
    debug_assert!(options.is_empty() || options.len() == 4);
    Ok(options)
}

/// The first few of `files`, for a message.
fn listed(files: &[&str]) -> String {
    const SHOWN: usize = 20;
    assert!(!files.is_empty(), "only a non-empty list is shown");
    let mut text = files
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > SHOWN {
        text.push_str(&format!(" and {} more", files.len() - SHOWN));
    }
    text
}

/// Merge every role's branch onto the run branch checked out in `run_tree`,
/// a worktree of the repository at `repo_path`, `--no-ff` and in roles
/// order. A branch already on the run branch is skipped, so a merge
/// interrupted by a crash resumes where it stopped. Each role's worktree
/// is settled first: its leftovers committed, or the merge refused when it
/// is off its branch, mid-operation, or holds what git cannot commit. A
/// conflict is aborted and reported; any other git failure is an error.
pub fn merge_team(
    repo_path: &Path,
    run_tree: &Path,
    plan: &MergePlan,
) -> Result<TeamMerge, ArchonError> {
    assert!(repo_path.is_absolute(), "repository paths are canonical");
    assert!(!plan.roles.is_empty(), "a team has roles to merge");
    let common = common_dir(repo_path)?;
    ready_run_tree(run_tree, &common, &plan.into)?;
    let root = team_root(run_tree).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "{} is not the run worktree of a team run",
            run_tree.display()
        ))
    })?;
    let mut saved = Vec::new();
    for role in &plan.roles {
        let worker = root.join(worker_dir(&plan.stage_id, &role.role));
        let settled = settle_worker(&worker, &common, role)?;
        saved.extend(settled.saved);
        if let Some(reason) = settled.refusal {
            let role = role.role.clone();
            let outcome = MergeOutcome::Refused { role, reason };
            return Ok(TeamMerge { outcome, saved });
        }
    }
    for role in &plan.roles {
        if let Some(outcome) = merge_role(run_tree, role)? {
            return Ok(TeamMerge { outcome, saved });
        }
    }
    debug_assert!(!merge_in_progress(run_tree)?);
    let outcome = MergeOutcome::Merged;
    Ok(TeamMerge { outcome, saved })
}

/// Delete each role's branch of a merged team once it is on the run
/// branch checked out in `run_tree`, so shipping leaves none behind. A
/// role's worktree still on its branch is detached first; the worktree
/// stays for the next attempt, which forks a new branch. A branch not
/// merged is an error, never deleted. Returns the branches deleted.
pub fn delete_merged_branches(
    repo_path: &Path,
    run_tree: &Path,
    plan: &MergePlan,
) -> Result<Vec<String>, ArchonError> {
    assert!(repo_path.is_absolute(), "repository paths are canonical");
    assert!(!plan.roles.is_empty(), "a team has roles to clean up");
    let common = common_dir(repo_path)?;
    check_tree(run_tree, &common)?;
    let root = team_root(run_tree).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "{} is not the run worktree of a team run",
            run_tree.display()
        ))
    })?;
    let mut deleted = Vec::new();
    for role in &plan.roles {
        let worker = root.join(worker_dir(&plan.stage_id, &role.role));
        if delete_merged_branch(run_tree, &worker, &common, &role.branch)? {
            deleted.push(role.branch.clone());
        }
    }
    Ok(deleted)
}

/// Delete `branch` if it exists, after proving it is on `run_tree`'s
/// `HEAD` and detaching `worker` from it.
fn delete_merged_branch(
    run_tree: &Path,
    worker: &Path,
    common: &Path,
    branch: &str,
) -> Result<bool, ArchonError> {
    assert!(
        !branch.is_empty() && !branch.starts_with('-'),
        "worker branches are derived from a valid run branch"
    );
    if !branch_exists(run_tree, branch)? {
        return Ok(false);
    }
    let reference = format!("refs/heads/{branch}");
    if !is_ancestor(run_tree, &reference, "HEAD")? {
        return Err(ArchonError::Invalid(format!(
            "worker branch {branch:?} is not on the run branch, so it is kept"
        )));
    }
    if present(worker)? {
        check_worktree(worker, common)?;
        if head(worker)?.is_branch(branch) {
            let commit = git(worker, &["rev-parse", "--verify", "HEAD^{commit}"])?;
            let reason = format!("archon: leave {branch}, merged into the run branch");
            git(
                worker,
                &["update-ref", "--no-deref", "-m", &reason, "HEAD", &commit],
            )?;
        }
    }
    git(run_tree, &["update-ref", "-d", &reference])?;
    debug_assert!(!branch_exists(run_tree, branch)?);
    Ok(true)
}

/// The run worktree `run_tree`, proven a worktree of the repository whose
/// common git directory is `common`, on the run branch `into` with no
/// merge left by a crash and no uncommitted change to a tracked file.
fn ready_run_tree(run_tree: &Path, common: &Path, into: &str) -> Result<(), ArchonError> {
    assert!(run_tree.is_absolute(), "the run worktree is absolute");
    check_tree(run_tree, common)?;
    let checked_out = head(run_tree)?;
    if !checked_out.is_branch(into) {
        return Err(ArchonError::Invalid(format!(
            "run worktree {} has {checked_out} checked out, not the run branch {into:?}",
            run_tree.display()
        )));
    }
    if merge_in_progress(run_tree)? {
        tracing::warn!(tree = %run_tree.display(), "aborting a merge left by a crash");
        git(run_tree, &["merge", "--abort"])?;
    }
    if has_tracked_changes(run_tree)? {
        return Err(ArchonError::Invalid(format!(
            "run worktree {} has uncommitted changes; the team's branches are not merged over them",
            run_tree.display()
        )));
    }
    Ok(())
}

/// How [`settle_worker`] left a role's worktree.
struct Settled {
    saved: Option<SavedWork>,
    /// Why its branch is not merged; `None` when it is ready.
    refusal: Option<String>,
}

/// Put everything `role` did in its worktree `path` on its branch before
/// the branch is merged: commit what it left uncommitted. A worktree off
/// its branch, mid-operation, holding what git cannot commit there, or
/// holding submodules git must not enter is refused, as its work may be
/// elsewhere.
fn settle_worker(path: &Path, common: &Path, role: &RoleBranch) -> Result<Settled, ArchonError> {
    assert!(path.is_absolute(), "worker worktrees are absolute");
    if !present(path)? {
        tracing::warn!(worktree = %path.display(), "a role's worktree is gone; merging its branch");
        return Ok(Settled {
            saved: None,
            refusal: None,
        });
    }
    check_worktree(path, common)?;
    if let Some(reason) = unsettled(path, common, role)? {
        return Ok(Settled {
            saved: None,
            refusal: Some(reason),
        });
    }
    let message = format!(
        "archon: commit the work role {} left uncommitted",
        role.role
    );
    let leftovers = commit_leftovers(path, &role.branch, &message)?;
    debug_assert!(head(path)?.is_branch(&role.branch));
    let refusal = (!leftovers.kept.is_empty()).then(|| {
        let files: Vec<&str> = leftovers.kept.iter().map(String::as_str).collect();
        format!(
            "its worktree `{}` holds what git cannot commit on its branch, so it would not \
             be merged: {}. Remove nested git repositories, and commit changes inside a \
             submodule in the submodule itself",
            path.display(),
            listed(&files)
        )
    });
    Ok(Settled {
        saved: leftovers.saved,
        refusal,
    })
}

/// Why `role`'s worktree `path` cannot be settled as it is: holding
/// submodules git must not enter, which is checked before any git that
/// would, off its branch, with unmerged files, or mid-operation.
fn unsettled(path: &Path, common: &Path, role: &RoleBranch) -> Result<Option<String>, ArchonError> {
    assert!(!role.branch.is_empty(), "roles have branches");
    let foreign = scan(path, common)?.foreign;
    if !foreign.is_empty() {
        let files: Vec<&str> = foreign.iter().map(String::as_str).collect();
        return Ok(Some(format!(
            "its worktree `{}` holds submodules whose git directory the repository does \
             not keep, so the daemon runs no git that enters them: {}. The next attempt \
             starts in a fresh worktree, where the role must add submodules only with \
             `git submodule add`",
            path.display(),
            listed(&files)
        )));
    }
    let checked_out = head(path)?;
    if !checked_out.is_branch(&role.branch) {
        return Ok(Some(format!(
            "its worktree `{}` has {checked_out} checked out, not its branch `{}`, \
             so work committed elsewhere would be left out",
            path.display(),
            role.branch
        )));
    }
    let unmerged = unmerged_paths(path)?;
    if !unmerged.is_empty() {
        let files: Vec<&str> = unmerged.iter().map(String::as_str).collect();
        return Ok(Some(format!(
            "its worktree `{}` has unmerged files: {}",
            path.display(),
            listed(&files)
        )));
    }
    let operation = operation_in_progress(path)?;
    Ok(operation.map(|operation| {
        format!(
            "its worktree `{}` holds an unfinished {operation}",
            path.display()
        )
    }))
}

/// Merge one role's branch; `Some` is the conflict that stopped it.
fn merge_role(run_tree: &Path, role: &RoleBranch) -> Result<Option<MergeOutcome>, ArchonError> {
    assert!(
        !role.branch.is_empty() && !role.branch.starts_with('-'),
        "worker branches are derived from a valid run branch"
    );
    if !branch_exists(run_tree, &role.branch)? {
        return Err(ArchonError::Invalid(format!(
            "worker branch {:?} of role {:?} does not exist",
            role.branch, role.role
        )));
    }
    let reference = format!("refs/heads/{}", role.branch);
    if is_ancestor(run_tree, &reference, "HEAD")? {
        return Ok(None);
    }
    let in_the_way = untracked_in_the_way(run_tree, &reference)?;
    if !in_the_way.is_empty() {
        return Ok(Some(MergeOutcome::Conflict {
            role: role.role.clone(),
            files: in_the_way,
        }));
    }
    let message = format!("Merge role {} from {}", role.role, role.branch);
    let mut args = daemon_commit_options(run_tree)?;
    args.extend(["merge", "--no-ff", "--no-edit", "-m", &message, &reference]);
    let output = git_output(run_tree, &args)?;
    if output.status.success() {
        debug_assert!(is_ancestor(run_tree, &reference, "HEAD")?);
        return Ok(None);
    }
    let files = unmerged_paths(run_tree);
    // Abort before any error of `ls-files` surfaces: never leave a merge open.
    if merge_in_progress(run_tree)? {
        git(run_tree, &["merge", "--abort"])?;
    }
    let files = files?;
    if files.is_empty() {
        return Err(git_failure(run_tree, &args, &output));
    }
    debug_assert!(!merge_in_progress(run_tree)?, "a conflict is aborted");
    Ok(Some(MergeOutcome::Conflict {
        role: role.role.clone(),
        files,
    }))
}

/// Untracked files in the run worktree where `reference` adds a file, or
/// a directory, or under a file it adds: git refuses to merge over them,
/// naming no conflict, so they are reported as one.
fn untracked_in_the_way(run_tree: &Path, reference: &str) -> Result<Vec<String>, ArchonError> {
    assert!(run_tree.is_absolute(), "the run worktree is absolute");
    assert!(
        reference.starts_with("refs/heads/"),
        "a role's branch is merged by its full ref"
    );
    let untracked = nul_paths(run_tree, &["ls-files", "--others", "--exclude-standard"])?;
    if untracked.is_empty() {
        return Ok(Vec::new());
    }
    let range = format!("HEAD...{reference}");
    let args = [
        "diff",
        "--name-only",
        "--no-renames",
        "--diff-filter=A",
        &range,
    ];
    let added = nul_paths(run_tree, &args)?;
    let added_files: BTreeSet<&str> = added.iter().map(String::as_str).collect();
    let mut added_paths = added_files.clone();
    added_paths.extend(added.iter().flat_map(|file| leading_dirs(file)));
    let in_the_way: Vec<String> = untracked
        .into_iter()
        .filter(|file| {
            added_paths.contains(file.as_str())
                || leading_dirs(file).any(|dir| added_files.contains(dir))
        })
        .collect();
    Ok(in_the_way)
}

/// `a/b/c` → `a`, `a/b`.
fn leading_dirs(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(at, _)| &path[..at])
}

/// The entries a git command lists NUL-terminated (`-z`), exactly as
/// named: neither quoted nor trimmed, whatever spaces or newlines they hold.
fn nul_entries(tree: &Path, args: &[&str]) -> Result<Vec<Vec<u8>>, ArchonError> {
    let mut full = args.to_vec();
    full.push("-z");
    let output = git_output(tree, &full)?;
    if !output.status.success() {
        return Err(git_failure(tree, &full, &output));
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// [`nul_entries`] as text, to show and compare: a name that is not
/// UTF-8 is decoded lossily, never an error. Never a git argument.
fn nul_paths(tree: &Path, args: &[&str]) -> Result<Vec<String>, ArchonError> {
    let entries = nul_entries(tree, args)?;
    Ok(entries
        .iter()
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect())
}

/// The files with unmerged index entries in `tree`, each once. Read from
/// the index (`ls-files -u`), so git enters no submodule.
fn unmerged_paths(tree: &Path) -> Result<Vec<String>, ArchonError> {
    let mut files: Vec<String> = Vec::new();
    // Each entry is `<mode> <object> <stage>\t<path>`, sorted by path.
    for entry in nul_paths(tree, &["ls-files", "--unmerged"])? {
        let Some((_, file)) = entry.split_once('\t') else {
            return Err(ArchonError::Invalid(format!(
                "git ls-files --unmerged printed a malformed entry in {}",
                tree.display()
            )));
        };
        if files.last().map(String::as_str) != Some(file) {
            files.push(file.to_owned());
        }
    }
    debug_assert!(files.windows(2).all(|pair| pair[0] != pair[1]));
    Ok(files)
}

/// [`check_worktree`], then refuse a worktree holding submodules git must
/// not enter ([`scan`]), before git that enters them runs.
fn check_tree(tree: &Path, common: &Path) -> Result<(), ArchonError> {
    check_worktree(tree, common)?;
    let foreign = scan(tree, common)?.foreign;
    if foreign.is_empty() {
        return Ok(());
    }
    let files: Vec<&str> = foreign.iter().map(String::as_str).collect();
    Err(ArchonError::Invalid(format!(
        "{} holds submodules whose git directory the repository does not keep: {}; the \
         daemon runs no git that enters them",
        tree.display(),
        listed(&files)
    )))
}

/// Options before every git command the daemon runs for a team: a role may
/// commit hooks where `core.hooksPath` looks, and none may run as the daemon.
const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// [`super::git`], without repository hooks.
fn git(dir: &Path, args: &[&str]) -> Result<String, ArchonError> {
    super::git(dir, &[&NO_HOOKS[..], args].concat())
}

/// [`super::git_output`], without repository hooks.
fn git_output(dir: &Path, args: &[&str]) -> Result<std::process::Output, ArchonError> {
    super::git_output(dir, &[&NO_HOOKS[..], args].concat())
}

/// Whether anything, a dangling symlink included, is at `path`.
pub(super) fn present(path: &Path) -> Result<bool, ArchonError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    }
}

fn has_tracked_changes(tree: &Path) -> Result<bool, ArchonError> {
    let status = git(tree, &["status", "--porcelain", "--untracked-files=no"])?;
    Ok(!status.is_empty())
}

fn merge_in_progress(tree: &Path) -> Result<bool, ArchonError> {
    let args = ["rev-parse", "-q", "--verify", "MERGE_HEAD"];
    let output = git_output(tree, &args)?;
    // `rev-parse -q --verify` exits 1, silently, when the ref is missing.
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(tree, &args, &output)),
    }
}

fn is_ancestor(tree: &Path, ancestor: &str, of: &str) -> Result<bool, ArchonError> {
    let args = ["merge-base", "--is-ancestor", ancestor, of];
    let output = git_output(tree, &args)?;
    // `merge-base --is-ancestor` exits 1 exactly when it is not one.
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(tree, &args, &output)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orphaned() -> KeptWork {
        KeptWork::Orphaned(OrphanedGitDir {
            worktree: PathBuf::from("/wt/repo/item/build-api"),
            git_dir_kept_at: PathBuf::from("/repo/.git/archon-set-aside/build-api-1"),
            head: "`archon/x--api-q69g5fav-a1`".into(),
        })
    }

    #[test]
    fn an_error_after_kept_work_keeps_its_variant_and_names_the_work() {
        let kept = orphaned();
        let error = after_kept(ArchonError::Unavailable("git lock".into()), Some(&kept));
        let ArchonError::Unavailable(text) = &error else {
            panic!("the variant changed: {error:?}");
        };
        assert!(text.starts_with(&kept.describe()), "{text}");
        assert!(text.ends_with(": git lock"), "{text}");

        let cause = std::io::Error::other("disk full");
        let error = after_kept(ArchonError::io("adding", cause), Some(&kept));
        let ArchonError::Io { context, cause } = &error else {
            panic!("the variant changed: {error:?}");
        };
        assert!(
            context.contains("archon-set-aside/build-api-1"),
            "{context}"
        );
        assert_eq!(cause.to_string(), "disk full");

        let error = after_kept(ArchonError::Store("s".into()), None);
        assert!(
            matches!(&error, ArchonError::Store(s) if s == "s"),
            "{error:?}"
        );
    }
}
