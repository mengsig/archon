//! The git and filesystem work behind a mirror. Its git directory is
//! `<data>/mirror-git/<name>`, out of reach of a helper confined to the
//! orchestrator's workspace, and its config is rewritten before every
//! command: nothing a helper writes runs as the daemon. The mirror's `.git`
//! is a file naming that directory, rewritten each refresh; git here never
//! reads it, and a symbolic link where a mirror belongs is replaced.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use archon_core::error::ArchonError;

use crate::worktree::daemon_git::{Config, DaemonGit};
pub(super) use crate::worktree::shadow::{is_real_dir, kind, remove_entry};
use crate::worktree::shadow::{make, seal, sealed_config};

/// Every branch of the repository, under a namespace no push names.
const FETCH_BRANCHES: &str = "+refs/heads/*:refs/mirror/*";
/// The base as the repository names it, when that is not one of its branches.
const BASE_REF: &str = "refs/archon/base";
/// A first fetch copies the repository's whole object store.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A first checkout writes every file of the base.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PUSH_GUARD: &str = "#!/bin/sh\n\
    echo 'archon: this mirror is read-only; queue an item to change the repository' >&2\n\
    exit 1\n";

/// The commit a mirror is checked out at, and where a fetch finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Base {
    pub commit: String,
    /// The ref of the repository that names it, fetched besides the
    /// branches; `None` for a bare commit id.
    source_ref: Option<String>,
    /// The repository's object format, which its mirror shares.
    format: String,
}

impl Base {
    /// What `base`, or `HEAD` without one, names in the repository now.
    pub(crate) fn resolve(repo: &Path, base: Option<&str>) -> Result<Self, ArchonError> {
        assert!(repo.is_absolute(), "registered paths are canonical");
        let name = base.unwrap_or("HEAD");
        // Git would read a base like `--all` as an option.
        if name.is_empty() || name.starts_with('-') {
            return Err(ArchonError::Invalid(format!(
                "base branch {name:?} is not a branch name"
            )));
        }
        let spec = format!("{name}^{{commit}}");
        let args = ["rev-parse", "--verify", "--quiet", &spec];
        let commit = DaemonGit::sealed_in(repo).args(args).text().map_err(|_| {
            ArchonError::Invalid(format!(
                "base {name:?} names no commit in {}",
                repo.display()
            ))
        })?;
        assert!(!commit.is_empty(), "rev-parse --verify prints the commit");
        let source_ref = match base {
            None => Some("HEAD".to_owned()),
            Some(_) => Some(
                DaemonGit::sealed_in(repo)
                    .args(["rev-parse", "--symbolic-full-name", name])
                    .text()?,
            )
            .filter(|full| !full.is_empty()),
        };
        let format = DaemonGit::sealed_in(repo)
            .args(["rev-parse", "--show-object-format"])
            .text()?;
        Ok(Self {
            commit,
            source_ref,
            format,
        })
    }

    /// The same source at `commit`, which must be one of its ancestors to be fetched.
    pub(crate) fn at(&self, commit: &str) -> Self {
        assert!(
            crate::worktree::stack::is_commit_id(commit),
            "a full commit id"
        );
        Self {
            commit: commit.to_owned(),
            ..self.clone()
        }
    }
}

/// One repository's mirror: the work tree helpers read and the git
/// directory only the daemon writes.
#[derive(Debug, Clone)]
pub(crate) struct Mirror {
    pub work: PathBuf,
    pub git_dir: PathBuf,
    /// The push guard's directory, named by the mirror's config.
    pub hooks: PathBuf,
}

impl Mirror {
    /// Make the git directory if it is missing and the work tree a real
    /// directory whose `.git` names it, whatever was there before.
    pub(crate) fn prepare(&self, base: &Base) -> Result<(), ArchonError> {
        make(&self.git_dir, &base.format)?;
        if !is_real_dir(&self.work)? {
            remove_entry(&self.work)?;
            std::fs::create_dir(&self.work)
                .map_err(|e| ArchonError::io(format!("creating {}", self.work.display()), e))?;
        }
        let dot_git = self.work.join(".git");
        let wanted = format!("gitdir: {}\n", self.git_dir.display());
        let plain = kind(&dot_git)?.is_some_and(|kind| kind.is_file());
        if !plain || std::fs::read_to_string(&dot_git).ok().as_deref() != Some(&wanted) {
            // A `.git` directory of the old layout goes too, objects and all.
            remove_entry(&dot_git)?;
            std::fs::write(&dot_git, &wanted)
                .map_err(|e| ArchonError::io(format!("writing {}", dot_git.display()), e))?;
        }
        assert!(is_real_dir(&self.work)?, "the work tree is a directory");
        Ok(())
    }

    /// Fetch the repository's branches, and its base when that is no branch,
    /// from its path. Incremental: only objects the mirror lacks are copied.
    pub(crate) fn fetch(&self, source: &Path, base: &Base) -> Result<(), ArchonError> {
        let source = source
            .to_str()
            .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", source.display())))?;
        let base_spec = base
            .source_ref
            .as_ref()
            .map(|source_ref| format!("+{source_ref}:{BASE_REF}"));
        let mut args = vec![
            "fetch",
            "--quiet",
            "--prune",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--upload-pack=git-upload-pack",
            "--",
            source,
            FETCH_BRANCHES,
        ];
        args.extend(base_spec.as_deref());
        self.git(&base.format)?
            .args(args)
            .limit(FETCH_TIMEOUT)
            .text()
            .map(drop)
    }

