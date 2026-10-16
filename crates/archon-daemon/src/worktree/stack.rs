//! Merge-only stack maintenance through the shadow (ARCHITECTURE §13.4): a
//! parent is merged into a run branch with `merge-tree` and `commit-tree`,
//! the branch and a worktree it is checked out in are moved forward, and the
//! branch is pushed from the shadow. Nothing an agent configured runs.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use super::clean;
use super::daemon_git::{Config, DaemonGit, Ran, no_programs};
use super::shadow::Shadow;
use crate::gh_account::Token;

/// Who the daemon's merge commits are made by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub name: String,
    pub email: String,
}

/// What merging a parent into a branch came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Merge {
    /// The branch holds the parent already.
    Contained,
    /// The merge commit `to`, whose first parent is the branch's `from`.
    Merged { from: String, to: String },
    /// The files that conflict, one per line; nothing was written.
    Conflict(String),
}

pub(crate) fn is_commit_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit `refs/heads/<branch>` names in `repo`.
pub(crate) fn sha(repo: &Path, branch: &str) -> Result<String, ArchonError> {
    super::check_branch(branch)?;
    let spec = format!("refs/heads/{branch}^{{commit}}");
    let sha = DaemonGit::sealed_in(repo)
        .args(["rev-parse", "--verify", &spec])
        .text()?;
    if !is_commit_id(&sha) {
        return Err(ArchonError::Invalid(
            "git returned an invalid commit id".into(),
        ));
    }
    assert!(!sha.contains('\n'));
    Ok(sha)
}

/// Whether `rev` in `dir` holds the commit `sha`.
pub(crate) fn contains(dir: &Path, sha: &str, rev: &str) -> Result<bool, ArchonError> {
    assert!(is_commit_id(sha), "a stack parent is a commit id");
    assert!(
        !rev.is_empty() && !rev.starts_with('-'),
        "a revision, no option"
    );
    let args = ["merge-base", "--is-ancestor", sha, rev];
    let ran = DaemonGit::sealed_in(dir).args(args).output()?;
    match ran.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ran.failure()),
    }
}

/// Merge the commit `sha` into `branch` of `repo` without touching any work
/// tree: the result is a commit no ref names yet, or the conflicting files.
pub(crate) fn merge(
    shadow: &Shadow,
    repo: &Path,
    branch: &str,
    sha: &str,
    identity: &Identity,
) -> Result<Merge, ArchonError> {
    assert!(is_commit_id(sha), "a stack parent is a commit id");
    assert!(!identity.email.is_empty(), "a merge has an author");
    let ours = self::sha(repo, branch)?;
    if contains(repo, sha, &ours)? {
        return Ok(Merge::Contained);
    }
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        "-z",
    ];
    let ran = shadow
        .git(None, Config::Sealed)?
        .args(args)
        .args([ours.as_str(), sha])
        .output()?;
    let printed = String::from_utf8_lossy(&ran.stdout).into_owned();
    let mut fields = printed.split('\0').filter(|field| !field.is_empty());
    let tree = fields.next().unwrap_or_default().to_owned();
    // `merge-tree --write-tree` exits 1 exactly when the merge conflicts.
    match ran.status.code() {
        Some(0) if is_commit_id(&tree) => {}
        Some(1) => return Ok(Merge::Conflict(fields.collect::<Vec<_>>().join("\n"))),
        _ => return Err(ran.failure()),
    }
    let message = format!("Merge commit '{sha}' into {branch}");
    let to = shadow
        .git(None, Config::Sealed)?
        .env("GIT_AUTHOR_NAME", &identity.name)
        .env("GIT_AUTHOR_EMAIL", &identity.email)
        .env("GIT_COMMITTER_NAME", &identity.name)
        .env("GIT_COMMITTER_EMAIL", &identity.email)
        .args([
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            &ours,
            "-p",
            sha,
            "-m",
            &message,
        ])
        .text()?;
    assert!(is_commit_id(&to), "commit-tree prints the commit");
    Ok(Merge::Merged { from: ours, to })
}

