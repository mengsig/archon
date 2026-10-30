//! Delivery into an existing pull request, the one an `onto` item froze at
//! enqueue: the checked commit goes onto its head branch by fast-forward
//! only, one delivery per pull request at a time, and counts once GitHub
//! shows it as that pull request's head. Its title and body stay its
//! owner's, and nothing is merged.
//!
//! As in `super::remote`, an inner `Err(String)` is a refusal no retry
//! changes; the outer error is a failure worth trying again.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError};

use archon_core::error::ArchonError;
use archon_core::pull_request::{OntoPullRequest, PullRequestRef};
use archon_core::repo::{Repo, github_slug};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::super::gh;
use crate::context::DaemonContext;
use crate::gh_account::{self, Token};
use crate::worktree::daemon_git::{DaemonGit, Ran};
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// One lock per pull request delivered into, kept while anyone holds or
/// awaits it.
#[derive(Debug, Default)]
pub(in crate::executor) struct Locks {
    by_pr: std::sync::Mutex<BTreeMap<PullRequestRef, Arc<Mutex<()>>>>,
}

impl Locks {
    /// Hold `pr`'s lock until the guard drops; another delivery into `pr` waits.
    pub(in crate::executor) async fn hold(&self, pr: &PullRequestRef) -> OwnedMutexGuard<()> {
        let lock = {
            let mut by_pr = self.by_pr.lock().unwrap_or_else(PoisonError::into_inner);
            // An entry only this map refers to is neither held nor awaited.
            by_pr.retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(by_pr.entry(pr.clone()).or_default())
        };
        assert!(Arc::strong_count(&lock) >= 2, "the map keeps it while held");
        lock.lock_owned().await
    }
}

/// Push `commit` onto the head branch of `onto`'s pull request, which must
/// still be at its frozen head or one `commit` holds, as the repository's
/// GitHub login when it has one, and return that pull request's URL once
/// GitHub shows `commit` as its head.
pub(super) async fn deliver(
    ctx: &DaemonContext,
    repo: &Repo,
    onto: &OntoPullRequest,
    commit: &str,
) -> Result<Result<String, String>, ArchonError> {
    assert!(stack::is_commit_id(commit), "the checked commit");
    assert!(stack::is_commit_id(&onto.head_sha), "frozen at enqueue");
    assert_ne!(
        commit, onto.head_sha,
        "the checks proved commits of its own"
    );
    if onto.review_only || onto.is_fork() {
        return Ok(Err(format!(
            "the item was queued to review {}, whose head Archon never pushes to; queue an \
             item that `depends_on` it to build on that head in a pull request of its own",
            onto.pr.url()
        )));
    }
    let binary = &ctx.config.github.binary;
    let token = match gh::token(binary, repo).await {
        Ok(token) => token,
        Err(ArchonError::Invalid(blocked)) => return Ok(Err(blocked)),
        Err(error) => return Err(error),
    };
    let _held = ctx.coordination.onto.hold(&onto.pr).await;
    let data_dir = &ctx.paths.data_dir;
    if let Err(refusal) = fast_forward(data_dir, &repo.path, onto, commit, token).await? {
        return Ok(Err(refusal));
    }
    confirm(binary, repo, onto, commit).await
}

/// Push `commit` to `onto.head_ref` unless `origin` holds it there already,
/// as `token`'s login when given; refused when that branch moved off the
/// frozen head to one `commit` lacks. Never forced.
async fn fast_forward(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
    commit: &str,
    token: Option<Token>,
) -> Result<Result<(), String>, ArchonError> {
    let (data_dir, repo) = (data_dir.to_owned(), repo.to_owned());
    let (onto, commit) = (onto.clone(), commit.to_owned());
    let push = move || fast_forward_now(&data_dir, &repo, &onto, &commit, token.as_ref());
    tokio::task::spawn_blocking(push)
        .await
        .map_err(|e| ArchonError::io("the push task", std::io::Error::other(e)))?
}