    /// Check the mirror out detached at `commit`. Tracked files are reset to
    /// it; untracked ones (a helper's notes, build output) are left.
    pub(crate) fn checkout(&self, base: &Base) -> Result<(), ArchonError> {
        assert!(!base.commit.starts_with('-'), "a commit id is no option");
        let args = ["checkout", "--quiet", "--force", "--detach", &base.commit];
        self.git(&base.format)?
            .args(args)
            .limit(CHECKOUT_TIMEOUT)
            .text()
            .map(drop)
    }

    /// The commit the mirror has checked out; `None` before its first checkout.
    pub(crate) fn head(&self, base: &Base) -> Result<Option<String>, ArchonError> {
        let args = ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"];
        let ran = self.git(&base.format)?.args(args).output()?;
        // `rev-parse --verify --quiet` exits 1 exactly when the name is unborn.
        match ran.status.code() {
            Some(0) => Ok(Some(ran.stdout_text()?)),
            Some(1) => Ok(None),
            _ => Err(ran.failure()),
        }
    }

    /// What was changed in the mirror since the daemon checked out `commit`:
    /// commits made on top of it and tracked files changed, staged or not;
    /// `None` when nothing was. Untracked files are not counted.
    pub(crate) fn changes_since(
        &self,
        base: &Base,
        commit: &str,
    ) -> Result<Option<String>, ArchonError> {
        assert!(
            crate::worktree::stack::is_commit_id(commit),
            "a full commit id"
        );
        let mut found = Vec::new();
        match self.head(base)? {
            Some(head) if head == commit => {}
            Some(head) => found.push(self.commits_between(base, commit, &head)?),
            None => found.push("HEAD unset".to_owned()),
        }
        let args = [
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=no",
            "--no-renames",
            "--ignore-submodules",
        ];
        let ran = self.git(&base.format)?.args(args).output()?;
        if !ran.status.success() {
            return Err(ran.failure());
        }
        // Each record is `XY path`, the status taking three bytes.
        let changed: Vec<String> = ran
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| record.len() > 3)
            .map(|record| String::from_utf8_lossy(&record[3..]).into_owned())
            .collect();
        if !changed.is_empty() {
            found.push(format!("changed {}", listed(&changed)));
        }
        Ok((!found.is_empty()).then(|| found.join("; ")))
    }

    /// The commits `head` has that `commit` lacks, newest first, as
    /// `HEAD moved to …`; just the move when git cannot list them.
    fn commits_between(
        &self,
        base: &Base,
        commit: &str,
        head: &str,
    ) -> Result<String, ArchonError> {
        assert_ne!(commit, head, "HEAD moved");
        let range = format!("{commit}..{head}");
        let args = ["log", "--format=%h %s", "--max-count=4", &range];
        let ran = self.git(&base.format)?.args(args).output()?;
        let short = &head[..head.len().min(12)];
        let made = ran.stdout_text().unwrap_or_default();
        if !ran.status.success() || made.is_empty() {
            return Ok(format!("HEAD moved to {short}"));
        }
        let made: Vec<String> = made.lines().map(|line| format!("`{line}`")).collect();
        Ok(format!(
            "HEAD moved to {short}, committing {}",
            listed(&made)
        ))
    }

    /// Remove untracked files git does not ignore, keeping ignored build output.
    pub(crate) fn clean(&self, base: &Base) -> Result<(), ArchonError> {
        let args = ["clean", "--quiet", "--force", "-d"];
        self.git(&base.format)?
            .args(args)
            .limit(CHECKOUT_TIMEOUT)
            .text()
            .map(drop)
    }

    /// Git on exactly this git directory and work tree, sealed first.
    fn git(&self, format: &str) -> Result<DaemonGit, ArchonError> {
        seal(
            &self.git_dir,
            &sealed_config(format, false, Some(&self.hooks)),
        )?;
        let git = DaemonGit::owned(&self.git_dir, Some(&self.work), Config::Sealed);
        Ok(git)
    }
}

/// The first few of `names`, and how many more there are.
fn listed(names: &[String]) -> String {
    const SHOWN: usize = 3;
    assert!(!names.is_empty(), "a list of something");
    let shown = names[..names.len().min(SHOWN)].join(", ");
    match names.len().checked_sub(SHOWN) {
        Some(more) if more > 0 => format!("{shown} and {more} more"),
        _ => shown,
    }
}

/// Remove every entry of the directory `root` not named in `keep`, opening
/// a read-only mirror first.
pub(super) fn remove_all_but(root: &Path, keep: &BTreeSet<String>) -> Result<(), ArchonError> {
    assert!(is_real_dir(root)?, "the directory was prepared");
    let entries = std::fs::read_dir(root)
        .map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| keep.contains(name)) {
            continue;
        }
        tracing::info!(entry = %entry.path().display(), "removing what is no registered repository's mirror");
        super::lock::open(&entry.path())?;
        remove_entry(&entry.path())?;
    }
    Ok(())
}

/// The `pre-push` hook every mirror runs: a push from a mirror, or from a
/// worktree of one, is refused wherever it is aimed.
pub(crate) fn write_push_guard(hooks: &Path) -> Result<(), ArchonError> {
    let hook = hooks.join("pre-push");
    let written = std::fs::read_to_string(&hook).is_ok_and(|text| text == PUSH_GUARD);
    if !written {
        std::fs::write(&hook, PUSH_GUARD)
            .map_err(|e| ArchonError::io(format!("writing {}", hook.display()), e))?;
    }
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| ArchonError::io(format!("marking {} executable", hook.display()), e))
}
