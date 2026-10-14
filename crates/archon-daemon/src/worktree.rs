//! Git worktrees per pipeline run (ARCHITECTURE §13), per team role
//! (`team`), and the snapshot a read-only run works in (`snapshot`). Shells
//! out to `git`; every failure carries git's stderr.

pub(crate) mod clean;
pub(crate) mod daemon_git;
pub(crate) mod shadow;
pub mod snapshot;
pub(crate) mod stack;
pub mod team;

use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::catalog::SNAPSHOT_DIR;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;

/// Longest title slug in a branch name.
const MAX_SLUG_CHARS: usize = 40;
/// Trailing characters of the item's ULID used in branch names.
const ITEM_SHORT_CHARS: usize = 8;
/// Under the worktree root, which no repository name starts with a dot:
/// the shadow new worktrees are checked out through.
const SHADOW_HOME: &str = ".archon";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    /// Added by this call; `false` when an existing worktree was reused.
    pub added: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeManager {
    root: PathBuf,
    branch_prefix: String,
    /// `worktrees.cache_dirs`: untracked files there never keep a worktree.
    cache_dirs: Vec<PathBuf>,
}

impl WorktreeManager {
    pub fn new(root: PathBuf, branch_prefix: String) -> Self {
        assert!(root.is_absolute(), "worktree root must be absolute");
        Self {
            root,
            branch_prefix,
            cache_dirs: Vec::new(),
        }
    }

    /// Also count untracked files in `cache_dirs`, relative to a worktree, as
    /// no change when removing one, besides the built-in caches.
    pub fn with_cache_dirs(mut self, cache_dirs: Vec<PathBuf>) -> Self {
        assert!(
            cache_dirs.iter().all(|dir| dir.is_relative()),
            "config validation keeps cache directories relative"
        );
        self.cache_dirs = cache_dirs;
        self
    }

    /// `<prefix><last 8 of item ulid, lowercase>-<slug(title), ≤ 40 chars>`,
    /// then `-fresh<n>` once the item was retried fresh `n` times.
    pub fn branch_name(&self, item: &QueueItemId, title: &str, fresh_retries: u32) -> String {
        let short = item_short(item);
        let slug = slug(title);
        let mut branch = match slug.is_empty() {
            true => format!("{}{short}", self.branch_prefix),
            false => format!("{}{short}-{slug}", self.branch_prefix),
        };
        if fresh_retries > 0 {
            branch.push_str(&format!("-fresh{fresh_retries}"));
        }
        debug_assert!(!branch.contains(".."), "slugs never contain dots");
        branch
    }

    /// Where the worktree of `item` in `repo_name` lives.
    pub fn path_for(&self, repo_name: &str, item: &QueueItemId) -> PathBuf {
        assert!(
            archon_core::repo::Repo::valid_name(repo_name),
            "repository name {repo_name:?} is not path-safe"
        );
        self.root.join(repo_name).join(item.as_str())
    }

    /// `git -C <repo> worktree add -b <branch> <root>/<repo_name>/<item> <base>`;
    /// `base` defaults to the repo's `HEAD` commit. Reuses an existing
    /// worktree for the same item (fix loops, restarts). `branch` is the
    /// item's [`Self::branch_name`].
    pub fn create(
        &self,
        repo_path: &Path,
        repo_name: &str,
        item: &QueueItemId,
        branch: &str,
        base: Option<&str>,
    ) -> Result<Worktree, ArchonError> {
        let path = self.path_for(repo_name, item);
        self.remove_leftover_snapshot(repo_path, &path)?;
        if holds_team_run(&path)? {
            return Err(ArchonError::Invalid(format!(
                "{} holds a team run of this item; remove it before running a single-worktree pipeline",
                path.display()
            )));
        }
        create_at(
            &self.shadow_home(),
            repo_path,
            path,
            check_branch(branch)?,
            base,
        )
    }

