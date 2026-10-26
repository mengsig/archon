//! `onto` in a `ship = local` repository, which has no pull request to deliver
//! into: the item works on the target item's branch in its worktree, as a fix
//! of that item (a [`StackFix`] without a base to merge), and ships locally there.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::pull_request::OntoTarget;
use archon_core::queue::QueueItem;
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::Store;
use archon_queue::store::Asked;
use archon_queue::store::stacks::StackFix;

use super::super::dependencies::is_plain_branch;
use crate::context::DaemonContext;

/// Store `item`, asked `onto` `target` in the local `repo`, as a fix of the
/// item `target` names; a replay of idempotency `key` returns its first item.
pub(crate) async fn enqueue(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    target: &OntoTarget,
    (key, asked): (Option<&str>, &Asked),
) -> Result<QueueItem, ArchonError> {
    assert_eq!(repo.ship, ShipMode::Local, "pr mode freezes a pull request");
    assert!(item.onto.is_none() && item.dependencies.is_empty());
    let replay = match key {
        Some(key) => ctx.store.lock().await.item_by_key(key)?.is_some(),
        None => false,
    };
    if replay {
        let mut store = ctx.store.lock().await;
        return store.enqueue_onto_branch(item, (key, asked), target, None);
    }
    let fix = resolve(&*ctx.store.lock().await, repo, target)?;
    check_branch(repo, &fix).await?;
    let _table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    let now = resolve(&store, repo, target)?;
    if (&now.owner, &now.branch) != (&fix.owner, &fix.branch) {
        return Err(ArchonError::InvalidTransition(format!(
            "{} changed while work was queued onto it; ask again",
            fix.owner
        )));
    }
    store.enqueue_onto_branch(item, (key, asked), target, Some(&fix))
}

/// The branch of the finished item `target` names in `repo`, which new work
/// may go onto: refused, with the reason, when it may not.
fn resolve(store: &Store, repo: &Repo, target: &OntoTarget) -> Result<StackFix, ArchonError> {
    let owner = match target {
        OntoTarget::Pr(number) => {
            return Err(ArchonError::Invalid(format!(
                "`{}` ships locally, so there is no pull request #{number} to queue onto; \
                 name the item (`qi_…`) or the branch the work goes onto",
                repo.name
            )));
        }
        OntoTarget::Item(id) => id.clone(),
        OntoTarget::Branch(branch) => branch_owner(store, repo, branch)?,
    };
    let (owner, run) = finished_run(store, repo, &owner)?;
    check_unshared(store, &owner, &run)?;
    assert_eq!(run.item, owner.id, "the owner's own run");
    assert!(is_plain_branch(&run.branch), "checked by finished_run");
    Ok(StackFix {
        owner: owner.id,
        branch: run.branch,
        team: false,
        refresh: None,
    })
}