/// Move `branch` of `repo` from `from` to `to`, and the worktree it is
/// checked out in with it; that worktree must be under `owned` and clean.
pub(crate) fn advance(
    shadow: &Shadow,
    repo: &Path,
    owned: &Path,
    branch: &str,
    from: &str,
    to: &str,
) -> Result<(), ArchonError> {
    assert!(is_commit_id(from) && is_commit_id(to), "commit ids");
    assert!(
        contains(repo, from, to)?,
        "a merge moves its branch forward"
    );
    let reference = format!("refs/heads/{branch}");
    let update = ["update-ref", "-m", "archon: restack", &reference, to, from];
    let Some(tree) = checked_out(repo, &reference)? else {
        return DaemonGit::sealed_in(repo).args(update).text().map(drop);
    };
    let owned = std::fs::canonicalize(owned)
        .map_err(|e| ArchonError::io(format!("resolving {}", owned.display()), e))?;
    if !tree.starts_with(&owned) {
        return Err(ArchonError::Invalid(format!(
            "`{branch}` is checked out in {}, which is no worktree of archond's",
            tree.display()
        )));
    }
    let index = clean::index_of(&tree)?;
    if !clean::is_clean(shadow, &tree, &index, from)? {
        return Err(ArchonError::Invalid(format!(
            "restack waits for a clean worktree: {} has uncommitted changes",
            tree.display()
        )));
    }
    checkout(shadow, &tree, &index, from, to)?;
    let Err(error) = DaemonGit::sealed_in(repo).args(update).text() else {
        return Ok(());
    };
    // The branch moved meanwhile: put the worktree back where the branch was.
    match checkout(shadow, &tree, &index, to, from) {
        Ok(()) => Err(error),
        Err(undo) => {
            Err(error.with_detail(&format!("restoring {} also failed: {undo}", tree.display())))
        }
    }
}

