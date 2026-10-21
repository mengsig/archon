//! Fork from shipped dependencies and merge moved parents without rewriting
//! history. Every merge is made through the shadow (ARCHITECTURE §13.4): no
//! program a worktree or repository config names runs, and a merge that
//! conflicts writes nothing; a fix item makes it instead.

use std::path::Path;

use super::dependencies::is_plain_branch;
use super::gh::{self, Pass, PullRequest};
use super::repo_identity::{self, RepoIdentity};
use crate::context::DaemonContext;
use crate::gh_account::{self, Token};
use crate::worktree::shadow::Shadow;
use crate::worktree::stack::{self, Identity, Merge};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::store::stacks::StackParent;

/// A branch's pull request as one pass sees it.
enum Pr {
    Open(Box<PullRequest>),
    /// Merged or closed: recorded, and never asked about again.
    Settled {
        merged: bool,
    },
}

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
        return Err(ArchonError::Corrupt(format!(
            "stacked item {owner} has no repository"
        )));
    };
    let configured = store.repo(&crate::resolve::repo_ref(&repo))?.base_branch;
    let base = configured.or_else(|| first.repository_base.clone());
    base.map(Some).ok_or_else(|| {
        ArchonError::Invalid(format!("stacked item {owner} records no repository base"))
    })
}

/// The parents `item` forks from: the shipped runs of its same-repository
/// dependencies whose pull request has not merged.
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
    let mut pass = Pass::default();
    let mut parents = Vec::new();
    for id in &item.dependencies {
        let (parent, run) = {
            let store = ctx.store.lock().await;
            let parent = store.item(id)?;
            if parent.repo != item.repo {
                continue;
            }
            let run = store.runs_for_item(id)?.last().cloned();
            (parent, run)
        };
        let Some(run) = run.filter(|r| r.shipped && is_plain_branch(&r.branch)) else {
            continue;
        };
        if let (Some(onto), ShipMode::Pr) = (&parent.onto, repo.ship) {
            return Err(ArchonError::Invalid(format!(
                "{id} went into the pull request {}, not a branch of its own, so {} cannot \
                 stack on it: queue the work `onto` {id} instead",
                onto.pr.url(),
                item.id
            )));
        }
        let head = match repo.ship {
            ShipMode::Local => None,
            ShipMode::Pr => {
                match pull_request(ctx, &mut pass, repo, &run.branch, Some(&run.id)).await? {
                    Pr::Settled { merged: true } => continue,
                    Pr::Settled { merged: false } => None,
                    Pr::Open(pr) => pr.head_ref_oid,
                }
            }
        };
        let sha = parent_sha(ctx, repo, &run.branch, head).await?;
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
    assert!(parents.len() <= item.dependencies.len());
    assert!(
        parents.iter().all(|p| p.parent_item != item.id),
        "no item stacks on itself"
    );
    save_parents(ctx, repo, item, base, &mut parents).await?;
    Ok(parents)
}

/// The pull request of `parent`'s branch, by the one its newest run on that
/// branch delivered to.
async fn parent_pr(
    ctx: &DaemonContext,
    pass: &mut Pass,
    repo: &Repo,
    parent: &StackParent,
) -> Result<Pr, ArchonError> {
    let runs = ctx.store.lock().await.runs_for_item(&parent.parent_item)?;
    let delivered = runs.iter().rev().find(|r| r.branch == parent.parent_branch);
    let branch = &parent.parent_branch;
    pull_request(ctx, pass, repo, branch, delivered.map(|r| &r.id)).await
}