    /// As [`Self::create`], for a pipeline with a team stage: the item's
    /// directory holds the run worktree `main` and, beside it, one worktree
    /// per role ([`team::ensure_worker`]).
    pub fn create_team_run(
        &self,
        repo_path: &Path,
        repo_name: &str,
        item: &QueueItemId,
        branch: &str,
        base: Option<&str>,
    ) -> Result<Worktree, ArchonError> {
        let root = self.path_for(repo_name, item);
        if root.join(".git").exists() {
            return Err(ArchonError::Invalid(format!(
                "{} holds a single-worktree run of this item; remove it before running a team pipeline",
                root.display()
            )));
        }
        let path = root.join(archon_pipeline::team::RUN_TREE_DIR);
        let home = self.shadow_home();
        let created = create_at(&home, repo_path, path, check_branch(branch)?, base)?;
        debug_assert_eq!(created.path.parent(), Some(root.as_path()));
        Ok(created)
    }

    /// Ready a fresh retry of `item` onto the new `branch`, which must not
    /// exist yet, so its run forks it from the base: move the item's run
    /// worktree aside, the flat `<item>` worktree or a team run's
    /// `<item>/main`, as [`team::set_aside_for_fresh_retry`] does. `None`
    /// when neither is there, as when `archon gc` removed it.
    pub fn start_fresh(
        &self,
        repo_path: &Path,
        repo_name: &str,
        item: &QueueItemId,
        branch: &str,
    ) -> Result<Option<team::SetAside>, ArchonError> {
        if branch_exists(repo_path, check_branch(branch)?.as_str())? {
            return Err(ArchonError::Invalid(format!(
                "branch {branch:?} exists already, so a fresh run would not start from the \
                 base branch; delete or rename it, then retry"
            )));
        }
        let root = self.path_for(repo_name, item);
        let tree = if team::present(&root.join(".git"))? {
            root
        } else {
            let main = root.join(archon_pipeline::team::RUN_TREE_DIR);
            if !team::present(&main)? {
                return Ok(None);
            }
            main
        };
        let aside = team::set_aside_for_fresh_retry(repo_path, &tree)?;
        assert!(
            aside.kept_at.starts_with(&self.root),
            "a run worktree is kept under the worktree root"
        );
        assert_eq!(aside.worktree, tree, "the run worktree was set aside");
        Ok(Some(aside))
    }

    /// Where the shadow that checks out new worktrees ([`add_checked_out`]) lives.
    pub(crate) fn shadow_home(&self) -> PathBuf {
        self.root.join(SHADOW_HOME)
    }

    /// Remove a worktree that holds no work but untracked caches, asking the shadow
    /// under `data_dir` whether it is clean so no filter runs; the branch stays.
    pub fn remove(
        &self,
        data_dir: &Path,
        repo_path: &Path,
        worktree: &Path,
    ) -> Result<(), ArchonError> {
        assert!(
            worktree.starts_with(&self.root),
            "refusing to remove {} outside the worktree root",
            worktree.display()
        );
        assert!(repo_path.is_absolute(), "repository paths are canonical");
        let shadow = shadow::Shadow::of(data_dir, repo_path)?;
        if let Some(why) = clean::unremovable(&shadow, worktree, &self.cache_dirs)? {
            return Err(ArchonError::Invalid(format!(
                "{} is kept: {why}",
                worktree.display()
            )));
        }
        // A locked worktree is still refused: that takes `--force` twice.
        let args = ["worktree", "remove", "--force", path_arg(worktree)?];
        daemon_git::DaemonGit::sealed_in(repo_path)
            .args(args)
            .text()?;
        debug_assert!(!worktree.exists(), "git reported success but left the tree");
        Ok(())
    }
}

