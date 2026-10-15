//! `refresh_item`: bring a finished item's branch up to its base branch again
//! (ARCHITECTURE §13). The base is fetched from `origin`, and an item on the
//! same branch merges it in, never rebasing: its agent resolves conflicts and
//! runs the checks, its review reads the change from the new merge base, and it
//! ships into the pull request the branch already has.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::store::stacks::Refresh;

use super::dependencies::is_plain_branch;
use super::{gh, recover};
use crate::context::DaemonContext;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// Queue a refresh of the finished item `id` onto its base branch, or return the
/// one still to finish for the same base commit.
pub(crate) async fn refresh_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<QueueItem, ArchonError> {
    let (owner, run, repo) = target(ctx, id).await?;
    let base = base_of(ctx, &run).await?;
    if repo.ship == ShipMode::Pr && run.shipped {
        check_pr_open(ctx, &repo, &run.branch).await?;
    }
    let sha = base_head(ctx, &repo, &base, &run.branch).await?;
    let refresh = Refresh { base, sha };
    let queued = {
        let _table = ctx.runs.lock().await;
        let mut store = ctx.store.lock().await;
        let now = store.item(id)?;
        let last = store.runs_for_item(id)?.pop();
        if now.state != owner.state || last.as_ref().map(|r| &r.id) != Some(&run.id) {
            return Err(ArchonError::InvalidTransition(format!(
                "{id} changed while its refresh was prepared; ask again"
            )));
        }
        store.enqueue_refresh(&owner, &run, &refresh)?
    };
    assert!(!queued.state.is_terminal(), "a refresh still to run");
    tracing::info!(item = %id, refresh = %queued.id, base = %refresh.base, sha = %refresh.sha, "refresh queued");
    ctx.dispatch.notify_one();
    Ok(queued)
}

/// The item, its last run and its repository, when that run left a branch of
/// a finished item that is no fix itself and is stacked on nothing unmerged.
async fn target(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<(QueueItem, PipelineRun, Repo), ArchonError> {
    let store = ctx.store.lock().await;
    let item = store.item(id)?;
    if let Some(fix) = store.stack_fix(id)? {
        return Err(ArchonError::Invalid(format!(
            "{id} works on the branch of {}; refresh that item instead",
            fix.owner
        )));
    }
    if !matches!(
        item.state,
        QueueState::Succeeded { .. } | QueueState::Failed { .. } | QueueState::Cancelled { .. }
    ) {
        return Err(ArchonError::InvalidTransition(format!(
            "only a finished item is refreshed; {id} is {}",
            item.state.kind()
        )));
    }
    let Some(repo) = &item.repo else {
        return Err(ArchonError::Invalid(format!("{id} has no repository")));
    };
    let repo = store.repo(&crate::resolve::repo_ref(repo))?;
    let run = store.runs_for_item(id)?.pop();
    let Some(run) = run.filter(|r| is_plain_branch(&r.branch)) else {
        return Err(ArchonError::Invalid(format!(
            "{id} has no run branch to refresh"
        )));
    };
    if let Some(parent) = store.stack_parents(id)?.iter().find(|p| !p.merged) {
        return Err(ArchonError::Invalid(format!(
            "{id} is stacked on `{}`, which restacking merges in as it moves",
            parent.parent_branch
        )));
    }
    assert_eq!(run.item, item.id, "the item's own run");
    Ok((item, run, repo))
}

/// The base branch `run` forked from, by name.
async fn base_of(ctx: &DaemonContext, run: &PipelineRun) -> Result<String, ArchonError> {
    let vars = recover::load_run_vars(&*ctx.store.lock().await, &run.id)?;
    let base = vars
        .get("base_branch")
        .map(|b| b.trim())
        .unwrap_or_default();
    if stack::is_commit_id(base) || !is_plain_branch(base) {
        return Err(ArchonError::Invalid(format!(
            "the item's run forked from {base:?}, not a base branch; set the repository's base \
             branch with `archon repo set`"
        )));
    }
    Ok(base.to_owned())
}

/// Refuse a refresh of a delivered branch whose pull request merged or closed:
/// it would only open a new one.
async fn check_pr_open(ctx: &DaemonContext, repo: &Repo, branch: &str) -> Result<(), ArchonError> {
    let settled = gh::settled(&*ctx.store.lock().await, &repo.id, branch)?;
    let open = match settled {
        Some(_) => Vec::new(),
        None => gh::of_branch(&ctx.config.github.binary, &repo.path, branch, true).await?,
    };
    if open.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "`{branch}` has no open pull request: it merged or closed, so there is nothing to \
             refresh; enqueue new work instead"
        )));
    }
    Ok(())
}

/// The commit of `base` a refresh merges: `origin`'s head, fetched now, in
/// `pr` mode, else the local branch's. Refused when `branch` holds it already.
async fn base_head(
    ctx: &DaemonContext,
    repo: &Repo,
    base: &str,
    branch: &str,
) -> Result<String, ArchonError> {
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let (base, branch) = (base.to_owned(), branch.to_owned());
    let remote = repo.ship == ShipMode::Pr;
    tokio::task::spawn_blocking(move || head_now(&data_dir, &path, remote, &base, &branch))
        .await
        .map_err(|e| ArchonError::io("refresh base task", std::io::Error::other(e)))?
}

fn head_now(
    data_dir: &Path,
    repo: &Path,
    remote: bool,
    base: &str,
    branch: &str,
) -> Result<String, ArchonError> {
    if !crate::worktree::branch_exists(repo, branch)? {
        return Err(ArchonError::Invalid(format!(
            "the item's branch `{branch}` is gone from the repository"
        )));
    }
    let sha = match remote {
        true => stack::fetch_head(&Shadow::of(data_dir, repo)?, repo, base)?.ok_or_else(|| {
            ArchonError::Invalid("the repository has no `origin` to fetch the base from".into())
        })?,
        false => stack::sha(repo, base)?,
    };
    if stack::contains(repo, &sha, &format!("refs/heads/{branch}"))? {
        return Err(ArchonError::Invalid(format!(
            "`{branch}` already holds `{base}` at {}; nothing to refresh",
            &sha[..12]
        )));
    }
    Ok(sha)
}
