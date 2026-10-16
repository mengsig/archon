//! An item's `onto` target: the existing pull request it delivers into,
//! resolved and frozen at enqueue, and where its worktree starts.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::repo::{Repo, ShipMode};
use archon_queue::Store;

use super::dependencies::is_plain_branch;
use super::gh::{self, Headed};
use crate::context::DaemonContext;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// The open pull request `target` names in `repo`, frozen as GitHub shows it
/// now. Refused when it is closed or merged, or its head is in a fork or a
/// repository the operator cannot push to.
pub(crate) async fn freeze(
    ctx: &DaemonContext,
    repo: &Repo,
    target: &OntoTarget,
) -> Result<OntoPullRequest, ArchonError> {
    if repo.ship != ShipMode::Pr {
        return Err(ArchonError::Invalid(format!(
            "`onto` delivers into a pull request, but `{}` ships locally",
            repo.name
        )));
    }
    let binary = &ctx.config.github.binary;
    let headed = match target {
        OntoTarget::Pr(0) => {
            return Err(ArchonError::Invalid(
                "pull request #0 does not exist".into(),
            ));
        }
        OntoTarget::Pr(number) => gh::view_headed(binary, &repo.path, &number.to_string()).await?,
        OntoTarget::Item(id) => {
            let pr = item_pull_request(&*ctx.store.lock().await, repo, id)?;
            gh::view_headed(binary, &repo.path, &pr.url()).await?
        }
        OntoTarget::Branch(branch) => branch_pull_request(binary, &repo.path, branch).await?,
    };
    let frozen = frozen(target, headed)?;
    if !gh::can_push(binary, &repo.path, &frozen.head_repository).await? {
        let why = format!("you have no write access to `{}`", frozen.head_repository);
        return Err(refused(&frozen.pr.url(), &why));
    }
    assert!(stack::is_commit_id(&frozen.head_sha), "checked by frozen");
    assert!(is_plain_branch(&frozen.head_ref), "checked by gh");
    Ok(frozen)
}

/// The pull request item `id` of `repo` delivered to: the one it was itself
/// queued onto, else its newest run's.
fn item_pull_request(
    store: &Store,
    repo: &Repo,
    id: &QueueItemId,
) -> Result<PullRequestRef, ArchonError> {
    let item = store.item(id)?;
    if item.repo.as_ref() != Some(&repo.id) {
        return Err(ArchonError::Invalid(format!(
            "item {id} is not in `{}`, so its pull request cannot be delivered into from there",
            repo.name
        )));
    }
    if let Some(onto) = item.onto {
        return Ok(onto.pr);
    }
    for run in store.runs_for_item(id)?.iter().rev() {
        assert_eq!(run.item, *id, "runs_for_item reads the item's runs");
        if let Some(recorded) = store.run_pull_request(&run.id)? {
            return Ok(recorded.pr);
        }
    }
    Err(ArchonError::Invalid(format!(
        "item {id} has delivered no pull request yet; queue onto it once it has one"
    )))
}

/// The one open pull request whose head is `branch`.
async fn branch_pull_request(
    binary: &Path,
    repo: &Path,
    branch: &str,
) -> Result<Headed, ArchonError> {
    if !is_plain_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is no plain branch name"
        )));
    }
    let open = gh::of_branch(binary, repo, branch, true).await?;
    let [pr] = open.as_slice() else {
        return Err(ArchonError::Invalid(format!(
            "`onto` needs one open pull request of `{branch}`, but GitHub lists {}",
            open.len()
        )));
    };
    let headed = gh::view_headed(binary, repo, &pr.url).await?;
    assert_eq!(
        headed.pr.identity(),
        pr.identity(),
        "checked by view_headed"
    );
    Ok(headed)
}

/// `headed` as `target`'s frozen record, refused unless Archon could push to it.
fn frozen(target: &OntoTarget, headed: Headed) -> Result<OntoPullRequest, ArchonError> {
    let url = &headed.pr.url;
    match headed.pr.state.as_str() {
        "OPEN" => {}
        "MERGED" => return Err(refused(url, "it is merged")),
        _ => return Err(refused(url, "it is closed")),
    }
    let Some(head_repository) = headed.head_repository() else {
        return Err(refused(url, "its head repository is gone"));
    };
    if headed.is_cross_repository {
        let why = format!("its head `{head_repository}` is a fork, which Archon does not push to");
        return Err(refused(url, &why));
    }
    let Some(head_sha) = headed.pr.head_ref_oid.clone() else {
        return Err(ArchonError::Protocol(format!(
            "GitHub gave no head commit for {url}"
        )));
    };
    assert!(stack::is_commit_id(&head_sha), "checked by gh");
    let node_id = headed.pr.node_id().map(str::to_owned);
    Ok(OntoPullRequest {
        target: target.clone(),
        pr: headed.pr.identity(),
        node_id,
        head_repository,
        head_ref: headed.pr.head_ref_name,
        base_ref: headed.pr.base_ref_name,
        head_sha,
    })
}

fn refused(url: &str, why: &str) -> ArchonError {
    ArchonError::Invalid(format!(
        "cannot deliver into {url}: {why}; queue the work without `onto` or name an open pull \
         request"
    ))
}

/// Make the frozen head of `onto` readable in `repo`, fetching it from
/// `origin` when missing: an `onto` item's branch starts there.
pub(super) fn fetch_head(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
) -> Result<String, ArchonError> {
    assert!(stack::is_commit_id(&onto.head_sha), "frozen at enqueue");
    assert!(is_plain_branch(&onto.head_ref), "frozen at enqueue");
    let shadow = Shadow::of(data_dir, repo)?;
    stack::fetch(&shadow, repo, &onto.head_ref, &onto.head_sha).map_err(|error| {
        error.with_detail(&format!(
            "the frozen head of {} could not be read",
            onto.pr.url()
        ))
    })?;
    Ok(onto.head_sha.clone())
}

/// What the agents of an `onto` item are told: which pull request their
/// change goes into, and the commit their branch starts from.
pub(super) fn section(onto: Option<&OntoPullRequest>) -> String {
    let Some(onto) = onto else {
        return String::new();
    };
    assert!(stack::is_commit_id(&onto.head_sha), "frozen at enqueue");
    format!(
        "\n## Existing pull request\n\nThis change goes into the open pull request {} \
         (head `{}` into `{}`), not a new one. Archon started this branch from that pull \
         request's head `{}`, frozen when the item was queued: diff and review the change \
         against that commit, and pass it to reviewers as the base.\n",
        onto.pr.url(),
        onto.head_ref,
        onto.base_ref,
        onto.head_sha
    )
}