/// The canonical worktree of `repo` with `reference` checked out, if one has.
fn checked_out(repo: &Path, reference: &str) -> Result<Option<PathBuf>, ArchonError> {
    assert!(reference.starts_with("refs/heads/"), "a branch");
    let listed = DaemonGit::sealed_in(repo)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !listed.status.success() {
        return Err(listed.failure());
    }
    let text = String::from_utf8_lossy(&listed.stdout);
    let mut tree = None;
    for field in text.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            tree = Some(PathBuf::from(path));
        } else if field.strip_prefix("branch ") == Some(reference) {
            let tree = tree.ok_or_else(|| {
                ArchonError::Protocol("git worktree list named a branch before a worktree".into())
            })?;
            let found = std::fs::canonicalize(&tree)
                .map_err(|e| ArchonError::io(format!("resolving {}", tree.display()), e))?;
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Move the clean worktree `tree` and its index from `from` to `to`.
fn checkout(
    shadow: &Shadow,
    tree: &Path,
    index: &Path,
    from: &str,
    to: &str,
) -> Result<(), ArchonError> {
    assert!(is_commit_id(from) && is_commit_id(to), "commit ids");
    assert!(tree.is_absolute() && index.is_absolute(), "absolute paths");
    shadow
        .git(Some(tree), Config::Operator)?
        .env("GIT_INDEX_FILE", path_str(index)?)
        .args(["read-tree", "-m", "-u", from, to])
        .text()
        .map(drop)
}

/// Where `repo`'s `origin` takes a push, read as data: `None` without one.
pub(crate) fn origin_url(repo: &Path) -> Result<Option<String>, ArchonError> {
    for key in ["remote.origin.pushurl", "remote.origin.url"] {
        let ran = DaemonGit::sealed_in(repo)
            .args(["config", "--get", key])
            .output()?;
        // `git config --get` exits 1 exactly when the key is unset.
        match ran.status.code() {
            Some(0) => return check_url(&ran.stdout_text()?).map(Some),
            Some(1) => continue,
            _ => return Err(ran.failure()),
        }
    }
    Ok(None)
}

/// `url`, when git reaches it by ssh, https or as a local repository; a
/// remote helper (`<transport>::<address>`) or an option is refused.
fn check_url(url: &str) -> Result<String, ArchonError> {
    let helper = url
        .split_once("::")
        .is_some_and(|(transport, _)| !transport.contains('/'));
    if url.is_empty() || url.starts_with('-') || helper || url.contains('\n') {
        return Err(ArchonError::Invalid(format!(
            "`origin` is {url:?}, which archond does not push to or fetch from; use an ssh or \
             https URL"
        )));
    }
    Ok(url.to_owned())
}

/// Whether git reaches `url` as a repository on this machine, where its
/// receive-pack would run that repository's hooks.
fn is_local(url: &str) -> bool {
    if let Some((scheme, _)) = url.split_once("://") {
        return scheme == "file";
    }
    // `host:path` is ssh unless a slash comes before the colon.
    match url.split_once(':') {
        Some((host, _)) => host.contains('/'),
        None => true,
    }
}

/// Push `commit` to `refs/heads/<branch>` of `url`, never forced, from the
/// shadow with the operator's credentials, or only `token`'s when given; a
/// local receiver runs no hook.
pub(crate) fn push(
    shadow: &Shadow,
    url: &str,
    branch: &str,
    commit: &str,
    token: Option<&Token>,
) -> Result<Ran, ArchonError> {
    push_with(shadow, url, (branch, commit), None, token)
}

/// [`push`] of `commit` to `branch` only while `url` holds `expected` there,
/// which the receiver checks in the same update. The caller proves `commit`
/// descends from `expected`, so the update is a fast-forward.
pub(crate) fn push_from(
    shadow: &Shadow,
    url: &str,
    (branch, commit): (&str, &str),
    expected: &str,
) -> Result<Ran, ArchonError> {
    assert!(is_commit_id(expected), "a lease names a commit");
    assert_ne!(expected, commit, "an update moves the branch");
    let lease = format!("--force-with-lease=refs/heads/{branch}:{expected}");
    push_with(shadow, url, (branch, commit), Some(&lease))
}

/// [`push`], with `lease` the extra `--force-with-lease` argument, if any.
fn push_with(
    shadow: &Shadow,
    url: &str,
    (branch, commit): (&str, &str),
    lease: Option<&str>,
    token: Option<&Token>,
) -> Result<Ran, ArchonError> {
    super::check_branch(branch)?;
    assert!(
        lease.is_none_or(|l| l.starts_with("--force-with-lease=refs/heads/")),
        "only a lease on the pushed branch"
    );
    assert!(is_commit_id(commit), "the checked commit is pushed");
    let url = check_url(url)?;
    let receive_pack = match is_local(&url) {
        true => format!("git {} receive-pack", no_programs().join(" ")),
        false => "git-receive-pack".to_owned(),
    };
    assert!(
        !receive_pack.contains(['\'', '"', '\n']),
        "one shell word list"
    );
    let refspec = format!("{commit}:refs/heads/{branch}");
    let receive_pack = format!("--receive-pack={receive_pack}");
    let mut args = vec!["push", "--porcelain", "--no-verify", &receive_pack];
    args.extend(lease);
    args.extend([url.as_str(), &refspec]);
    let Some(token) = token else {
        let git = shadow.git(None, Config::Operator)?.network();
        return git.args(args).output();
    };
    // No operator config: no credential helper, URL rewrite or proxy of theirs.
    assert!(url.starts_with("https://github.com/"), "only github.com");
    let git = shadow.git(None, Config::Sealed)?.network();
    git.secret_config(&token.push_settings())
        .args(args)
        .output()
}

/// Make `sha`, the head of `branch` on `origin`, readable in `repo`. No ref
/// or `FETCH_HEAD` is written; the objects land in the repository's store.
pub(crate) fn fetch(
    shadow: &Shadow,
    repo: &Path,
    branch: &str,
    sha: &str,
) -> Result<(), ArchonError> {
    super::check_branch(branch)?;
    assert!(is_commit_id(sha), "a commit id");
    let spec = format!("{sha}^{{commit}}");
    if DaemonGit::sealed_in(repo)
        .args(["cat-file", "-e", &spec])
        .output()?
        .status
        .success()
    {
        return Ok(());
    }
    let Some(url) = origin_url(repo)? else {
        return Err(ArchonError::Invalid(format!(
            "{sha} is on no branch here and the repository has no `origin`"
        )));
    };
    let mut args = vec![
        "fetch".to_owned(),
        "--quiet".to_owned(),
        "--no-tags".to_owned(),
        "--no-write-fetch-head".to_owned(),
        "--no-recurse-submodules".to_owned(),
        "--upload-pack=git-upload-pack".to_owned(),
    ];
    // The shadow has no refs: the repository's branch heads say what it has.
    args.extend(
        local_heads(repo)?
            .into_iter()
            .map(|head| format!("--negotiation-tip={head}")),
    );
    args.extend([url, format!("refs/heads/{branch}")]);
    let fetched = shadow.git(None, Config::Operator)?.network();
    fetched.args(args.iter().map(String::as_str)).text()?;
    let found = DaemonGit::sealed_in(repo)
        .args(["rev-parse", "--verify", &spec])
        .text()?;
    if found != sha {
        return Err(ArchonError::Unavailable(format!(
            "`origin` no longer has {sha} on `{branch}`"
        )));
    }
    Ok(())
}

/// `origin`'s head of `branch`, its objects fetched into `repo` and recorded as
/// `refs/remotes/origin/<branch>`, as `git fetch` would; no other ref or work tree
/// is touched. `None` when the repository has no `origin`.
pub(crate) fn fetch_head(
    shadow: &Shadow,
    repo: &Path,
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    super::check_branch(branch)?;
    let Some(url) = origin_url(repo)? else {
        return Ok(None);
    };
    let Some(sha) = remote_head(shadow, &url, branch)? else {
        return Err(ArchonError::Invalid(format!(
            "`origin` has no branch `{branch}`"
        )));
    };
    fetch(shadow, repo, branch, &sha)?;
    let tracking = format!("refs/remotes/origin/{branch}");
    let update = ["update-ref", "-m", "archon: fetch", &tracking, &sha];
    DaemonGit::sealed_in(repo).args(update).text()?;
    assert!(is_commit_id(&sha), "ls-remote names a full commit id");
    Ok(Some(sha))
}

/// The commit `url` names as the head of `branch`; `None` when it has no such branch.
pub(crate) fn remote_head(
    shadow: &Shadow,
    url: &str,
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    super::check_branch(branch)?;
    let wanted = format!("refs/heads/{branch}");
    let args = ["ls-remote", "--upload-pack=git-upload-pack", url, &wanted];
    let listed = shadow
        .git(None, Config::Operator)?
        .network()
        .args(args)
        .text()?;
    let head = listed.lines().find_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        (name == wanted && is_commit_id(sha)).then(|| sha.to_owned())
    });
    debug_assert!(head.as_deref().is_none_or(is_commit_id));
    Ok(head)
}