/// [`fast_forward`], blocking. The push carries a lease on the head read
/// here, so `origin` refuses it when its tip moved after that.
fn fast_forward_now(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
    commit: &str,
    token: Option<&Token>,
) -> Result<Result<(), String>, ArchonError> {
    let frozen = &onto.head_sha;
    if !descends(repo, frozen, commit)? {
        return Ok(Err(format!(
            "{commit} does not descend from {frozen}, the frozen head of {}, so it is no \
             fast-forward of it",
            onto.pr.url()
        )));
    }
    let url = match origin(repo)? {
        Ok(url) => url,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let Some(refusal) = elsewhere(&url, onto) {
        return Ok(Err(refusal));
    }
    let shadow = Shadow::of(data_dir, repo)?;
    let push_to = match push_destination(repo, &shadow, &url, onto, token)? {
        Ok(push_to) => push_to,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let branch = onto.head_ref.as_str();
    // Read where and as whom the push goes.
    let head = || stack::remote_head_as(&shadow, &push_to, branch, token);
    let lease = match head()? {
        // An earlier try pushed it and failed after.
        Some(head) if head == commit => return Ok(Ok(())),
        Some(head) if head == *frozen => head,
        // The branch merged the moved head already, so this is still a fast-forward.
        Some(head) if holds(repo, &head, commit)? => head,
        Some(head) => return Ok(Err(moved(onto, &head))),
        None => return Ok(Err(gone(onto))),
    };
    let pushed = stack::push_from(&shadow, &push_to, (branch, commit), &lease, token)?;
    if pushed.status.success() {
        return Ok(Ok(()));
    }
    match head()? {
        Some(head) if head == commit => Ok(Ok(())),
        Some(head) if head != lease && head != *frozen => Ok(Err(moved(onto, &head))),
        None => Ok(Err(gone(onto))),
        Some(_) => Ok(Err(push_refused(branch, &pushed, token)?)),
    }
}

/// Where the push of `onto`'s branch goes: `url` read through `origin` as
/// configured, or `token`'s login's URL of `origin`, which `repo`'s config is
/// read again for. Refused unless git sends it to `onto`'s head repository.
fn push_destination(
    repo: &Path,
    shadow: &Shadow,
    url: &str,
    onto: &OntoPullRequest,
    token: Option<&Token>,
) -> Result<Result<String, String>, ArchonError> {
    assert!(!url.is_empty(), "a checked origin URL");
    let Some(token) = token else {
        // The push reads the operator's config: check where git sends it.
        let sent_to = stack::pinned_url(shadow, url)?;
        return Ok(elsewhere(&sent_to, onto).map_or(Ok(url.to_owned()), Err));
    };
    let push_to = match gh_account::push_url(repo, token.user())? {
        Ok(push_to) => push_to,
        Err(why) => return Ok(Err(format!("BLOCKED: {why}"))),
    };
    assert!(
        push_to.starts_with("https://github.com/"),
        "only github.com"
    );
    // `origin` may have changed since `url` was read and checked.
    Ok(elsewhere(&push_to, onto).map_or(Ok(push_to), Err))
}

/// Why `origin`, still at the frozen head, refused the push: a rule or access
/// only the operator can change, else a failure worth trying again. What git
/// printed is shown without `token`.
fn push_refused(branch: &str, pushed: &Ran, token: Option<&Token>) -> Result<String, ArchonError> {
    assert!(!pushed.status.success(), "only a refused push");
    let shown = |text: String| match token {
        Some(token) => token.redact(&text),
        None => text,
    };
    let said = shown(pushed.stderr_text());
    let stdout = shown(String::from_utf8_lossy(&pushed.stdout).into_owned());
    let doing = format!("pushing `{branch}` to `origin`");
    if let Some(line) = stdout
        .lines()
        .find(|line| line.contains("[remote rejected]"))
    {
        return Ok(format!(
            "BLOCKED: `origin` refused the push of `{branch}`: {line}"
        ));
    }
    match gh::needs_operator(&doing, &said) {
        Some(blocked) => Ok(blocked),
        None => Err(ArchonError::Unavailable(format!(
            "git push of `{branch}` failed: {said}"
        ))),
    }
}

/// Whether `commit` holds `ancestor`, as `repo` knows them.
fn descends(repo: &Path, ancestor: &str, commit: &str) -> Result<bool, ArchonError> {
    assert!(stack::is_commit_id(ancestor) && stack::is_commit_id(commit));
    let ran = DaemonGit::sealed_in(repo)
        .args(["merge-base", "--is-ancestor", ancestor, commit])
        .output()?;
    // `merge-base --is-ancestor` exits 1 exactly when it is not one.
    match ran.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ran.failure()),
    }
}

/// Whether `commit` holds `head`, a commit `repo` may not have at all.
fn holds(repo: &Path, head: &str, commit: &str) -> Result<bool, ArchonError> {
    assert!(stack::is_commit_id(head), "checked by its caller");
    let spec = format!("{head}^{{commit}}");
    let known = DaemonGit::sealed_in(repo)
        .args(["cat-file", "-e", &spec])
        .output()?;
    Ok(known.status.success() && descends(repo, head, commit)?)
}

