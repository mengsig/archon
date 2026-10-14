//! Fork from shipped dependencies and merge moved parents without rewriting history.

use super::{dependencies::is_plain_branch, initiative_ship};
use crate::context::DaemonContext;
use crate::worktree::{WorktreeManager, stack};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_queue::store::stacks::StackParent;

/// The base a stacked `item` (or the item a restack fix repairs) is for now:
/// its first unmerged parent's branch, else the repository's (§8.8).
pub(super) fn target_base(
    store: &archon_queue::Store,
    item: &QueueItemId,
) -> Result<Option<String>, ArchonError> {
    let owner = store
        .stack_fix(item)?
        .map_or_else(|| item.clone(), |fix| fix.owner);
    let parents = store.stack_parents(&owner)?;
    let Some(first) = parents.first() else {
        return Ok(None);
    };
    assert_eq!(first.item, owner, "the parents are the stacked item's");
    if let Some(open) = parents.iter().find(|p| !p.merged) {
        assert!(is_plain_branch(&open.parent_branch), "recorded at the fork");
        return Ok(Some(open.parent_branch.clone()));
    }
    let Some(repo) = store.item(&owner)?.repo else {
        return Err(ArchonError::Store(format!(
            "stacked item {owner} has no repository"
        )));
    };
    let configured = store.repo(&crate::resolve::repo_ref(&repo))?.base_branch;
    let base = configured.or_else(|| first.repository_base.clone());
    base.map(Some).ok_or_else(|| {
        ArchonError::Invalid(format!("stacked item {owner} records no repository base"))
    })
}

pub(super) async fn parents(
    ctx: &DaemonContext,
    item: &QueueItem,
    repo: &Repo,
    base: Option<&str>,
) -> Result<Vec<StackParent>, ArchonError> {
    let existing = ctx.store.lock().await.stack_parents(&item.id)?;
    if !existing.is_empty() && item.fresh_retries == 0 {
        return Ok(existing);
    }
    let mut parents = Vec::new();
    for id in &item.dependencies {
        let run = {
            let store = ctx.store.lock().await;
            if store.item(id)?.repo != item.repo {
                continue;
            }
            store.runs_for_item(id)?.last().cloned()
        };
        let Some(run) = run.filter(|r| is_plain_branch(&r.branch)) else {
            continue;
        };
        let pr = if repo.ship == ShipMode::Pr && run.shipped {
            Some(initiative_ship::view(ctx, &repo.path, &run.branch).await?)
        } else {
            None
        };
        if pr.as_ref().is_some_and(|p| p.state == "MERGED") {
            continue;
        }
        let sha = parent_sha(repo, &run.branch, pr.and_then(|p| p.head_ref_oid)).await?;
        parents.push(StackParent {
            item: item.id.clone(),
            parent_item: id.clone(),
            parent_branch: run.branch,
            parent_sha: sha,
            position: parents.len(),
            repository_base: None,
            merged: false,
            conflict: None,
        });
    }
    save_parents(ctx, repo, item, base, &mut parents).await?;
    Ok(parents)
}

async fn save_parents(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    base: Option<&str>,
    parents: &mut [StackParent],
) -> Result<(), ArchonError> {
    assert!(parents.iter().all(|p| p.item == item.id));
    assert!(parents.len() <= item.dependencies.len());
    if !parents.is_empty() {
        let path = repo.path.clone();
        let base = base.map(str::to_owned);
        let base = tokio::task::spawn_blocking(move || match base {
            Some(base) => Ok(base),
            None => match crate::worktree::head(&path)? {
                crate::worktree::Head::Branch(branch) => Ok(branch),
                _ => Err(ArchonError::Invalid(
                    "stacking from a detached repository requires a base branch".into(),
                )),
            },
        })
        .await
        .map_err(|e| {
            ArchonError::io("resolve stack repository base", std::io::Error::other(e))
        })??;
        if !is_plain_branch(&base) {
            return Err(ArchonError::Invalid(
                "stack repository base must be a branch name".into(),
            ));
        }
        for parent in parents.iter_mut() {
            parent.repository_base = Some(base.clone());
        }
    }
    ctx.store
        .lock()
        .await
        .replace_stack_parents(&item.id, parents)
}

