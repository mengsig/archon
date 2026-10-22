//! Whether a worktree holds anything moving or removing it would lose, asked
//! through the shadow (ARCHITECTURE §13.4): no filter the repository or an
//! agent configured runs, as one would under `git status`.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use super::daemon_git::{Config, DaemonGit};
use super::shadow::{Shadow, kind};
use super::stack::is_commit_id;

/// Git's mode of a submodule's entry in an index.
const GITLINK_MODE: &str = "160000";
/// Untracked build and tool caches that are never work, ignored or not: the
/// directories by name at any depth, `target` and Archon's own `.archon/`
/// scratch only at the top level.
const CACHE_PATTERNS: [&str; 12] = [
    "__pycache__/",
    "*.pyc",
    "*.pyo",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".hypothesis/",
    ".tox/",
    ".coverage",
    "node_modules/",
    "/target/",
    "/.archon/",
];

/// Why `git worktree remove` would refuse `tree` without `--force`, if it would: a submodule,
/// or changes to tracked or untracked files other than caches (built-in or under `cache_dirs`).
pub(crate) fn unremovable(
    shadow: &Shadow,
    tree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Option<String>, ArchonError> {
    assert!(tree.is_absolute(), "worktree paths are absolute");
    let index = index_of(tree)?;
    if let Some(submodule) = submodule_in(tree)? {
        return Ok(Some(format!("it holds the submodule `{submodule}`")));
    }
    let head = DaemonGit::sealed_in(tree)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .text()?;
    assert!(is_commit_id(&head), "rev-parse --verify prints a commit id");
    if !is_clean(shadow, tree, &index, &head)? || has_untracked(shadow, tree, &index, cache_dirs)? {
        return Ok(Some("it has uncommitted changes".to_owned()));
    }
    Ok(None)
}

/// The index file of the worktree `tree`.
pub(crate) fn index_of(tree: &Path) -> Result<PathBuf, ArchonError> {
    git_path(tree, "index")
}

/// Whether `tree`'s index and files are `head`'s. A submodule counts as
/// changed only when its commit moved: git is never run inside it.
pub(crate) fn is_clean(
    shadow: &Shadow,
    tree: &Path,
    index: &Path,
    head: &str,
) -> Result<bool, ArchonError> {
    assert!(is_commit_id(head), "a commit id");
    assert!(index.is_absolute(), "index paths are absolute");
    let index = path_str(index)?;
    let git = || -> Result<DaemonGit, ArchonError> {
        Ok(shadow
            .git(Some(tree), Config::Operator)?
            .env("GIT_INDEX_FILE", index))
    };
    git()?.args(["update-index", "-q", "--refresh"]).text()?;
    for args in [
        vec!["diff-files", "--quiet", "--ignore-submodules=dirty"],
        vec!["diff-index", "--cached", "--quiet", head],
    ] {
        let ran = git()?.args(args).output()?;
        match ran.status.code() {
            Some(0) => {}
            Some(1) => return Ok(false),
            _ => return Err(ran.failure()),
        }
    }
    Ok(true)
}

/// Whether `tree` has a file neither tracked, ignored (`.gitignore`, the repository's
/// `info/exclude` or the operator's), nor a cache ([`CACHE_PATTERNS`], `cache_dirs`).
fn has_untracked(
    shadow: &Shadow,
    tree: &Path,
    index: &Path,
    cache_dirs: &[PathBuf],
) -> Result<bool, ArchonError> {
    assert!(tree.is_absolute(), "worktree paths are absolute");
    assert!(
        index.is_absolute(),
        "GIT_INDEX_FILE is read relative to nothing"
    );
    let exclude = git_path(tree, "info/exclude")?;
    let exclude_from = format!("--exclude-from={}", path_str(&exclude)?);
    let caches = cache_excludes(cache_dirs)?;
    let mut args = vec![
        "ls-files",
        "-z",
        "--others",
        "--exclude-standard",
        "--directory",
        "--no-empty-directory",
    ];
    if kind(&exclude)?.is_some_and(|kind| kind.is_file()) {
        args.push(&exclude_from);
    }
    args.extend(caches.iter().map(String::as_str));
    let ran = shadow
        .git(Some(tree), Config::Operator)?
        .env("GIT_INDEX_FILE", path_str(index)?)
        .args(args)
        .output()?;
    if !ran.status.success() {
        return Err(ran.failure());
    }
    Ok(!ran.stdout.is_empty())
}

/// `--exclude` options for the built-in caches and each of `cache_dirs`,
/// anchored at the top of the worktree, with git's wildcards taken literally.
fn cache_excludes(cache_dirs: &[PathBuf]) -> Result<Vec<String>, ArchonError> {
    let mut excludes: Vec<String> = CACHE_PATTERNS
        .iter()
        .map(|pattern| format!("--exclude={pattern}"))
        .collect();
    for dir in cache_dirs {
        assert!(dir.is_relative(), "config validation keeps them relative");
        let mut pattern = String::from("/");
        for c in path_str(dir)?.trim_end_matches('/').chars() {
            if matches!(c, '*' | '?' | '[' | '\\') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        assert!(pattern.len() > 1, "config validation refuses an empty path");
        excludes.push(format!("--exclude={pattern}/"));
    }
    Ok(excludes)
}

/// A submodule `git worktree remove` refuses to delete: one whose git
/// directory the worktree's own keeps, or one populated in the tree.
fn submodule_in(tree: &Path) -> Result<Option<String>, ArchonError> {
    let modules = git_path(tree, "modules")?;
    if kind(&modules)?.is_some_and(|kind| kind.is_dir()) {
        return Ok(Some(modules.display().to_string()));
    }
    assert!(modules.is_absolute(), "--path-format=absolute");
    let listed = DaemonGit::sealed_in(tree)
        .args(["ls-files", "--stage", "-z"])
        .output()?;
    if !listed.status.success() {
        return Err(listed.failure());
    }
    for entry in listed.stdout.split(|&b| b == 0) {
        let entry = String::from_utf8_lossy(entry);
        let Some((stage, path)) = entry.split_once('\t') else {
            continue;
        };
        let gitlink = stage.split(' ').next() == Some(GITLINK_MODE);
        if gitlink && kind(&tree.join(path).join(".git"))?.is_some() {
            return Ok(Some(path.to_owned()));
        }
    }
    Ok(None)
}

/// The absolute path of `name` in `tree`'s git directory, as git resolves it.
fn git_path(tree: &Path, name: &str) -> Result<PathBuf, ArchonError> {
    let args = ["rev-parse", "--path-format=absolute", "--git-path", name];
    let path = PathBuf::from(DaemonGit::sealed_in(tree).args(args).text()?);
    assert!(path.is_absolute(), "--path-format=absolute");
    Ok(path)
}

fn path_str(path: &Path) -> Result<&str, ArchonError> {
    path.to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_git_would_refuse_to_remove_is_found_without_running_a_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            DaemonGit::sealed_in(&repo)
                .args(args.to_vec())
                .text()
                .unwrap()
        };
        git(&["init", "--quiet"]);
        std::fs::write(repo.join(".gitattributes"), "* filter=planted\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
        git(&["add", "."]);
        let who = ["-c", "user.name=a", "-c", "user.email=a@example.invalid"];
        git(&[&who[..], &["commit", "--quiet", "-m", "x"]].concat());
        let marker = root.join("ran");
        git(&[
            "config",
            "filter.planted.clean",
            &format!("touch {}", marker.display()),
        ]);
        let shadow = Shadow::of(&root.join("data"), &repo).unwrap();
        let why = || unremovable(&shadow, &repo, &[]).unwrap();

        std::fs::write(repo.join("ignored"), "x").unwrap();
        assert_eq!(why(), None, "an ignored file is no change");
        std::fs::write(repo.join("new.txt"), "x").unwrap();
        assert_eq!(why().as_deref(), Some("it has uncommitted changes"));
        std::fs::remove_file(repo.join("new.txt")).unwrap();
        std::fs::write(repo.join(".gitignore"), "changed\n").unwrap();
        assert_eq!(why().as_deref(), Some("it has uncommitted changes"));
        std::fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
        assert_eq!(why(), None, "the same content again is no change");
        assert!(!marker.exists(), "the repository's filter never ran");

        let head = git(&["rev-parse", "HEAD"]);
        git(&["config", "--unset", "filter.planted.clean"]);
        git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},sub"),
        ]);
        std::fs::create_dir_all(repo.join("sub/.git")).unwrap();
        assert_eq!(why().as_deref(), Some("it holds the submodule `sub`"));
    }

    #[test]
    fn untracked_caches_are_no_change_but_untracked_source_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        let git = |args: &[&str]| {
            let git = DaemonGit::sealed_in(&repo);
            git.args(args.to_vec()).text().unwrap()
        };
        git(&["init", "--quiet"]);
        std::fs::write(repo.join("pkg/mod.py"), "x = 1\n").unwrap();
        git(&["add", "."]);
        let who = ["-c", "user.name=a", "-c", "user.email=a@example.invalid"];
        git(&[&who[..], &["commit", "--quiet", "-m", "x"]].concat());
        let shadow = Shadow::of(&root.join("data"), &repo).unwrap();
        let build = [PathBuf::from("web/build")];
        let why = || unremovable(&shadow, &repo, &build).unwrap();
        let write = |path: &str| {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "cache").unwrap();
        };
        for cache in [
            "pkg/__pycache__/mod.cpython-313.pyc",
            "pkg/stray.pyc",
            ".pytest_cache/v/cache/lastfailed",
            ".mypy_cache/3.13/pkg.json",
            "web/node_modules/left-pad/index.js",
            "target/debug/app",
            "web/build/app.js",
            ".archon/scratch/notes.json",
        ] {
            write(cache);
        }
        assert_eq!(why(), None, "untracked caches are no change");
        assert_eq!(
            unremovable(&shadow, &repo, &[]).unwrap().as_deref(),
            Some("it has uncommitted changes"),
            "web/build is a cache only where it is configured"
        );

        for work in [
            "pkg/new.py",
            "pkg/target/kept.py",
            "build/app.js",
            "pkg/.archon/x.py",
        ] {
            write(work);
            assert_eq!(
                why().as_deref(),
                Some("it has uncommitted changes"),
                "{work}"
            );
            std::fs::remove_file(repo.join(work)).unwrap();
        }
        std::fs::write(repo.join("pkg/mod.py"), "x = 2\n").unwrap();
        assert_eq!(why().as_deref(), Some("it has uncommitted changes"));
    }
}