/// `origin`'s URL, or a refusal when the repository has none archond may use.
fn origin(repo: &Path) -> Result<Result<String, String>, ArchonError> {
    match stack::origin_url(repo) {
        Ok(Some(url)) => {
            assert!(!url.is_empty(), "checked by origin_url");
            Ok(Ok(url))
        }
        Ok(None) => Ok(Err(
            "BLOCKED: the repository has no `origin` remote to push to".into(),
        )),
        Err(ArchonError::Invalid(why)) => Ok(Err(format!("BLOCKED: {why}"))),
        Err(error) => Err(error),
    }
}

/// `onto`'s URL once GitHub shows `commit` as the head of that open pull
/// request; one still showing a head `commit` holds, as the frozen head or
/// a moved head it was pushed over, lags behind the push and is retried.
async fn confirm(
    binary: &Path,
    repo: &Repo,
    onto: &OntoPullRequest,
    commit: &str,
) -> Result<Result<String, String>, ArchonError> {
    let url = onto.pr.url();
    let viewed = match gh::view_headed(binary, repo, &url).await {
        Ok(viewed) => viewed,
        Err(ArchonError::Invalid(blocked)) => return Ok(Err(blocked)),
        Err(error) => return Err(error),
    };
    assert_eq!(viewed.pr.identity(), onto.pr, "checked by view_headed");
    if !viewed.pr.is_open() || viewed.pr.head_ref_name != onto.head_ref {
        return Ok(Err(format!(
            "pushed {commit} to `{}`, but {url} is {} with head `{}` now, so it does not carry \
             the change; queue the work again without `onto` or onto an open pull request",
            onto.head_ref, viewed.pr.state, viewed.pr.head_ref_name
        )));
    }
    let Some(head) = viewed.pr.head_ref_oid else {
        return Err(ArchonError::Unavailable(format!(
            "GitHub gave no head commit for {url}"
        )));
    };
    if head == commit {
        return Ok(Ok(url));
    }
    if head == onto.head_sha || held_by(&repo.path, &head, commit).await? {
        return Err(ArchonError::Unavailable(format!(
            "GitHub still shows {head} as the head of {url} after the push of {commit}"
        )));
    }
    Ok(Err(format!(
        "pushed {commit} to `{}`, but {url} shows head {head}: someone pushed after it",
        onto.head_ref
    )))
}

/// [`holds`], off the async runtime: whether `commit` holds GitHub's `head`.
async fn held_by(repo: &Path, head: &str, commit: &str) -> Result<bool, ArchonError> {
    if !stack::is_commit_id(head) {
        return Err(ArchonError::Protocol(format!(
            "GitHub gave {head:?} as a head commit"
        )));
    }
    let (repo, head, commit) = (repo.to_owned(), head.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || holds(&repo, &head, &commit))
        .await
        .map_err(|e| ArchonError::io("the ancestry task", std::io::Error::other(e)))?
}

/// Why `onto`'s pull request is not updated: `origin` is not provably the
/// github.com repository holding its head. A path on this machine is trusted.
fn elsewhere(url: &str, onto: &OntoPullRequest) -> Option<String> {
    assert!(!onto.head_repository.is_empty(), "frozen at enqueue");
    let Some(slug) = github_slug(url) else {
        if stack::is_local(url) {
            return None;
        }
        // The URL itself is not shown: it may carry credentials.
        return Some(format!(
            "BLOCKED: `origin` is no plain github.com URL (https://github.com/{0}.git or \
             git@github.com:{0}.git), so Archon cannot tell it holds the head of {1}; nothing \
             was pushed",
            onto.head_repository,
            onto.pr.url()
        ));
    };
    if slug.eq_ignore_ascii_case(&onto.head_repository) {
        return None;
    }
    Some(format!(
        "BLOCKED: `origin` is `{slug}` now, but the head of {} lives in `{}`; nothing was \
         pushed. Point `origin` back at `{}`, or queue the work again without `onto`",
        onto.pr.url(),
        onto.head_repository,
        onto.head_repository
    ))
}

/// Why `onto`'s pull request is not updated: its head branch is gone.
fn gone(onto: &OntoPullRequest) -> String {
    format!(
        "`origin` has no branch `{}` any more, the head of {}; nothing was pushed",
        onto.head_ref,
        onto.pr.url()
    )
}

