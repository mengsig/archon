//! Merge-only stack maintenance, confined to registered daemon worktrees.

use super::{git, git_failure, git_output, head};
use archon_core::error::ArchonError;
use std::path::Path;

pub(crate) fn sha(repo: &Path, branch: &str) -> Result<String, ArchonError> {
    super::check_branch(branch)?;
    let sha = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )?;
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ArchonError::Invalid(
            "git returned an invalid commit id".into(),
        ));
    }
    assert!(!sha.contains('\n'));
    assert!(sha.is_ascii());
    Ok(sha)
}

pub(crate) fn merge(
    repo: &Path,
    tree: &Path,
    branch: &str,
    sha: &str,
) -> Result<Option<String>, ArchonError> {
    let common = super::team::identity::common_dir(repo)?;
    super::team::identity::check_worktree(tree, &common)?;
    if !head(tree)?.is_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "restack worktree is not on {branch}"
        )));
    }
    assert_ne!(tree, repo, "never merge in the operator checkout");
    assert!(matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()));
    if contains(tree, sha)? {
        return Ok(None);
    }
    let conflicts = git(tree, &["diff", "--name-only", "--diff-filter=U"])?;
    if !conflicts.is_empty() {
        let merging = git(tree, &["rev-parse", "--verify", "MERGE_HEAD"])?;
        if merging != sha {
            return Err(ArchonError::Invalid(
                "child has a different merge in progress".into(),
            ));
        }
        return Ok(Some(conflicts));
    }
    if !git(tree, &["status", "--porcelain"])?.is_empty() {
        return Err(ArchonError::Invalid(
            "restack waits for a clean child worktree".into(),
        ));
    }
    let args = [
        "-c",
        "core.hooksPath=/dev/null",
        "merge",
        "--no-edit",
        "--no-gpg-sign",
        sha,
    ];
    let output = git_output(tree, &args)?;
    if output.status.success() {
        return Ok(None);
    }
    let conflicts = git(tree, &["diff", "--name-only", "--diff-filter=U"])?;
    if conflicts.is_empty() {
        return Err(git_failure(tree, &args, &output));
    }
    Ok(Some(conflicts))
}

pub(crate) fn push(tree: &Path, branch: &str) -> Result<(), ArchonError> {
    super::check_branch(branch)?;
    assert!(head(tree)?.is_branch(branch));
    assert!(git(tree, &["diff", "--name-only", "--diff-filter=U"])?.is_empty());
    git(
        tree,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "origin",
            &format!("HEAD:refs/heads/{branch}"),
        ],
    )?;
    Ok(())
}

pub(crate) fn fetch_head(repo: &Path, branch: &str, sha: &str) -> Result<(), ArchonError> {
    super::check_branch(branch)?;
    assert!(matches!(sha.len(), 40 | 64));
    assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
    let exists = git_output(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")])?;
    if exists.status.success() {
        return Ok(());
    }
    let reference = format!("refs/heads/{branch}");
    git(
        repo,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--no-tags",
            "origin",
            &reference,
        ],
    )?;
    let found = git(
        repo,
        &["rev-parse", "--verify", &format!("{sha}^{{commit}}")],
    )?;
    assert_eq!(found, sha);
    Ok(())
}

pub(crate) fn contains(tree: &Path, sha: &str) -> Result<bool, ArchonError> {
    assert!(matches!(sha.len(), 40 | 64));
    assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
    let args = ["merge-base", "--is-ancestor", sha, "HEAD"];
    let output = git_output(tree, &args)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(tree, &args, &output)),
    }
}