/// Create `branch` on `origin` at `origin`'s head of `from` when `origin` has
/// no `branch`, never forced, as `token`'s login when given. `false`: it had
/// one, or there is no `origin`.
pub(crate) fn ensure_remote_branch(
    shadow: &Shadow,
    repo: &Path,
    (branch, from): (&str, &str),
    token: Option<&Token>,
) -> Result<bool, ArchonError> {
    super::check_branch(from)?;
    assert_ne!(
        branch, from,
        "validated: the integration branch is not the release branch"
    );
    let Some(url) = origin_url(repo)? else {
        return Ok(false);
    };
    if remote_head(shadow, &url, branch)?.is_some() {
        return Ok(false);
    }
    let Some(sha) = remote_head(shadow, &url, from)? else {
        return Err(ArchonError::Invalid(format!(
            "`origin` has neither `{branch}` nor `{from}` to create it from"
        )));
    };
    // The local `from` may be behind: the push needs `origin`'s commit here.
    fetch(shadow, repo, from, &sha)?;
    let push_to = match token {
        Some(token) => crate::gh_account::push_url(repo, token.user())?
            .map_err(|why| ArchonError::Invalid(format!("BLOCKED: {why}")))?,
        None => url.clone(),
    };
    create_remote_branch(shadow, (&push_to, &url), (branch, &sha), token)
}

/// Push `commit` as `branch` to `push_to`, which must not have it yet, as
/// `token`'s login when given; `read` is where to look for it after a failed
/// push. `false`: another creator made it first, and its branch is left as it is.
fn create_remote_branch(
    shadow: &Shadow,
    (push_to, read): (&str, &str),
    (branch, commit): (&str, &str),
    token: Option<&Token>,
) -> Result<bool, ArchonError> {
    assert!(is_commit_id(commit), "a commit id");
    assert!(
        token.is_none() || push_to.starts_with("https://github.com/"),
        "a token only goes to github.com"
    );
    // An empty expected value refuses a branch that exists, even a fast-forward.
    let lease = format!("--force-with-lease=refs/heads/{branch}:");
    let pushed = push_with(shadow, push_to, (branch, commit), Some(&lease), token)?;
    if pushed.status.success() {
        return Ok(true);
    }
    if remote_head(shadow, read, branch)?.is_some() {
        return Ok(false);
    }
    let Some(token) = token else {
        return Err(pushed.failure());
    };
    let said = token.redact(&pushed.stderr_text());
    let what = format!("git push creating `{branch}` as {}", token.user());
    Err(ArchonError::io(what, std::io::Error::other(said)))
}

