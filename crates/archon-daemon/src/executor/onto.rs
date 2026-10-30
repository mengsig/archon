//! An item's `onto` target: the existing pull request it delivers into,
//! resolved and frozen at enqueue, and where its worktree starts. A `ship =
//! local` repository has none: [`local`] works on the target item's branch.

pub(crate) mod local;

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;

use super::dependencies::is_plain_branch;
use super::gh::{self, Headed};
use crate::context::DaemonContext;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// The open pull request `target` names in `repo`, frozen as GitHub shows it
/// now. Refused when it is closed or merged; for an item that delivers into
/// it, also when its head is in a fork or a repository the operator cannot
/// push to. A `review_only` item pushes nothing, so any head will do.
pub(crate) async fn freeze(
    ctx: &DaemonContext,
    repo: &Repo,
    target: &OntoTarget,
    review_only: bool,
) -> Result<OntoPullRequest, ArchonError> {
    if repo.ship != ShipMode::Pr {
        return Err(ArchonError::Invalid(format!(
            "`{}` ships locally, so there is no pull request to deliver into; queue the work \
             on its own `onto` an item or branch, and it works on that item's branch",
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
        OntoTarget::Pr(number) => gh::view_headed(binary, repo, &number.to_string()).await?,
        OntoTarget::Item(id) => {
            let pr = item_pull_request(&*ctx.store.lock().await, repo, id)?;
            gh::view_headed(binary, repo, &pr.url()).await?
        }
        OntoTarget::Branch(branch) => branch_pull_request(binary, repo, branch).await?,
    };
    let frozen = frozen(target, headed, review_only)?;
    if !review_only && !gh::can_push(binary, repo, &frozen.head_repository).await? {
        let why = format!("you have no write access to `{}`", frozen.head_repository);
        return Err(refused(&frozen.pr.url(), &why));
    }
    assert!(stack::is_commit_id(&frozen.head_sha), "checked by frozen");
    assert!(is_plain_branch(&frozen.head_ref), "checked by gh");
    Ok(frozen)
}

/// The commit the newest passed review on `stages` was held to, as Archon
/// recorded it; `None` when no stage holds a pass that counts.
pub(super) fn reviewed_head(stages: &[StageRun]) -> Option<String> {
    let head = stages
        .iter()
        .rev()
        .filter(|s| s.state == StageState::Completed)
        .find_map(|s| archon_pipeline::review::reviewed_head(s.structured.as_ref()))?;
    assert!(stack::is_commit_id(&head), "recorded as a full commit id");
    Some(head)
}

/// The pull request a succeeded review item among `item`'s dependencies
/// passed at its frozen head, the first in order: an item queued on it with
/// no `onto` starts from that head, and those commits ship beneath its own,
/// unchanged. A refresh or restack fix answers for its owner, whose branch
/// it works on. `None` for an `onto` item or with no such dependency.
pub(crate) fn reviewed_dependency(
    store: &Store,
    item: &QueueItem,
) -> Result<Option<OntoPullRequest>, ArchonError> {
    if item.onto.is_some() {
        return Ok(None);
    }
    let owner = match store.stack_fix(&item.id)? {
        Some(fix) => Some(store.item(&fix.owner)?),
        None => None,
    };
    let item = owner.as_ref().unwrap_or(item);
    assert!(
        store.stack_fix(&item.id)?.is_none(),
        "a stack fix's owner is no fix itself"
    );
    for id in &item.dependencies {
        assert_ne!(id, &item.id, "no item depends on itself");
        let dependency = store.item(id)?;
        let QueueState::Succeeded { run, .. } = &dependency.state else {
            continue;
        };
        let Some(onto) = dependency.onto.filter(|o| o.review_only) else {
            continue;
        };
        if dependency.repo != item.repo {
            continue;
        }
        let passed = reviewed_head(&store.stage_runs(run)?);
        if passed.as_deref() == Some(onto.head_sha.as_str()) {
            return Ok(Some(*onto));
        }
    }
    Ok(None)
}

/// What the agents of an item stacked on a reviewed pull request are told:
/// their change starts at its reviewed head, whose commits stay as they are.
pub(super) fn stacked_section(reviewed: Option<&OntoPullRequest>) -> String {
    let Some(reviewed) = reviewed else {
        return String::new();
    };
    assert!(reviewed.review_only, "stacked on a reviewed pull request");
    format!(
        "\n## Stacked on a reviewed pull request\n\nThis branch starts at `{}`, the head of \
         the open pull request {} that a review item passed. Its commits are another \
         author's and ship as they are beneath yours: never amend, rebase, squash or drop \
         them, or Archon refuses the delivery. Your change starts at that commit: diff and \
         review it from there, and pass it to reviewers as the base. Archon opens a pull \
         request of its own into `{}` and never pushes to the reviewed one.\n",
        reviewed.head_sha,
        reviewed.pr.url(),
        reviewed.base_ref
    )
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
    repo: &Repo,
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

/// `headed` as `target`'s frozen record, refused unless it is open and, for an
/// item that delivers into it, Archon could push to it.
fn frozen(
    target: &OntoTarget,
    headed: Headed,
    review_only: bool,
) -> Result<OntoPullRequest, ArchonError> {
    let url = &headed.pr.url;
    match headed.pr.state.as_str() {
        "OPEN" => {}
        "MERGED" => return Err(refused(url, "it is merged")),
        _ => return Err(refused(url, "it is closed")),
    }
    let Some(head_repository) = headed.head_repository() else {
        return Err(refused(url, "its head repository is gone"));
    };
    if headed.is_cross_repository && !review_only {
        let why = format!(
            "its head `{head_repository}` is a fork, which Archon does not push to; queue it \
             with the `review` pipeline to review it without pushing"
        );
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
        review_only,
    })
}

fn refused(url: &str, why: &str) -> ArchonError {
    ArchonError::Invalid(format!(
        "cannot deliver into {url}: {why}; queue the work without `onto` or name an open pull \
         request"
    ))
}

/// Make the frozen head of `onto` readable in `repo`, fetching it from
/// `origin` when missing: an `onto` item's branch starts there. A fork's
/// head is fetched from `origin`'s `refs/pull/<n>/head`, never from the fork.
pub(super) fn fetch_head(
    data_dir: &Path,
    repo: &Path,
    onto: &OntoPullRequest,
) -> Result<String, ArchonError> {
    assert!(stack::is_commit_id(&onto.head_sha), "frozen at enqueue");
    assert!(is_plain_branch(&onto.head_ref), "frozen at enqueue");
    let shadow = Shadow::of(data_dir, repo)?;
    let remote_ref = match onto.is_fork() {
        true => format!("refs/pull/{}/head", onto.pr.number),
        false => format!("refs/heads/{}", onto.head_ref),
    };
    stack::fetch_ref(&shadow, repo, &remote_ref, &onto.head_sha).map_err(|error| {
        error.with_detail(&format!(
            "the frozen head of {} could not be read",
            onto.pr.url()
        ))
    })?;
    Ok(onto.head_sha.clone())
}

/// What the agents of an `onto` item are told: which pull request their
/// change goes into, and the commit their branch starts from; for a review
/// of the pull request, that it is the change and nothing is committed.
pub(super) fn section(onto: Option<&OntoPullRequest>) -> String {
    let Some(onto) = onto else {
        return String::new();
    };
    assert!(stack::is_commit_id(&onto.head_sha), "frozen at enqueue");
    if onto.review_only {
        return review_section(onto);
    }
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

fn review_section(onto: &OntoPullRequest) -> String {
    assert!(
        onto.review_only,
        "only a review item reviews the pull request"
    );
    let fork = match onto.is_fork() {
        true => format!(" Its head is in the fork `{}`.", onto.head_repository),
        false => String::new(),
    };
    format!(
        "\n## Pull request under review\n\nThis item reads the open pull request {} (head \
         `{}` into `{}`) as it stands at `{}`, frozen when the item was queued.{fork} Archon \
         started this branch at that commit. The change under review is the whole pull \
         request: from where it forks from `{}` (`git merge-base HEAD origin/{}`) to that \
         head; pass that fork point to reviewers as the base. Commit nothing and push \
         nothing: Archon refuses a review once the branch moves, and only a review that \
         passed that exact head lets the pull request be merged.\n",
        onto.pr.url(),
        onto.head_ref,
        onto.base_ref,
        onto.head_sha,
        onto.base_ref,
        onto.base_ref
    )
}