/// Whether the item directory `path` is laid out as a team run: not a
/// worktree itself, but holding the run worktree, a role's worktree, which
/// stays after a finished run's worktree is removed, or worktrees set aside.
/// A `repo-readonly` run's snapshot left there is no team run.
fn holds_team_run(path: &Path) -> Result<bool, ArchonError> {
    assert!(path.is_absolute(), "item directories are absolute");
    if !path.is_dir() || path.join(".git").exists() {
        return Ok(false);
    }
    let run_tree = path.join(archon_pipeline::team::RUN_TREE_DIR);
    if run_tree.exists() || path.join(team::SET_ASIDE_DIR).exists() {
        return Ok(true);
    }
    let reading = |e| ArchonError::io(format!("reading {}", path.display()), e);
    for entry in std::fs::read_dir(path).map_err(reading)? {
        let entry = entry.map_err(reading)?;
        if entry.file_name() != SNAPSHOT_DIR && entry.path().join(".git").exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add (or reuse) the worktree at `path` on `branch`, forking a new
/// branch from `base`, which defaults to the repo's `HEAD` commit.
fn create_at(
    shadow_home: &Path,
    repo_path: &Path,
    path: PathBuf,
    branch: String,
    base: Option<&str>,
) -> Result<Worktree, ArchonError> {
    assert!(repo_path.is_absolute(), "repository paths are canonical");
    assert!(path.is_absolute(), "worktree paths are absolute");
    let base = match base {
        // Git would read a base like `--no-checkout` as an option.
        Some(base) if base.starts_with('-') => {
            return Err(ArchonError::Invalid(format!(
                "base branch {base:?} is not a branch name"
            )));
        }
        Some(base) => base.to_owned(),
        None => git(repo_path, &["rev-parse", "--verify", "HEAD^{commit}"])?,
    };
    if path.exists() {
        return reuse(&path, branch, base);
    }
    let parent = path.parent().expect("worktree paths have a parent");
    std::fs::create_dir_all(parent)
        .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
    forget_missing(repo_path, &path)?;
    let path_text = path_arg(&path)?;
    // A kept branch (worktree removed by `archon gc`) is checked out again.
    let args: Vec<&str> = match branch_exists(repo_path, &branch)? {
        true => vec![path_text, &branch],
        false => vec!["-b", &branch, path_text, &base],
    };
    add_checked_out(shadow_home, repo_path, &path, &args)?;
    debug_assert!(
        head(&path)?.is_branch(&branch),
        "git added it on the branch"
    );
    Ok(Worktree {
        path,
        branch,
        base,
        added: true,
    })
}

/// `git worktree add --no-checkout <args>` for `path`, then its files checked out through the
/// shadow under `shadow_home`, so no hook or filter a repository or agent configured runs.
pub(crate) fn add_checked_out(
    shadow_home: &Path,
    repo_path: &Path,
    path: &Path,
    args: &[&str],
) -> Result<(), ArchonError> {
    assert!(shadow_home.is_absolute(), "the shadow's home is absolute");
    assert!(!path.exists(), "a worktree is added where nothing is");
    let shadow = shadow::Shadow::of(shadow_home, repo_path)?;
    let add = [&["worktree", "add", "--no-checkout"][..], args].concat();
    daemon_git::DaemonGit::sealed_in(repo_path)
        .args(add)
        .text()?;
    let Err(error) = check_out(&shadow, path) else {
        return Ok(());
    };
    // A registered worktree with no files would be reused as is, all deleted.
    let path_text = path_arg(path)?;
    let remove = ["worktree", "remove", "--force", path_text];
    match daemon_git::DaemonGit::sealed_in(repo_path)
        .args(remove)
        .text()
    {
        Ok(_) => Err(error),
        Err(undo) => Err(error.with_detail(&format!("removing {path_text} also failed: {undo}"))),
    }
}

/// Check out `HEAD` into the worktree `tree`, added with no index and no files.
fn check_out(shadow: &shadow::Shadow, tree: &Path) -> Result<(), ArchonError> {
    let commit = daemon_git::DaemonGit::sealed_in(tree)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .text()?;
    assert!(
        stack::is_commit_id(&commit),
        "rev-parse --verify prints a commit id"
    );
    let index = clean::index_of(tree)?;
    assert!(!index.exists(), "--no-checkout writes no index");
    shadow
        .git(Some(tree), daemon_git::Config::Operator)?
        .env("GIT_INDEX_FILE", path_arg(&index)?)
        .args(["read-tree", "-m", "-u", &commit])
        .text()
        .map(drop)
}

/// A worktree directory deleted by hand stays registered in git, which
/// blocks `worktree add` there and keeps its branch checked out.
fn forget_missing(repo_path: &Path, path: &Path) -> Result<(), ArchonError> {
    let Some(registered) = registered_missing(repo_path, path)? else {
        return Ok(());
    };
    tracing::warn!(path = %path.display(), "worktree was deleted by hand; deregistering it");
    git(repo_path, &["worktree", "remove", &registered])?;
    Ok(())
}

/// The path git registered for the missing worktree `path`, as git lists
/// it, if git still has it; `path`'s parent must exist.
fn registered_missing(repo_path: &Path, path: &Path) -> Result<Option<String>, ArchonError> {
    assert!(!path.exists(), "only a missing worktree is looked up");
    let parent = path.parent().expect("worktree paths have a parent");
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| ArchonError::io(format!("resolving {}", parent.display()), e))?;
    let canonical = parent.join(path.file_name().expect("worktree paths name an item"));
    let listed = git(repo_path, &["worktree", "list", "--porcelain"])?;
    let registered = listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .find(|p| Path::new(p) == path || Path::new(p) == canonical);
    Ok(registered.map(str::to_owned))
}

/// An existing directory is reused only if it is a worktree on `branch`;
/// anything else there is an error rather than silently shared.
fn reuse(path: &Path, branch: String, base: String) -> Result<Worktree, ArchonError> {
    check_own_worktree(path)?;
    let checked_out = head(path)?;
    if !checked_out.is_branch(&branch) {
        return Err(ArchonError::Invalid(format!(
            "{} exists but has {checked_out} checked out, not {branch:?}",
            path.display()
        )));
    }
    Ok(Worktree {
        path: path.to_owned(),
        branch,
        base,
        added: false,
    })
}

/// Git looks for a repository upwards from where it runs: make sure `path`
/// is a work tree's top level, not a directory inside some other one.
fn check_own_worktree(path: &Path) -> Result<(), ArchonError> {
    assert!(path.is_absolute(), "worktree paths are absolute");
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| ArchonError::io(format!("resolving {}", path.display()), e))?;
    let top = toplevel(path)?;
    if top != canonical {
        return Err(ArchonError::Invalid(format!(
            "{} is not a git worktree (git found the work tree {} around it)",
            path.display(),
            top.display()
        )));
    }
    Ok(())
}

