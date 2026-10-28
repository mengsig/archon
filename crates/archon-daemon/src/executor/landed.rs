//! Restacking a child over parents whose pull requests merged as a squash or
//! a rebase (ARCHITECTURE §8.8): the base then holds a parent's content under
//! commits of its own, which conflict with the child's copy of the parent.
//! The child's branch is merged over that landing, never rebased or
//! force-pushed.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::queue::QueueItem;
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::store::stacks::StackParent;

use super::restack::{self, Incoming, Restacked};
use crate::context::DaemonContext;
use crate::worktree::landed::{self, Landing};
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// What a merged parent asks of the child's branch, against the base's head.
#[derive(Debug)]
enum Plan {
    /// The branch needs nothing for it: the base holds the parent's head
    /// itself, or the branch holds the parent's landing already, as pushed.
    Held,
    /// The base holds the parent's content under other commits.
    Landed(Landing),
    /// The base holds the parent's content in no way a tree or patch id proves.
    Unproven,
}

/// Restack `item`'s branch over each parent that merged since, once none is
/// open, and record it: one landed as a squash or rebase is merged over its
/// landing, one the base holds otherwise is a plain merge of the base. A
/// conflict or refused push is told to the operator, once.
pub(super) async fn reconcile(
    ctx: &DaemonContext,
    (repo, item, run): (&Repo, &QueueItem, &PipelineRun),
    child_head: Option<&str>,
) -> Result<(), ArchonError> {
    assert_eq!(run.item, item.id, "the item's own run");
    let parents = ctx.store.lock().await.stack_parents(&item.id)?;
    if parents.iter().any(StackParent::is_open) {
        return Ok(());
    }
    let pending = parents
        .into_iter()
        .filter(|p| p.merged && p.landed.is_none());
    for mut parent in pending {
        let Some(base) = due_base(ctx, item).await? else {
            return Ok(());
        };
        let Some(mark) = restack::mark_current(ctx, item, run).await? else {
            return Ok(());
        };
        let at = (base.as_str(), run.branch.as_str(), child_head);
        let (head, plan) = plan(ctx, repo, at, &parent.parent_sha).await?;
        let incoming = match plan {
            Plan::Held => None,
            Plan::Landed(landing) => Some(Incoming::Landed {
                dep: parent.parent_sha.clone(),
                landing,
            }),
            Plan::Unproven => Some(Incoming::Commit(head.clone())),
        };
        if let Some(incoming) = incoming {
            let restacked =
                restack::merge_into_branch(ctx, repo, run, (&incoming, child_head), mark).await?;
            tell(ctx, (item, run), (&parent, &base), restacked).await?;
        }
        assert!(
            parent.merged && !parent.closed,
            "only a merged parent lands"
        );
        parent.landed = Some(head);
        ctx.store.lock().await.save_stack_parent(&parent)?;
    }
    Ok(())
}

/// For `refresh_item`: merge `item`'s branch over each merged parent the base
/// `base` holds as a squash or rebase, and push it where it was delivered.
/// Whether anything was merged; any other parent is left to the refresh's
/// own merge of the base.
pub(super) async fn for_refresh(
    ctx: &DaemonContext,
    (repo, item, run): (&Repo, &QueueItem, &PipelineRun),
    base: &str,
    child_head: Option<&str>,
) -> Result<bool, ArchonError> {
    assert_eq!(run.item, item.id, "the item's own run");
    assert!(item.onto.is_none(), "an onto item has no stack parents");
    let parents = ctx.store.lock().await.stack_parents(&item.id)?;
    let mut merged = false;
    for mut parent in parents.into_iter().filter(|p| p.merged) {
        let at = (base, run.branch.as_str(), child_head);
        let (head, plan) = plan(ctx, repo, at, &parent.parent_sha).await?;
        let Plan::Landed(landing) = plan else {
            continue;
        };
        if ctx.store.lock().await.has_pending_stack_fix(&item.id)? {
            return Err(ArchonError::InvalidTransition(format!(
                "{} has a restack fix or refresh still to finish, which works on its branch",
                item.id
            )));
        }
        let Some(mark) = restack::mark_current(ctx, item, run).await? else {
            return Err(ArchonError::InvalidTransition(format!(
                "{} changed or is being collected while its refresh was prepared; ask again",
                item.id
            )));
        };
        let dep = parent.parent_sha.clone();
        let incoming = Incoming::Landed { dep, landing };
        match restack::merge_into_branch(ctx, repo, run, (&incoming, child_head), mark).await? {
            Restacked::Done => merged = true,
            // The refresh's agent merges the base and resolves it.
            Restacked::Conflict(_) => continue,
            Restacked::Rejected(said) => {
                return Err(ArchonError::Invalid(rejected(item, run, base, &said)));
            }
        }
        parent.landed = Some(head);
        ctx.store.lock().await.save_stack_parent(&parent)?;
    }
    Ok(merged)
}