/// Why `onto`'s pull request is not updated: its head left the frozen one.
fn moved(onto: &OntoPullRequest, head: &str) -> String {
    assert_ne!(head, onto.head_sha, "only a head that moved");
    format!(
        "the head of {} moved from {} (frozen when the item was queued) to {head}, so the change \
         is no fast-forward of it; Archon never force-pushes. Refresh: run `refresh_item` on \
         the item so its branch merges {head} and the change is delivered as a fast-forward",
        onto.pr.url(),
        onto.head_sha
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn pr(number: u64) -> PullRequestRef {
        PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{number}")).unwrap()
    }

    #[tokio::test]
    async fn deliveries_into_one_pull_request_take_turns_and_others_do_not_wait() {
        let locks = Arc::new(Locks::default());
        let first = locks.hold(&pr(100)).await;

        let waiting = tokio::spawn({
            let locks = Arc::clone(&locks);
            async move { drop(locks.hold(&pr(100)).await) }
        });
        let other = tokio::time::timeout(Duration::from_secs(5), locks.hold(&pr(101))).await;
        assert!(other.is_ok(), "another pull request is not held up");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiting.is_finished(),
            "the second delivery into #100 waits"
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(5), waiting).await;
        assert!(second.is_ok(), "it goes on once the first let go");
        drop(other);
        let _again = locks.hold(&pr(102)).await;
        let kept = locks.by_pr.lock().unwrap().len();
        assert_eq!(kept, 1, "only a held lock is kept");
    }

    fn frozen() -> OntoPullRequest {
        OntoPullRequest {
            target: archon_core::pull_request::OntoTarget::Pr(100),
            pr: pr(100),
            node_id: None,
            head_repository: "o/r".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            head_sha: "a".repeat(40),
            review_only: false,
        }
    }

    #[test]
    fn a_push_goes_only_to_the_repository_holding_the_frozen_head() {
        let onto = frozen();
        for same in [
            "https://github.com/o/r.git",
            "git@github.com:O/R",
            "/srv/origin.git",
        ] {
            assert_eq!(elsewhere(same, &onto), None, "{same}");
        }
        for fork in [
            "https://github.com/fork/r.git",
            "git@github.com:o/mirror.git",
        ] {
            let said = elsewhere(fork, &onto).expect(fork);
            assert!(
                said.starts_with("BLOCKED:") && said.contains("`o/r`"),
                "{said}"
            );
        }
        let unparsed = [
            "https://github.com:443/fork/r.git",
            "https://user:secret@github.com/o/r.git",
            "github-alias:o/r.git",
        ];
        for url in unparsed {
            let said = elsewhere(url, &onto).expect(url);
            assert!(
                said.starts_with("BLOCKED:") && !said.contains("secret"),
                "{said}"
            );
        }
    }

    #[test]
    fn a_moved_head_names_both_commits_and_never_forces() {
        let onto = frozen();
        let said = moved(&onto, &"b".repeat(40));
        assert!(
            said.contains(&"a".repeat(40)) && said.contains(&"b".repeat(40)),
            "{said}"
        );
        assert!(
            said.contains("never force-pushes") && said.contains("Refresh"),
            "{said}"
        );
    }

    /// `origin` turned to a fork after its URL was checked: the URL the
    /// token's push would take is checked again, so nothing goes to the fork.
    #[test]
    fn a_token_push_is_refused_when_origin_moved_to_another_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        let git = |args: &[&str]| {
            let mut ran = std::process::Command::new("git");
            let ran = ran.arg("-C").arg(&repo).args(args);
            assert!(ran.status().unwrap().success(), "git {args:?}");
        };
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "--quiet"]);
        let checked = "https://github.com/o/r.git";
        git(&["remote", "add", "origin", "https://github.com/fork/r.git"]);
        let shadow = Shadow::of(&dir.path().join("data"), &repo).unwrap();
        let user = "devfake".parse().unwrap();
        let token = Token::read(&user, &"t".repeat(40)).unwrap();
        let onto = frozen();

        let moved = push_destination(&repo, &shadow, checked, &onto, Some(&token)).unwrap();
        let refusal = moved.expect_err("the fork is refused");
        assert!(refusal.contains("`fork/r`"), "{refusal}");
        git(&["remote", "set-url", "origin", checked]);
        let back = push_destination(&repo, &shadow, checked, &onto, Some(&token)).unwrap();
        assert_eq!(back, Ok(checked.to_owned()));
    }
}
