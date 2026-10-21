//! What archond proves about a run worktree before it delivers anything
//! (ARCHITECTURE §8.7): it is the run's own worktree, on the run branch,
//! with no uncommitted change, holding the tree the pipeline verified, ahead
//! of its base, and every commit it adds carries the repository's identity
//! and, under the attribution policy `none`, no attribution line.
//! Blocking git, none of it running a program the worktree's config names:
//! callers run it off the async runtime.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::repo::Attribution;
use archon_pipeline::risk::DiffStat;
use archon_pipeline::swarm::PLAN_FILE;
use archon_pipeline::{ShipRecord, attribution_lines};

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
    /// The commit the run's own commits start after, when not `base`: the
    /// frozen head of the pull request an `onto` item delivers into.
    pub since: Option<String>,
    /// The addresses a commit the run adds may carry, recorded at registration.
    pub allowed: Vec<String>,
    /// The repository's attribution policy at delivery.
    pub attribution: Attribution,
}

/// A worktree that may be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Checked {
    /// The run branch's head, which is what ships.
    pub commit: String,
    /// What the run's own commits change; `None` when git could not say.
    pub diff: Option<DiffStat>,
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
    let base = match floor(tree, target)? {
        Ok(base) => base,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let ahead = git(tree, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    if ahead == "0" {
        let since = target.since.as_deref().unwrap_or(&target.base);
        return Ok(Err(format!(
            "nothing to ship: `{}` has no commit beyond `{since}`",
            target.branch
        )));
    }
    if let Err(refusal) = foreign_commits(tree, &base, &target.allowed)? {
        return Ok(Err(refusal));
    }
    if target.attribution == Attribution::None
        && let Err(refusal) = unattributed(tree, &base, record)?
    {
        return Ok(Err(refusal));
    }
    if let Err(refusal) = unshippable_files(tree, &base)? {
        return Ok(Err(refusal));
    }
    let diff = match diff_stat(tree, &base) {
        Ok(diff) => Some(diff),
        Err(error) => {
            tracing::warn!(%error, "the delivered change's diff could not be read");
            None
        }
    };
    Ok(Ok(Checked { commit, diff }))
}

/// Prove the run worktree holds no change at all: its own worktree, on the
/// run branch, clean, with no commit beyond its base. The inner `Err` says why
/// a claim of "no change needed" is false.
pub(super) fn check_unchanged(target: &Target) -> Result<Result<(), String>, ArchonError> {
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
            "the run worktree has uncommitted changes, so a change exists: {}",
            listed(dirty.lines())
        )));
    }
    let staged = git(tree, &["diff", "--cached", "--name-only", "--no-renames"])?;
    let untracked = git(tree, &["ls-files", "--others", "--exclude-standard"])?;
    let present = staged
        .lines()
        .chain(untracked.lines())
        .filter(|path| !is_unshippable(path))
        .collect::<Vec<_>>();
    if !present.is_empty() {
        return Ok(Err(format!(
            "the run worktree has staged or untracked files, so a change exists: {}",
            listed(present.into_iter())
        )));
    }
    let base = match floor(tree, target)? {
        Ok(base) => base,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let ahead = git(tree, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    if ahead != "0" {
        return Ok(Err(format!(
            "`{}` holds {ahead} commit(s) beyond `{base}`, so a change exists",
            target.branch
        )));
    }
    Ok(Ok(()))
}

/// Whether `path` is output that is never part of a change: a Python
/// bytecode cache, or the swarm plan the workers built from.
fn is_unshippable(path: &str) -> bool {
    path == PLAN_FILE
        || path.split('/').any(|part| part == "__pycache__")
        || path.ends_with(".pyc")
        || path.ends_with(".pyo")
}

/// Refuse a change that adds a build cache or the swarm plan file, naming
/// each so the author removes it in a commit.
fn unshippable_files(tree: &Path, base: &str) -> Result<Result<(), String>, ArchonError> {
    assert!(is_floor(base), "a full ref name or a commit id");
    let range = format!("{base}...HEAD");
    let ran = DaemonGit::sealed_in(tree)
        .args([
            "diff",
            "--name-only",
            "--diff-filter=d",
            "-z",
            range.as_str(),
        ])
        .output()?;
    if !ran.status.success() {
        return Err(ran.failure());
    }
    let names: Vec<String> = ran
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    let found: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|path| is_unshippable(path))
        .collect();
    if found.is_empty() {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "the change carries files that must not ship: {}. Remove them in a commit \
         (`git rm --cached`; a build cache belongs in `.gitignore`)",
        listed(found.iter().copied())
    )))
}

/// What the commits after `base`, a [`floor`], change up to `HEAD`.
fn diff_stat(tree: &Path, base: &str) -> Result<DiffStat, ArchonError> {
    assert!(is_floor(base), "a full ref name or a commit id");
    let range = format!("{base}...HEAD");
    let raw = DaemonGit::sealed_in(tree)
        .args(["diff", "--numstat", "-z", "--no-color", range.as_str()])
        .text()?;
    let diff = DiffStat::from_numstat(&raw).map_err(ArchonError::Protocol)?;
    assert!(
        diff.files.iter().all(|f| !f.path.is_empty()),
        "every file is named"
    );
    Ok(diff)
}

