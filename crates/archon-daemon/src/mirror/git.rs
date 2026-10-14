//! The git and filesystem work behind a mirror. Git runs here without the
//! operator's global config and with hooks off, on a directory proven to be
//! the mirror itself: a symbolic link, or a `.git` that points elsewhere, is
//! replaced, never followed.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use archon_core::error::ArchonError;

use crate::worktree;

/// Every branch of the repository, under a namespace no push names.
const FETCH_BRANCHES: &str = "+refs/heads/*:refs/mirror/*";
/// The base as the repository names it, when that is not one of its branches.
const BASE_REF: &str = "refs/archon/base";
/// A clone still being made; a leading dot is not a repository name.
const NEW_PREFIX: &str = ".new-";
/// Settings of the daemon's own git in a mirror, whatever its config says.
const OWN_CONFIG: [&str; 8] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "gc.autoDetach=false",
    "-c",
    "advice.detachedHead=false",
];
const PUSH_GUARD: &str = "#!/bin/sh\n\
    echo 'archon: this mirror is read-only; queue an item to change the repository' >&2\n\
    exit 1\n";

/// The commit a mirror is checked out at, and where a fetch finds it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Base {
    pub commit: String,
    /// The ref of the repository that names it, fetched besides the
    /// branches; `None` for a bare commit id.
    source_ref: Option<String>,
}

impl Base {
    /// What `base`, or `HEAD` without one, names in the repository now.
    pub(super) fn resolve(repo: &Path, base: Option<&str>) -> Result<Self, ArchonError> {
        assert!(repo.is_absolute(), "registered paths are canonical");
        let name = base.unwrap_or("HEAD");
        // Git would read a base like `--all` as an option.
        if name.is_empty() || name.starts_with('-') {
            return Err(ArchonError::Invalid(format!(
                "base branch {name:?} is not a branch name"
            )));
        }
        let spec = format!("{name}^{{commit}}");
        let commit =
            worktree::git(repo, &["rev-parse", "--verify", "--quiet", &spec]).map_err(|_| {
                ArchonError::Invalid(format!(
                    "base {name:?} names no commit in {}",
                    repo.display()
                ))
            })?;
        assert!(!commit.is_empty(), "rev-parse --verify prints the commit");
        let source_ref = match base {
            None => Some("HEAD".to_owned()),
            Some(_) => Some(worktree::git(
                repo,
                &["rev-parse", "--symbolic-full-name", name],
            )?)
            .filter(|full| !full.is_empty()),
        };
        Ok(Self { commit, source_ref })
    }
}

/// Whether `path` is a mirror git may run in: a real directory with a real
/// `.git` directory, neither of them a symbolic link.
pub(super) fn is_mirror(path: &Path) -> Result<bool, ArchonError> {
    Ok(is_real_dir(path)? && is_real_dir(&path.join(".git"))?)
}

pub(super) fn is_real_dir(path: &Path) -> Result<bool, ArchonError> {
    Ok(kind(path)?.is_some_and(|kind| kind.is_dir()))
}

pub(super) fn is_symlink(path: &Path) -> Result<bool, ArchonError> {
    Ok(kind(path)?.is_some_and(|kind| kind.is_symlink()))
}

/// What is at `path` itself, a symbolic link not followed; `None` if nothing.
fn kind(path: &Path) -> Result<Option<std::fs::FileType>, ArchonError> {
    match std::fs::symlink_metadata(path) {
        Ok(found) => Ok(Some(found.file_type())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    }
}

/// Remove whatever is at `path`: a directory with all it holds, a symbolic
/// link as the link itself. Nothing there is fine.
pub(super) fn remove_entry(path: &Path) -> Result<(), ArchonError> {
    let removed = match kind(path)? {
        None => return Ok(()),
        Some(kind) if kind.is_dir() => std::fs::remove_dir_all(path),
        Some(_) => std::fs::remove_file(path),
    };
    removed.map_err(|e| ArchonError::io(format!("removing {}", path.display()), e))
}

/// Remove every entry of the mirrors directory `root` not named in `keep`.
pub(super) fn remove_all_but(root: &Path, keep: &BTreeSet<String>) -> Result<(), ArchonError> {
    assert!(is_real_dir(root)?, "the mirrors directory was prepared");
    let entries = std::fs::read_dir(root)
        .map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| keep.contains(name)) {
            continue;
        }
        tracing::info!(entry = %entry.path().display(), "removing what is no registered repository's mirror");
        remove_entry(&entry.path())?;
    }
    Ok(())
}

/// The `pre-push` hook every mirror runs: a push from a mirror, or from a
/// worktree of one, is refused wherever it is aimed.
pub(super) fn write_push_guard(hooks: &Path) -> Result<(), ArchonError> {
    let hook = hooks.join("pre-push");
    let written = std::fs::read_to_string(&hook).is_ok_and(|text| text == PUSH_GUARD);
    if !written {
        std::fs::write(&hook, PUSH_GUARD)
            .map_err(|e| ArchonError::io(format!("writing {}", hook.display()), e))?;
    }
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| ArchonError::io(format!("marking {} executable", hook.display()), e))
}