/// What `HEAD` names in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Head {
    /// A branch, by its name under `refs/heads/`.
    Branch(String),
    /// A commit.
    Detached,
    /// Some other reference, such as a tag: `HEAD` can be pointed anywhere
    /// under `refs/`.
    Other(String),
}

impl Head {
    pub(crate) fn is_branch(&self, name: &str) -> bool {
        matches!(self, Head::Branch(branch) if branch == name)
    }
}

impl std::fmt::Display for Head {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Head::Branch(name) | Head::Other(name) => write!(f, "`{name}`"),
            Head::Detached => f.write_str("a detached `HEAD`"),
        }
    }
}

/// What `HEAD` names in the worktree `tree`, read with `symbolic-ref`:
/// `rev-parse --abbrev-ref` would say `heads/<name>` were a tag so named.
pub(crate) fn head(tree: &Path) -> Result<Head, ArchonError> {
    let args = ["symbolic-ref", "--quiet", "HEAD"];
    let output = git_output(tree, &args)?;
    // `symbolic-ref --quiet` exits 1, silently, when `HEAD` is detached.
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Head::Detached),
        _ => return Err(git_failure(tree, &args, &output)),
    }
    // Lossy: a name that is not UTF-8 is only ever shown or compared.
    let text = String::from_utf8_lossy(&output.stdout);
    let reference = text.trim_end_matches('\n');
    debug_assert!(!reference.is_empty(), "exit 0 names a reference");
    Ok(match reference.strip_prefix("refs/heads/") {
        Some(branch) => Head::Branch(branch.to_owned()),
        None => Head::Other(reference.to_owned()),
    })
}