/// Git in the run worktree under the repository's config, running none of
/// the programs it names; its stdout.
fn git(tree: &Path, args: &[&str]) -> Result<String, ArchonError> {
    DaemonGit::sealed_in(tree).args(args.iter().copied()).text()
}

/// The run worktree's tracked files that differ from `commit`, compared through the
/// shadow in its own index, so no configured filter runs and the worktree is not written.
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

/// Why the base branch cannot be found, naming a missing `origin` remote
/// rather than blaming the base for it.
fn missing_base(target: &Target) -> String {
    let no_origin = matches!(
        crate::worktree::stack::origin_url(&target.repo_path),
        Ok(None)
    );
    match no_origin {
        true => format!(
            "the base branch `{}` is not in the repository and it has no `origin` remote",
            target.base
        ),
        false => format!(
            "the base branch `{}` exists neither in the repository nor on `origin`",
            target.base
        ),
    }
}

/// Where the run's own commits start: `target.since` when it is an ancestor
/// of the run's head, else the ref of `target.base`.
fn floor(tree: &Path, target: &Target) -> Result<Result<String, String>, ArchonError> {
    let Some(since) = &target.since else {
        let found = base_ref(tree, &target.base)?;
        return Ok(found.ok_or_else(|| missing_base(target)));
    };
    assert!(
        crate::worktree::stack::is_commit_id(since),
        "a frozen commit id"
    );
    let held = DaemonGit::sealed_in(tree)
        .args(["merge-base", "--is-ancestor", since, "HEAD"])
        .output()?;
    // `merge-base --is-ancestor` exits 1 exactly when it is not one.
    match held.status.code() {
        Some(0) => Ok(Ok(since.clone())),
        Some(1) => Ok(Err(format!(
            "the run branch `{}` does not hold {since}, the head of the pull request it \
             delivers into, so it cannot be pushed there by fast-forward",
            target.branch
        ))),
        _ => Err(held.failure()),
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
    assert!(is_floor(base), "a full ref name or a commit id");
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
         `archon repo add`, and again at `archon repo set` after a deliberate change. \
         A plain `archon queue retry` keeps these commits and is refused again: run `archon repo \
         set` if their identity is the one to accept, else `archon queue retry --fresh`",
        foreign.len(),
        allowed[0],
        listed(foreign.iter().map(String::as_str))
    )))
}

/// Refuse a record whose `pr.body`, or a commit the run adds, carries a line
/// attributing the change to a model or a tool, quoting each such line.
fn unattributed(
    tree: &Path,
    base: &str,
    record: &ShipRecord,
) -> Result<Result<(), String>, ArchonError> {
    assert!(is_floor(base), "a full ref name or a commit id");
    assert!(!record.body.trim().is_empty(), "a read record has a body");
    let mut found: Vec<String> = attribution_lines(&record.body)
        .into_iter()
        .map(|line| format!("`pr.body` has {line:?}"))
        .collect();
    let log = git(
        tree,
        &[
            "log",
            "-z",
            "--format=%H%n%B",
            "HEAD",
            "--not",
            base,
            "--remotes",
        ],
    )?;
    for entry in log.split('\0').filter(|entry| !entry.trim().is_empty()) {
        let entry = entry.trim_start();
        let (commit, message) = entry.split_once('\n').unwrap_or((entry, ""));
        if !crate::worktree::stack::is_commit_id(commit) {
            return Err(ArchonError::Protocol(format!(
                "git log printed {entry:?}, not a commit and its message"
            )));
        }
        let short = &commit[..12];
        let lines = attribution_lines(message).into_iter();
        found.extend(lines.map(|line| format!("commit {short} has {line:?}")));
    }
    if found.is_empty() {
        return Ok(Ok(()));
    }
    // Every line is quoted, not `listed`: each is one the stage must remove.
    Ok(Err(format!(
        "this repository's attribution policy is `none`, so nothing that ships may credit a \
         model or a tool, and {} line(s) do: {}. Retry the item so its stage writes `pr.body` \
         without them and rewords those commits (no `Co-Authored-By:` trailer, no \
         `Generated with` line), then records the new `commit` and `tree`",
        found.len(),
        found.join("; ")
    )))
}

/// Whether `base` is what [`floor`] gives: a full ref name or a commit id.
fn is_floor(base: &str) -> bool {
    base.starts_with("refs/") || crate::worktree::stack::is_commit_id(base)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_and_the_swarm_plan_are_unshippable() {
        for path in [
            ".archon/swarm-plan.json",
            "pkg/__pycache__/m.cpython-312.pyc",
            "x.pyc",
            "src/y.pyo",
        ] {
            assert!(is_unshippable(path), "{path}");
        }
        for path in [".archon/config.toml", "src/lib.rs", "docs/swarm-plan.json"] {
            assert!(!is_unshippable(path), "{path}");
        }
    }
}
