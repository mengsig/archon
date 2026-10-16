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
use archon_core::repo::Repo;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::super::gh;
use crate::context::DaemonContext;
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
/// still be at its frozen head, and return that pull request's URL once
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
    let _held = ctx.coordination.onto.hold(&onto.pr).await;
    let data_dir = &ctx.paths.data_dir;
    if let Err(refusal) = fast_forward(data_dir, &repo.path, onto, commit).await? {
        return Ok(Err(refusal));
    }
    confirm(&ctx.config.github.binary, &repo.path, onto, commit).await
}

/// Push `commit` to `onto.head_ref` unless `origin` holds it there already;
/// refused when that branch moved off the frozen head. Never forced.
async fn fast_forward(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
    commit: &str,
) -> Result<Result<(), String>, ArchonError> {
    let (data_dir, repo) = (data_dir.to_owned(), repo.to_owned());
    let (onto, commit) = (onto.clone(), commit.to_owned());
    tokio::task::spawn_blocking(move || fast_forward_now(&data_dir, &repo, &onto, &commit))
        .await
        .map_err(|e| ArchonError::io("the push task", std::io::Error::other(e)))?
}

/// [`fast_forward`], blocking. The push carries a lease on the frozen head,
/// so `origin` refuses it when its tip moved after it was read here.
fn fast_forward_now(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
    commit: &str,
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
    let shadow = Shadow::of(data_dir, repo)?;
    let branch = onto.head_ref.as_str();
    match stack::remote_head(&shadow, &url, branch)? {
        // An earlier try pushed it and failed after.
        Some(head) if head == commit => return Ok(Ok(())),
        Some(head) if head == *frozen => {}
        Some(head) => return Ok(Err(moved(onto, &head))),
        None => return Ok(Err(gone(onto))),
    }
    let pushed = stack::push_from(&shadow, &url, (branch, commit), frozen)?;
    if pushed.status.success() {
        return Ok(Ok(()));
    }
    match stack::remote_head(&shadow, &url, branch)? {
        Some(head) if head == commit => Ok(Ok(())),
        Some(head) if head != *frozen => Ok(Err(moved(onto, &head))),
        None => Ok(Err(gone(onto))),
        Some(_) => Ok(Err(push_refused(branch, &pushed)?)),
    }
}

/// Why `origin`, still at the frozen head, refused the push: a rule or access
/// only the operator can change, else a failure worth trying again.
fn push_refused(branch: &str, pushed: &Ran) -> Result<String, ArchonError> {
    assert!(!pushed.status.success(), "only a refused push");
    let said = pushed.stderr_text();
    let stdout = String::from_utf8_lossy(&pushed.stdout);
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
/// request; a pull request still showing the frozen head is retried.
async fn confirm(
    binary: &Path,
    repo: &Path,
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
    match viewed.pr.head_ref_oid.as_deref() {
        Some(head) if head == commit => Ok(Ok(url)),
        Some(head) if head == onto.head_sha => Err(ArchonError::Unavailable(format!(
            "GitHub still shows {head} as the head of {url} after the push of {commit}"
        ))),
        Some(head) => Ok(Err(format!(
            "pushed {commit} to `{}`, but {url} shows head {head}: someone pushed after it",
            onto.head_ref
        ))),
        None => Err(ArchonError::Unavailable(format!(
            "GitHub gave no head commit for {url}"
        ))),
    }
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
         is no fast-forward of it; Archon never force-pushes. Refresh: queue the work again \
         `onto` that pull request so it starts from {head}",
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

    #[test]
    fn a_moved_head_names_both_commits_and_never_forces() {
        let onto = OntoPullRequest {
            target: archon_core::pull_request::OntoTarget::Pr(100),
            pr: pr(100),
            node_id: None,
            head_repository: "o/r".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            head_sha: "a".repeat(40),
        };
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
}