fn branch_exists(repo: &Path, branch: &str) -> Result<bool, ArchonError> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output(repo, &["show-ref", "--verify", "--quiet", &reference])?;
    // `show-ref --verify --quiet` exits 1 exactly when the ref is missing.
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(repo, &["show-ref", &reference], &output)),
    }
}

/// `branch`, when git cannot read it as an option or an empty name.
fn check_branch(branch: &str) -> Result<String, ArchonError> {
    if branch.is_empty() || branch.starts_with('-') {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is not a branch name"
        )));
    }
    Ok(branch.to_owned())
}

fn path_arg(path: &Path) -> Result<&str, ArchonError> {
    path.to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", path.display())))
}

/// Run `git -C <dir> <args>` and return its trimmed stdout.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, ArchonError> {
    let output = git_output(dir, args)?;
    if !output.status.success() {
        return Err(git_failure(dir, args, &output));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| ArchonError::Invalid(format!("git {} printed non-UTF-8", args.join(" "))))?;
    Ok(stdout.trim().to_owned())
}

/// The full id of the commit `name` resolves to in `dir`; `None` when there is none.
pub(crate) fn commit_id(dir: &Path, name: &str) -> Result<Option<String>, ArchonError> {
    let commit = format!("{name}^{{commit}}");
    let id = git_exit(dir, &["rev-parse", "-q", "--verify", commit.as_str()])?;
    assert!(
        id.as_ref().is_none_or(|id| matches!(id.len(), 40 | 64)),
        "rev-parse --verify prints a full object id"
    );
    Ok(id)
}

/// Whether `ancestor` is `of` or one of its ancestors.
pub(crate) fn is_ancestor(dir: &Path, ancestor: &str, of: &str) -> Result<bool, ArchonError> {
    let args = ["merge-base", "--is-ancestor", ancestor, of];
    Ok(git_exit(dir, &args)?.is_some())
}

/// The best common ancestor of commits `a` and `b`; `None` when they share none.
pub(crate) fn merge_base(dir: &Path, a: &str, b: &str) -> Result<Option<String>, ArchonError> {
    git_exit(dir, &["merge-base", a, b])
}

/// `git` whose exit 1 answers no (`None`) rather than failing; stdout on success.
/// Both `rev-parse -q --verify` and `merge-base --is-ancestor` exit 1 exactly so.
fn git_exit(dir: &Path, args: &[&str]) -> Result<Option<String>, ArchonError> {
    let output = git_output(dir, args)?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Some(1) => Ok(None),
        _ => Err(git_failure(dir, args, &output)),
    }
}

fn git_output(dir: &Path, args: &[&str]) -> Result<std::process::Output, ArchonError> {
    assert!(!args.is_empty(), "a git command needs arguments");
    // A role controls its worktree: no hook or fsmonitor command it
    // configured runs as the daemon.
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(daemon_git::no_programs())
        .args(args)
        .output()
        .map_err(|e| ArchonError::io(format!("running git in {}", dir.display()), e))
}

fn git_failure(dir: &Path, args: &[&str], output: &std::process::Output) -> ArchonError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ArchonError::io(
        format!("git -C {} {}", dir.display(), args.join(" ")),
        std::io::Error::other(format!("{}: {stderr}", output.status)),
    )
}

/// The canonical top level of the work tree containing `path`.
pub(crate) fn toplevel(path: &Path) -> Result<PathBuf, ArchonError> {
    let top = git(path, &["rev-parse", "--show-toplevel"])?;
    std::fs::canonicalize(&top).map_err(|e| ArchonError::io(format!("resolving {top}"), e))
}