/// Clone `source` to `mirror` under `root`, hard-linked, with no remote, branch,
/// hook or checkout, and moved into place whole so a crash leaves no half.
pub(super) fn clone(
    source: &Path,
    root: &Path,
    mirror: &Path,
    hooks: &Path,
) -> Result<(), ArchonError> {
    assert_eq!(mirror.parent(), Some(root), "mirrors sit in the root");
    let name = mirror.file_name().expect("a mirror has a name");
    let fresh = root.join(format!("{NEW_PREFIX}{}", name.to_string_lossy()));
    remove_entry(&fresh)?;
    remove_entry(mirror)?;
    let mut git = isolated_git();
    git.args(["clone", "--quiet", "--local", "--no-checkout"])
        .args(["--template=", "--"])
        .arg(source)
        .arg(&fresh)
        .current_dir(root);
    let output = git
        .output()
        .map_err(|e| ArchonError::io(format!("running git clone in {}", root.display()), e))?;
    if !output.status.success() {
        return Err(failure(root, &["clone", "--local"], &output));
    }
    run(&fresh, &["remote", "remove", "origin"])?;
    let branches = run(
        &fresh,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    )?;
    for branch in branches.lines() {
        run(&fresh, &["update-ref", "-d", branch])?;
    }
    run(&fresh, &["config", "core.hooksPath", path_str(hooks)?])?;
    assert!(
        run(&fresh, &["remote"])?.is_empty(),
        "a mirror has no remote"
    );
    std::fs::rename(&fresh, mirror)
        .map_err(|e| ArchonError::io(format!("moving the clone to {}", mirror.display()), e))
}

/// Fetch the repository's branches, and its base when that is no branch,
/// from its path. Incremental: only objects the mirror lacks are copied.
pub(super) fn fetch(mirror: &Path, source: &Path, base: &Base) -> Result<(), ArchonError> {
    let base_spec = base
        .source_ref
        .as_ref()
        .map(|source_ref| format!("+{source_ref}:{BASE_REF}"));
    let mut args = vec![
        "fetch",
        "--quiet",
        "--prune",
        "--no-recurse-submodules",
        "--",
    ];
    args.extend([path_str(source)?, FETCH_BRANCHES]);
    args.extend(base_spec.as_deref());
    run(mirror, &args).map(drop)
}

/// Check the mirror out detached at `commit`. Tracked files are reset to
/// it; untracked ones (a helper's notes, build output) are left.
pub(super) fn checkout(mirror: &Path, commit: &str) -> Result<(), ArchonError> {
    assert!(!commit.starts_with('-'), "a commit id is no option");
    run(
        mirror,
        &["checkout", "--quiet", "--force", "--detach", commit],
    )
    .map(drop)
}

/// The commit the mirror has checked out; `None` before its first checkout.
pub(super) fn head(mirror: &Path) -> Result<Option<String>, ArchonError> {
    let args = ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"];
    let output = output(mirror, &args)?;
    // `rev-parse --verify --quiet` exits 1 exactly when the name is unborn.
    match output.status.code() {
        Some(0) => Ok(Some(stdout(&output, &args)?)),
        Some(1) => Ok(None),
        _ => Err(failure(mirror, &args, &output)),
    }
}

/// Run the daemon's own git in `mirror` and return its trimmed stdout.
fn run(mirror: &Path, args: &[&str]) -> Result<String, ArchonError> {
    let output = output(mirror, args)?;
    if !output.status.success() {
        return Err(failure(mirror, args, &output));
    }
    stdout(&output, args)
}

/// Git on exactly this directory's `.git` and work tree: a `core.worktree`
/// written into its config cannot aim a checkout elsewhere.
fn output(mirror: &Path, args: &[&str]) -> Result<Output, ArchonError> {
    assert!(!args.is_empty(), "a git command needs arguments");
    assert!(mirror.is_absolute(), "mirror paths are absolute");
    isolated_git()
        .arg("--git-dir")
        .arg(mirror.join(".git"))
        .arg("--work-tree")
        .arg(mirror)
        .args(OWN_CONFIG)
        .args(args)
        .current_dir(mirror)
        .output()
        .map_err(|e| ArchonError::io(format!("running git in {}", mirror.display()), e))
}

/// Git that reads no global or system config, so no hook, filter or helper
/// the operator configured runs for a mirror, and that never prompts.
fn isolated_git() -> Command {
    let mut git = Command::new("git");
    git.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    git
}

fn stdout(output: &Output, args: &[&str]) -> Result<String, ArchonError> {
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| ArchonError::Invalid(format!("git {} printed non-UTF-8", args.join(" "))))?;
    Ok(text.trim().to_owned())
}

fn failure(dir: &Path, args: &[&str], output: &Output) -> ArchonError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ArchonError::io(
        format!("git {} for the mirror in {}", args.join(" "), dir.display()),
        std::io::Error::other(format!("{}: {stderr}", output.status)),
    )
}

fn path_str(path: &Path) -> Result<&str, ArchonError> {
    path.to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", path.display())))
}