/// The repository base `item` is for now, unless a restack fix or refresh
/// still works on its branch.
async fn due_base(ctx: &DaemonContext, item: &QueueItem) -> Result<Option<String>, ArchonError> {
    let store = ctx.store.lock().await;
    if store.has_pending_stack_fix(&item.id)? {
        return Ok(None);
    }
    restack::target_base(&store, &item.id)
}

/// The head of `base` now, and what the merged parent head `dep` asks of
/// `branch`, whose pull request's head is `child_head`, against it.
async fn plan(
    ctx: &DaemonContext,
    repo: &Repo,
    (base, branch, child_head): (&str, &str, Option<&str>),
    dep: &str,
) -> Result<(String, Plan), ArchonError> {
    assert!(stack::is_commit_id(dep), "a recorded parent head");
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let (base, branch, dep) = (base.to_owned(), branch.to_owned(), dep.to_owned());
    let child_head = child_head.map(str::to_owned);
    let remote = repo.ship == ShipMode::Pr;
    tokio::task::spawn_blocking(move || {
        let shadow = Shadow::of(&data_dir, &path)?;
        let head = base_head(&shadow, &path, remote, &base)?;
        if stack::contains(&path, &dep, &head)? {
            return Ok((head, Plan::Held));
        }
        let plan = match landed::find(&shadow, &path, &dep, &head)? {
            None => Plan::Unproven,
            Some(landing) => {
                // A merge the pull request lacks, its push failed, is pushed again.
                let local = stack::sha(&path, &branch)?;
                let pushed = child_head.is_none_or(|head| head == local);
                match pushed && stack::contains(&path, landing.commit(), &local)? {
                    true => Plan::Held,
                    false => Plan::Landed(landing),
                }
            }
        };
        Ok((head, plan))
    })
    .await
    .map_err(|e| ArchonError::io("landed parent task", std::io::Error::other(e)))?
}

/// `origin`'s head of `base`, fetched, when `remote`, else the local branch's.
pub(super) fn base_head(
    shadow: &Shadow,
    repo: &Path,
    remote: bool,
    base: &str,
) -> Result<String, ArchonError> {
    let head = match remote {
        true => stack::fetch_head(shadow, repo, base)?.ok_or_else(|| {
            ArchonError::Invalid("the repository has no `origin` to fetch the base from".into())
        })?,
        false => stack::sha(repo, base)?,
    };
    assert!(stack::is_commit_id(&head), "a base head is a commit id");
    Ok(head)
}

/// Tell the operator a restack over a merged parent was not delivered.
async fn tell(
    ctx: &DaemonContext,
    (item, run): (&QueueItem, &PipelineRun),
    (parent, base): (&StackParent, &str),
    restacked: Restacked,
) -> Result<(), ArchonError> {
    let (id, branch) = (&item.id, &run.branch);
    let text = match restacked {
        Restacked::Done => return Ok(()),
        Restacked::Conflict(files) => format!(
            "Item {id}'s parent {} merged into `{base}`, and merging `{base}` into `{branch}` \
             conflicts in:\n{files}\nNothing was written, and Archon never rebases or \
             force-pushes: run `refresh_item` on {id} to merge `{base}` and resolve it",
            parent.parent_item
        ),
        Restacked::Rejected(said) => rejected(item, run, base, &said),
    };
    super::finish::notify(ctx, NotificationKind::Blocker, &text, Some(id), None).await
}

/// Why a restacked branch is not on `origin`: `said` is the refused push.
fn rejected(item: &QueueItem, run: &PipelineRun, base: &str, said: &str) -> String {
    format!(
        "Restacking item {} over its merged parent merged `{base}` into `{b}`, but `origin` \
         rejected the push as not a fast-forward ({said}): the pull request has commits Archon \
         does not have, and Archon never force-pushes. Merge `origin/{b}` into the branch by \
         hand, then refresh it again",
        item.id,
        b = run.branch
    )
}