/// The pull request of `branch` in `repo`, the one `run` delivered to when it
/// recorded one: settled when known to be, else as GitHub shows it now.
async fn pull_request(
    ctx: &DaemonContext,
    pass: &mut Pass,
    repo: &Repo,
    branch: &str,
    run: Option<&PipelineRunId>,
) -> Result<Pr, ArchonError> {
    assert_eq!(
        repo.ship,
        ShipMode::Pr,
        "only a pr repository has pull requests"
    );
    assert!(!branch.is_empty(), "a delivered branch");
    let (own, settled) = {
        let store = ctx.store.lock().await;
        let recorded = run.map(|run| store.run_pull_request(run)).transpose()?;
        let own = recorded.flatten().map(|recorded| recorded.pr);
        let settled = own.as_ref().map(|pr| gh::settled(&store, pr)).transpose()?;
        (own, settled.flatten())
    };
    if let Some(state) = settled {
        return Ok(Pr::Settled {
            merged: state == "MERGED",
        });
    }
    let binary = &ctx.config.github.binary;
    let mut pr = pass.pull_request(binary, repo, branch).await?;
    assert_eq!(pr.head_ref_name, branch, "checked by the gh module");
    if let Some(own) = own.filter(|own| *own != pr.identity()) {
        // The branch carries a newer pull request, so the run's own ended.
        pr = pass.by_number(binary, repo, &own).await?;
        if pr.is_open() {
            return Err(ArchonError::Protocol(format!(
                "{own} is open, but GitHub lists {} for `{branch}`",
                pr.url
            )));
        }
    }
    if pr.is_open() {
        return Ok(Pr::Open(Box::new(pr)));
    }
    gh::settle(&mut *ctx.store.lock().await, &pr)?;
    Ok(Pr::Settled {
        merged: pr.state == "MERGED",
    })
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

/// The commit a parent is merged at: `origin`'s head of its pull request,
/// fetched as objects only, else the local branch's.
async fn parent_sha(
    ctx: &DaemonContext,
    repo: &Repo,
    branch: &str,
    remote: Option<String>,
) -> Result<String, ArchonError> {
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let branch = branch.to_owned();
    tokio::task::spawn_blocking(move || match remote {
        Some(sha) => {
            let shadow = Shadow::of(&data_dir, &path)?;
            stack::fetch(&shadow, &path, &branch, &sha)?;
            Ok(sha)
        }
        None => stack::sha(&path, &branch),
    })
    .await
    .map_err(|e| ArchonError::io("read stack parent", std::io::Error::other(e)))?
}

/// Merge `sha` into `branch` and move the branch forward, with the worktree
/// under `worktrees` it is checked out in: the conflicting files, if any.
fn merge_branch(
    shadow: &Shadow,
    repo: &Path,
    worktrees: &Path,
    branch: &str,
    sha: &str,
    identity: &Identity,
) -> Result<Option<String>, ArchonError> {
    assert!(is_plain_branch(branch), "a run branch");
    assert!(stack::is_commit_id(sha), "a parent commit");
    match stack::merge(shadow, repo, branch, sha, identity)? {
        Merge::Contained => Ok(None),
        Merge::Merged { from, to } => {
            stack::advance(shadow, repo, worktrees, branch, &from, &to)?;
            Ok(None)
        }
        Merge::Conflict(files) => Ok(Some(files)),
    }
}

/// Merge the parents a fresh worktree did not fork from into its branch;
/// the first that conflicts is recorded, for the run's agent to merge.
pub(super) async fn merge_others(
    ctx: &DaemonContext,
    repo: &Repo,
    tree: &Path,
    branch: &str,
    parents: &[StackParent],
) -> Result<(), ArchonError> {
    assert!(
        tree.starts_with(&ctx.paths.worktrees),
        "a worktree of archond's"
    );
    assert!(is_plain_branch(branch));
    let identity = RepoIdentity::merger(repo_identity::of(ctx, repo).await?.as_ref());
    for parent in parents.iter().filter(|p| !p.merged) {
        let (data_dir, worktrees) = (ctx.paths.data_dir.clone(), ctx.paths.worktrees.clone());
        let (path, branch) = (repo.path.clone(), branch.to_owned());
        let (sha, identity) = (parent.parent_sha.clone(), identity.clone());
        let conflicts = tokio::task::spawn_blocking(move || {
            let shadow = Shadow::of(&data_dir, &path)?;
            merge_branch(&shadow, &path, &worktrees, &branch, &sha, &identity)
        })
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

/// Maintain every stacked item once; one that fails is its own blocker and
/// the others go on.
pub(crate) async fn reconcile(ctx: &DaemonContext, pass: &mut Pass) -> Result<(), ArchonError> {
    let items = ctx.store.lock().await.stacked_items()?;
    for id in items {
        let result = reconcile_item(ctx, &id, pass).await;
        let what = format!("Restacking item {id}");
        super::coordination::isolate(ctx, &format!("stack:{id}"), Some(&id), &what, result).await?;
    }
    Ok(())
}

async fn reconcile_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
    pass: &mut Pass,
) -> Result<(), ArchonError> {
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
        let Some(repo) = &item.repo else {
            return Err(ArchonError::Protocol(format!(
                "stacked item {id} has no repository"
            )));
        };
        let repo = store.repo(&crate::resolve::repo_ref(repo))?;
        (item, run, repo, store.stack_parents(id)?)
    };
    assert_eq!(item.id, run.item);
    assert!(!run.branch.is_empty());
    let pr = match run.shipped && repo.ship == ShipMode::Pr {
        true => match pull_request(ctx, pass, &repo, &run.branch, Some(&run.id)).await? {
            Pr::Open(pr) => Some(*pr),
            Pr::Settled { .. } => return Ok(()),
        },
        false => None,
    };
    update_parents(ctx, &repo, &item, &run, parents, pass).await?;
    if let Some(pr) = pr {
        retarget(ctx, &repo, &run, pr).await?;
    }
    Ok(())
}

async fn update_parents(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    run: &PipelineRun,
    parents: Vec<StackParent>,
    pass: &mut Pass,
) -> Result<(), ArchonError> {
    assert_eq!(run.item, item.id);
    assert!(parents.iter().all(|p| p.item == item.id));
    let can_merge = run.shipped && matches!(item.state, QueueState::Succeeded { .. });
    for mut parent in parents.into_iter().filter(|p| !p.merged) {
        let head = match repo.ship {
            ShipMode::Local => None,
            ShipMode::Pr => match parent_pr(ctx, pass, repo, &parent).await? {
                Pr::Settled { merged: true } => {
                    parent.merged = true;
                    ctx.store.lock().await.save_stack_parent(&parent)?;
                    continue;
                }
                Pr::Settled { merged: false } => None,
                Pr::Open(pr) => pr.head_ref_oid,
            },
        };
        if !can_merge {
            continue;
        }
        let sha = parent_sha(ctx, repo, &parent.parent_branch, head).await?;
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
    run: &PipelineRun,
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
    let conflict = merge_into_branch(ctx, repo, run, sha, mark).await?;
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

/// Merge `sha` into the run branch, and in `pr` mode push the branch once
/// it merged: the conflicting files when it does not.
async fn merge_into_branch(
    ctx: &DaemonContext,
    repo: &Repo,
    run: &PipelineRun,
    sha: &str,
    mark: crate::gc::Mark,
) -> Result<Option<String>, ArchonError> {
    assert!(run.workspace.starts_with(&ctx.paths.worktrees));
    assert!(is_plain_branch(&run.branch), "a run branch");
    let identity = RepoIdentity::merger(repo_identity::of(ctx, repo).await?.as_ref());
    let (data_dir, worktrees) = (ctx.paths.data_dir.clone(), ctx.paths.worktrees.clone());
    let (path, branch, sha) = (repo.path.clone(), run.branch.clone(), sha.to_owned());
    let push = repo.ship == ShipMode::Pr;
    let token = match push {
        true => gh::token(&ctx.config.github.binary, repo).await?,
        false => None,
    };
    tokio::task::spawn_blocking(move || {
        let _mark = mark;
        let shadow = Shadow::of(&data_dir, &path)?;
        let conflict = merge_branch(&shadow, &path, &worktrees, &branch, &sha, &identity)?;
        if conflict.is_none() && push {
            push_branch(&shadow, &path, &branch, token.as_ref())?;
        }
        Ok(conflict)
    })
    .await
    .map_err(|e| ArchonError::io("restack merge task", std::io::Error::other(e)))?
}

/// Push `branch` of `repo` to `origin` as it stands, never forced, as
/// `token`'s login when given.
fn push_branch(
    shadow: &Shadow,
    repo: &Path,
    branch: &str,
    token: Option<&Token>,
) -> Result<(), ArchonError> {
    let url = match token {
        Some(token) => gh_account::push_url(repo, token.user())?
            .map_err(|why| ArchonError::Invalid(format!("BLOCKED: {why}")))?,
        None => stack::origin_url(repo)?.ok_or_else(|| {
            ArchonError::Invalid(
                "the repository has no `origin` to push the restacked branch to".into(),
            )
        })?,
    };
    let commit = stack::sha(repo, branch)?;
    let pushed = stack::push(shadow, &url, branch, &commit, token)?;
    match (pushed.status.success(), token) {
        (true, _) => {}
        (false, None) => return Err(pushed.failure()),
        (false, Some(token)) => {
            let said = token.redact(&pushed.stderr_text());
            let what = format!("git push of the restacked `{branch}` as {}", token.user());
            return Err(ArchonError::io(what, std::io::Error::other(said)));
        }
    }
    assert!(stack::is_commit_id(&commit), "the pushed commit");
    Ok(())
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
    run: &PipelineRun,
    pr: PullRequest,
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
    if pr.is_open() && pr.base_ref_name != base {
        let args = ["pr", "edit", &pr.url, "--base", &base];
        gh::text(&ctx.config.github.binary, repo, &args).await?;
    }
    Ok(())
}