async fn parent_sha(
    repo: &Repo,
    branch: &str,
    remote: Option<String>,
) -> Result<String, ArchonError> {
    let (path, branch) = (repo.path.clone(), branch.to_owned());
    tokio::task::spawn_blocking(move || match remote {
        Some(sha) => {
            stack::fetch_head(&path, &branch, &sha)?;
            Ok(sha)
        }
        None => stack::sha(&path, &branch),
    })
    .await
    .map_err(|e| ArchonError::io("read stack parent", std::io::Error::other(e)))?
}

pub(super) async fn merge_others(
    ctx: &DaemonContext,
    repo: &Repo,
    tree: &std::path::Path,
    branch: &str,
    parents: &[StackParent],
) -> Result<(), ArchonError> {
    assert_ne!(repo.path, tree);
    assert!(is_plain_branch(branch));
    for parent in parents.iter().filter(|p| !p.merged) {
        let (repo, tree, branch, sha) = (
            repo.path.clone(),
            tree.to_owned(),
            branch.to_owned(),
            parent.parent_sha.clone(),
        );
        let conflicts =
            tokio::task::spawn_blocking(move || stack::merge(&repo, &tree, &branch, &sha))
                .await
                .map_err(|e| {
                    ArchonError::io("merge additional stack parent", std::io::Error::other(e))
                })??;
        if let Some(conflicts) = conflicts {
            let mut parent = parent.clone();
            parent.conflict = Some(conflicts);
            ctx.store.lock().await.save_stack_parent(&parent)?;
            break;
        }
    }
    Ok(())
}

pub(crate) async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let items = ctx.store.lock().await.stacked_items()?;
    for id in items {
        reconcile_item(ctx, &id).await?;
    }
    Ok(())
}

async fn reconcile_item(ctx: &DaemonContext, id: &QueueItemId) -> Result<(), ArchonError> {
    let (item, run, repo, parents) = {
        let _table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        let item = store.item(id)?;
        if ctx.collecting.contains(id) {
            return Ok(());
        }
        let Some(run) = store.runs_for_item(id)?.last().cloned() else {
            return Ok(());
        };
        if run.branch.is_empty() {
            return Ok(());
        }
        let repo = store.repo(&crate::resolve::repo_ref(
            item.repo.as_ref().expect("stack repo"),
        ))?;
        (item, run, repo, store.stack_parents(id)?)
    };
    assert_eq!(item.id, run.item);
    assert!(!run.branch.is_empty());
    let pr = if run.shipped && repo.ship == ShipMode::Pr {
        Some(initiative_ship::view(ctx, &repo.path, &run.branch).await?)
    } else {
        None
    };
    if pr.as_ref().is_some_and(|pr| pr.state != "OPEN") {
        return Ok(());
    }
    update_parents(ctx, &repo, &item, &run, parents).await?;
    if let Some(pr) = pr {
        retarget(ctx, &repo, &run, pr).await?;
    }
    Ok(())
}

async fn update_parents(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    run: &archon_core::run::PipelineRun,
    parents: Vec<StackParent>,
) -> Result<(), ArchonError> {
    assert_eq!(run.item, item.id);
    assert!(parents.iter().all(|p| p.item == item.id));
    let can_merge = run.shipped && matches!(item.state, QueueState::Succeeded { .. });
    for mut parent in parents.into_iter().filter(|p| !p.merged) {
        let pr = if repo.ship == ShipMode::Pr {
            Some(initiative_ship::view(ctx, &repo.path, &parent.parent_branch).await?)
        } else {
            None
        };
        if pr.as_ref().is_some_and(|p| p.state == "MERGED") {
            parent.merged = true;
            ctx.store.lock().await.save_stack_parent(&parent)?;
            continue;
        }
        if !can_merge {
            continue;
        }
        let sha = parent_sha(repo, &parent.parent_branch, pr.and_then(|p| p.head_ref_oid)).await?;
        if sha != parent.parent_sha || parent.conflict.is_some() {
            merge_parent(ctx, repo, item, run, &mut parent, &sha).await?;
        }
    }
    Ok(())
}