pub(crate) fn item_short(item: &QueueItemId) -> String {
    let ulid = &item.as_str()[QueueItemId::PREFIX.len() + 1..];
    assert!(ulid.len() >= ITEM_SHORT_CHARS, "ULIDs have 26 characters");
    ulid[ulid.len() - ITEM_SHORT_CHARS..].to_ascii_lowercase()
}

/// Lowercase ASCII letters and digits; every other run becomes one `-`.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if out.len() >= MAX_SLUG_CHARS {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_short_slugged_and_prefixed() {
        let manager = WorktreeManager::new("/wt".into(), "archon/".into());
        let item = QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(
            manager.branch_name(&item, "Fix: the  Login bug!", 0),
            "archon/q69g5fav-fix-the-login-bug"
        );
        assert_eq!(manager.branch_name(&item, "¿?", 0), "archon/q69g5fav");
        let long = "a".repeat(100);
        let branch = manager.branch_name(&item, &long, 0);
        assert_eq!(branch.len(), "archon/q69g5fav-".len() + MAX_SLUG_CHARS);
        assert_eq!(
            manager.branch_name(&item, &long, 2),
            format!("{branch}-fresh2"),
            "a fresh retry's suffix is never clipped"
        );
        assert_eq!(
            manager.branch_name(&item, "¿?", 1),
            "archon/q69g5fav-fresh1"
        );
    }

    /// A repository whose `.gitattributes` names a smudge filter and whose
    /// post-checkout hook both leave a marker in `root` when anything runs them.
    fn planted_repo(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let git = daemon_git::DaemonGit::sealed_in(&repo);
            git.args(args.to_vec()).text().unwrap()
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=planted\n").unwrap();
        std::fs::write(repo.join("a.txt"), "tracked\n").unwrap();
        git(&["add", "."]);
        let who = ["-c", "user.name=a", "-c", "user.email=a@example.invalid"];
        git(&[&who[..], &["commit", "--quiet", "-m", "x"]].concat());
        let smudge = format!("touch {}; cat", root.join("smudged").display());
        git(&["config", "filter.planted.smudge", &smudge]);
        let hook = repo.join(".git/hooks/post-checkout");
        let script = format!("#!/bin/sh\ntouch {}\n", root.join("hooked").display());
        archon_core::testing::write_script(&hook, &script);
        repo
    }

    #[test]
    fn a_new_worktree_is_checked_out_without_the_repository_s_filters_or_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = planted_repo(&root);
        let manager = WorktreeManager::new(root.join("worktrees"), "archon/".into());
        let item = QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let branch = manager.branch_name(&item, "planted", 0);

        let created = manager.create(&repo, "demo", &item, &branch, None).unwrap();
        assert!(created.added && head(&created.path).unwrap().is_branch(&branch));
        let file = std::fs::read_to_string(created.path.join("a.txt")).unwrap();
        assert_eq!(file, "tracked\n");
        let status =
            daemon_git::DaemonGit::sealed_in(&created.path).args(["status", "--porcelain"]);
        assert_eq!(
            status.text().unwrap(),
            "",
            "index and files are the branch's"
        );
        assert!(
            !root.join("smudged").exists(),
            "the repository's filter never ran"
        );
        assert!(
            !root.join("hooked").exists(),
            "the repository's hook never ran"
        );

        let plain = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(root.join("plain"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(plain.success());
        assert!(
            root.join("smudged").exists() && root.join("hooked").exists(),
            "the planted filter and hook are real: plain git runs them"
        );
    }

    #[test]
    fn slugs_never_end_in_a_dash() {
        assert_eq!(slug("abc -- "), "abc");
        assert_eq!(slug("--x"), "x");
        assert_eq!(slug(&format!("{}!b", "a".repeat(39))), "a".repeat(39));
    }
}