/// The one item of `repo`, no fix itself, whose last run left `branch`.
fn branch_owner(store: &Store, repo: &Repo, branch: &str) -> Result<QueueItemId, ArchonError> {
    if !is_plain_branch(branch) {
        return Err(ArchonError::Invalid(format!(
            "{branch:?} is no plain branch name"
        )));
    }
    let ran = store.items_with_run_branch(&repo.id, branch)?;
    assert!(
        ran.windows(2).all(|pair| pair[0] < pair[1]),
        "each item once"
    );
    let mut owners = Vec::with_capacity(ran.len());
    for id in ran {
        let last = store.runs_for_item(&id)?.pop();
        let left = last.is_some_and(|run| run.branch == branch);
        if left && store.stack_fix(&id)?.is_none() {
            owners.push(id);
        }
    }
    match owners.as_slice() {
        [owner] => Ok(owner.clone()),
        [] => Err(ArchonError::Invalid(format!(
            "no item of `{}` ran last on `{branch}`; name the item (`qi_…`) the work goes onto",
            repo.name
        ))),
        many => Err(ArchonError::Invalid(format!(
            "items {} of `{}` all ran last on `{branch}`; name the one (`qi_…`) the work goes \
             onto",
            many.iter()
                .map(QueueItemId::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            repo.name
        ))),
    }
}

/// Item `id` of `repo`, no fix itself, and its last run, when the item has
/// finished and that run left a plain branch.
fn finished_run(
    store: &Store,
    repo: &Repo,
    id: &QueueItemId,
) -> Result<(QueueItem, PipelineRun), ArchonError> {
    let item = store.item(id)?;
    if item.repo.as_ref() != Some(&repo.id) {
        return Err(ArchonError::Invalid(format!(
            "item {id} is not in `{}`, so work queued there cannot go onto its branch",
            repo.name
        )));
    }
    if let Some(fix) = store.stack_fix(id)? {
        return Err(ArchonError::Invalid(format!(
            "{id} works on the branch of {}; queue onto {} instead",
            fix.owner, fix.owner
        )));
    }
    if !item.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "only a finished item is queued onto in a `ship = local` repository; {id} is {}",
            item.state.kind()
        )));
    }
    let run = store.runs_for_item(id)?.pop();
    let Some(run) = run.filter(|r| is_plain_branch(&r.branch)) else {
        return Err(ArchonError::Invalid(format!(
            "{id} has no run that left a branch to queue onto"
        )));
    };
    assert!(run.state.is_terminal(), "a finished item's runs ended");
    assert_eq!(&run.item, id, "runs_for_item reads the item's runs");
    Ok((item, run))
}

/// Refuse a branch another stack, a team or unfinished work also writes to.
fn check_unshared(store: &Store, owner: &QueueItem, run: &PipelineRun) -> Result<(), ArchonError> {
    let id = &owner.id;
    assert_eq!(&run.item, id, "the owner's own run");
    assert!(owner.state.is_terminal(), "checked by finished_run");
    if run.team_root.is_some() {
        return Err(ArchonError::Invalid(format!(
            "{id} ran as a team, whose roles merge their own branches into `{}`; queue the \
             work without `onto`",
            run.branch
        )));
    }
    if let Some(parent) = store.stack_parents(id)?.iter().find(|p| p.is_open()) {
        return Err(ArchonError::Invalid(format!(
            "{id} is stacked on `{}`, which has not merged; queue onto {id} once it has",
            parent.parent_branch
        )));
    }
    if store.has_pending_stack_fix(id)? {
        return Err(ArchonError::InvalidTransition(format!(
            "{id} has a restack fix, refresh or `onto` item still to finish on its branch; \
             queue onto it once that one has finished"
        )));
    }
    Ok(())
}

/// Refuse a branch that is gone from the repository: nothing would hold the work.
async fn check_branch(repo: &Repo, fix: &StackFix) -> Result<(), ArchonError> {
    assert!(is_plain_branch(&fix.branch), "checked by finished_run");
    let (path, branch) = (repo.path.clone(), fix.branch.clone());
    let exists =
        tokio::task::spawn_blocking(move || crate::worktree::branch_exists(&path, &branch))
            .await
            .map_err(|e| ArchonError::io("onto branch task", std::io::Error::other(e)))??;
    if !exists {
        return Err(ArchonError::Invalid(format!(
            "the branch `{}` of {} is gone from the repository",
            fix.branch, fix.owner
        )));
    }
    Ok(())
}

/// What the agents of an item queued `onto` another item's branch are told.
pub(crate) fn section(store: &Store, item: &QueueItemId) -> Result<String, ArchonError> {
    let Some(fix) = store.onto_branch(item)? else {
        return Ok(String::new());
    };
    assert_ne!(&fix.owner, item, "a fix works on another item's branch");
    assert!(fix.refresh.is_none() && !fix.team, "a solo run's branch");
    Ok(format!(
        "\n## Existing branch\n\nThis change goes onto `{}`, the branch of item {}, not a new \
         one: Archon runs it in that item's worktree, on top of its last commit. Commit on \
         that branch, and never rebase, reset or force it. The review reads the whole branch \
         from where it left its base, that item's change included.\n",
        fix.branch, fix.owner
    ))
}