async fn merge_parent(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    run: &archon_core::run::PipelineRun,
    parent: &mut StackParent,
    sha: &str,
) -> Result<(), ArchonError> {
    if fix_pending(ctx, item, parent, sha).await? {
        return Ok(());
    }
    let mark = {
        let _table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        if store.item(&item.id)?.state != item.state
            || store.runs_for_item(&item.id)?.last().map(|r| &r.id) != Some(&run.id)
        {
            return Ok(());
        }
        let Some(mark) = ctx.collecting.mark(item.id.clone()) else {
            return Ok(());
        };
        mark
    };
    let mark = std::sync::Arc::new(mark);
    let conflict = merge_in_worktree(ctx, repo, item, run, sha, mark.clone()).await?;
    assert_eq!(parent.item, item.id);
    assert_ne!(parent.parent_item, item.id);
    parent.conflict = conflict;
    if let Some(conflict) = &parent.conflict {
        ctx.store
            .lock()
            .await
            .enqueue_stack_fix(item, run, parent, sha, conflict)?;
    } else {
        parent.parent_sha = sha.to_owned();
    }
    ctx.store.lock().await.save_stack_parent(parent)?;
    Ok(())
}

async fn merge_in_worktree(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    run: &archon_core::run::PipelineRun,
    sha: &str,
    mark: std::sync::Arc<crate::gc::Mark>,
) -> Result<Option<String>, ArchonError> {
    assert_eq!(run.item, item.id);
    assert!(run.workspace.starts_with(&ctx.paths.worktrees));
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let (repo_path, name, id, branch, base, sha) = (
        repo.path.clone(),
        repo.name.clone(),
        item.id.clone(),
        run.branch.clone(),
        repo.base_branch.clone(),
        sha.to_owned(),
    );
    let team = run.team_root.is_some();
    let push = repo.ship == ShipMode::Pr;
    let merge_sha = sha.clone();
    let conflict = tokio::task::spawn_blocking(move || {
        let _mark = mark;
        let tree = if team {
            manager.create_team_run(&repo_path, &name, &id, &branch, base.as_deref())?
        } else {
            manager.create(&repo_path, &name, &id, &branch, base.as_deref())?
        };
        let conflict = stack::merge(&repo_path, &tree.path, &branch, &merge_sha)?;
        if conflict.is_none() && push {
            stack::push(&tree.path, &branch)?;
        }
        Ok::<_, ArchonError>(conflict)
    })
    .await
    .map_err(|e| ArchonError::io("restack merge task", std::io::Error::other(e)))??;
    Ok(conflict)
}

async fn fix_pending(
    ctx: &DaemonContext,
    item: &QueueItem,
    parent: &StackParent,
    sha: &str,
) -> Result<bool, ArchonError> {
    let store = ctx.store.lock().await;
    assert_eq!(parent.item, item.id);
    assert_ne!(parent.parent_item, item.id);
    if store.has_pending_stack_fix(&item.id)? {
        return Ok(true);
    }
    let key = format!("restack_fix:{}:{}:{sha}", item.id, parent.parent_item);
    let Some(fix) = store.kv_get(&key)? else {
        return Ok(false);
    };
    Ok(!matches!(
        store.item(&QueueItemId::parse(&fix)?)?.state,
        QueueState::Succeeded { .. }
    ))
}

async fn retarget(
    ctx: &DaemonContext,
    repo: &Repo,
    run: &archon_core::run::PipelineRun,
    pr: initiative_ship::PullRequest,
) -> Result<(), ArchonError> {
    if repo.ship != ShipMode::Pr {
        return Ok(());
    }
    let stacked = target_base(&*ctx.store.lock().await, &run.item)?;
    let Some(base) = stacked else {
        return Ok(());
    };
    assert!(is_plain_branch(&base));
    assert_eq!(pr.head_ref_name, run.branch);
    if pr.state == "OPEN" && pr.base_ref_name != base {
        initiative_ship::gh(
            ctx,
            &repo.path,
            &["pr", "edit", &run.branch, "--base", &base],
        )
        .await?;
    }
    Ok(())
}