/// Up to a few commits `repo`'s branches name, for a fetch to negotiate with.
fn local_heads(repo: &Path) -> Result<Vec<String>, ArchonError> {
    const MAX_TIPS: usize = 64;
    let args = [
        "for-each-ref",
        "--format=%(objectname)",
        "--sort=-committerdate",
        "refs/heads/",
        "refs/remotes/",
    ];
    let listed = DaemonGit::sealed_in(repo).args(args).text()?;
    let heads: Vec<String> = listed
        .lines()
        .filter(|l| is_commit_id(l))
        .take(MAX_TIPS)
        .map(str::to_owned)
        .collect();
    assert!(heads.len() <= MAX_TIPS);
    Ok(heads)
}

fn path_str(path: &Path) -> Result<&str, ArchonError> {
    path.to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_ssh_https_or_local_origin_is_used() {
        for url in [
            "git@github.com:o/r.git",
            "https://github.com/o/r",
            "/srv/r.git",
            "file:///srv/r",
        ] {
            assert_eq!(check_url(url).unwrap(), url);
        }
        for url in ["ext::sh -c x", "fd::3", "-uevil", "", "https://x\nmore"] {
            assert!(check_url(url).is_err(), "{url:?}");
        }
        assert!(is_local("/srv/r.git") && is_local("file:///srv/r") && is_local("./a:b"));
        assert!(!is_local("git@github.com:o/r.git") && !is_local("ssh://h/r"));
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@archon.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A branch another creator made between the absence check and the push,
    /// even at an ancestor of ours, is adopted and never moved.
    #[test]
    fn creating_a_remote_branch_never_moves_one_made_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, origin) = (dir.path().join("repo"), dir.path().join("origin.git"));
        run_git(dir.path(), &["init", "--quiet", "--bare", "origin.git"]);
        run_git(dir.path(), &["init", "--quiet", "-b", "main", "repo"]);
        run_git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "one"]);
        let first = run_git(&repo, &["rev-parse", "HEAD"]);
        run_git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "two"]);
        let second = run_git(&repo, &["rev-parse", "HEAD"]);
        let url = origin.to_str().unwrap();
        run_git(
            &repo,
            &["push", "--quiet", url, &format!("{first}:refs/heads/next")],
        );
        let shadow = Shadow::of(&dir.path().join("data"), &repo).unwrap();

        assert!(!create_remote_branch(&shadow, (url, url), ("next", &second), None).unwrap());
        assert_eq!(remote_head(&shadow, url, "next").unwrap(), Some(first));
        assert!(create_remote_branch(&shadow, (url, url), ("fresh", &second), None).unwrap());
        assert_eq!(remote_head(&shadow, url, "fresh").unwrap(), Some(second));
    }

    /// A tip that moved off the expected commit after it was read, even to
    /// an ancestor of the pushed one, refuses the update and is kept.
    #[test]
    fn a_push_from_a_commit_refuses_a_tip_that_moved_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, origin) = (dir.path().join("repo"), dir.path().join("origin.git"));
        run_git(dir.path(), &["init", "--quiet", "--bare", "origin.git"]);
        run_git(dir.path(), &["init", "--quiet", "-b", "main", "repo"]);
        let mut commits = Vec::new();
        for message in ["frozen", "between", "ours"] {
            run_git(
                &repo,
                &["commit", "--quiet", "--allow-empty", "-m", message],
            );
            commits.push(run_git(&repo, &["rev-parse", "HEAD"]));
        }
        let [frozen, between, ours] = commits.as_slice() else {
            unreachable!("three commits")
        };
        let url = origin.to_str().unwrap();
        let tip = |sha: &str| format!("{sha}:refs/heads/feature");
        run_git(&repo, &["push", "--quiet", url, &tip(between)]);
        let shadow = Shadow::of(&dir.path().join("data"), &repo).unwrap();

        let moved = push_from(&shadow, url, ("feature", ours), frozen).unwrap();
        assert!(!moved.status.success(), "the lease refuses it");
        assert_eq!(
            remote_head(&shadow, url, "feature").unwrap().as_ref(),
            Some(between)
        );
        run_git(&repo, &["push", "--quiet", "--force", url, &tip(frozen)]);
        let pushed = push_from(&shadow, url, ("feature", ours), frozen).unwrap();
        assert!(pushed.status.success(), "{pushed:?}");
        assert_eq!(
            remote_head(&shadow, url, "feature").unwrap().as_ref(),
            Some(ours)
        );
    }
}
