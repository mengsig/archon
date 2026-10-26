//! Starting an item: pipeline, workspace, run, driver, first spawns.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use archon_core::autonomy::MergeMode;
use archon_core::catalog::{StageKind, WorkspaceMode, is_snapshot_of};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::notify::NotificationKind;
use archon_core::pull_request::OntoPullRequest;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, RunState};
use archon_pipeline::autonomy::OperatorGates;
use archon_pipeline::{PipelineSpec, RunDriver, RunVars};
use archon_queue::store::stacks::{StackFix, StackParent};

use super::{Cleanup, FollowUp, RunTable, finish, frozen, recover, settle};
use crate::context::DaemonContext;
use crate::knobs::{self, ItemRoute};
use crate::resolve::RepoEnv;
use crate::worktree::WorktreeManager;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// How a failed removal names the worktree of an abandoned start.
const ABANDONED: &str = "of a start that was abandoned";

/// A run ready to be inserted, with its driver.
struct Prepared {
    run: PipelineRun,
    driver: RunDriver,
    base_vars: BTreeMap<String, String>,
    /// The worktree this start added; no run row names it yet.
    added: Option<Cleanup>,
}

/// Where a run works.
struct Workspace {
    path: PathBuf,
    /// The directory of a team run's worktrees; `path` is its `main`.
    team_root: Option<PathBuf>,
    branch: String,
    base: String,
    added: Option<Cleanup>,
}

pub(super) async fn start_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<FollowUp, ArchonError> {
    let Some(item) = claim_item(ctx, id).await? else {
        return Ok(FollowUp::default());
    };
    let run_id = PipelineRunId::new();
    let subject = format!("starting item {}", item.id);
    let preparing = super::coordination::guarded(&subject, prepare(ctx, &item, run_id.clone()));
    let prepared = match preparing.await {
        Ok(prepared) => prepared,
        Err(error) => {
            frozen::discard(&ctx.paths.data_dir, &run_id);
            return fail_unstarted(ctx, &item.id, &error.to_string()).await;
        }
    };
    let mut table = ctx.runs.lock().await;
    launch(ctx, &mut table, prepared).await
}

/// `queued → dispatching` (a direct start), or an admitted `dispatching`
/// item without a run. Anything else is not ours to start, nor a queued
/// item whose worktree gc is collecting: a later dispatch starts it.
async fn claim_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<Option<QueueItem>, ArchonError> {
    let _table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    let item = store.item(id)?;
    let owner = store.stack_fix(id)?.map(|t| t.owner);
    let collecting =
        ctx.collecting.contains(id) || owner.as_ref().is_some_and(|id| ctx.collecting.contains(id));
    match item.state {
        QueueState::Queued if collecting || held_by_stack(&store, id)?.is_some() => Ok(None),
        QueueState::Queued => {
            let item = store.transition_item(id, QueueState::Dispatching, archon_core::now())?;
            Ok(Some(item))
        }
        QueueState::Dispatching => {
            let live = store
                .runs_for_item(id)?
                .iter()
                .any(|r| !r.state.is_terminal());
            Ok((!live).then_some(item))
        }
        _ => Ok(None),
    }
}

/// Why `id` may not start now: its restack fix is still to finish, or it is
/// the fix of an item that is running, which would share its branch.
pub(crate) fn held_by_stack(
    store: &archon_queue::Store,
    id: &QueueItemId,
) -> Result<Option<String>, ArchonError> {
    if store.has_pending_stack_fix(id)? {
        return Ok(Some(format!(
            "item {id} waits for its restack fix to finish"
        )));
    }
    let Some(fix) = store.stack_fix(id)? else {
        return Ok(None);
    };
    assert_ne!(&fix.owner, id, "a fix repairs another item");
    let live = store.item(&fix.owner)?.state.is_active()
        || store
            .runs_for_item(&fix.owner)?
            .iter()
            .any(|r| !r.state.is_terminal());
    Ok(live.then(|| {
        format!(
            "restack fix {id} waits for item {} to stop running",
            fix.owner
        )
    }))
}

/// Everything that can fail before the run exists. Slow parts (git) run
/// without any lock. The run uses a snapshot of its pipeline.
async fn prepare(
    ctx: &DaemonContext,
    item: &QueueItem,
    run_id: PipelineRunId,
) -> Result<Prepared, ArchonError> {
    assert_eq!(item.state, QueueState::Dispatching, "prepare follows claim");
    let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
    let (base, current) = knobs::item_specs(&env, item)?;
    // A steer may have given the item a stage list of its own (§6.6).
    let current = super::steer::shaped(ctx, item, current).await?;
    // The ceiling may have been lowered since the item was enqueued.
    let policy = env.autonomy(ctx);
    let checked = crate::authority::item_grants::for_ceiling_check(
        &*ctx.store.lock().await,
        item,
        policy.ceiling,
    )?;
    knobs::check_ceiling(&policy, &env, &base, checked)?;
    let item = &tiers_at_dispatch(ctx, &env, item, &current).await?;
    let item = &without_retired_ship(item, &current);
    let spec = Arc::new(frozen::freeze(&ctx.paths.data_dir, &run_id, &current).await?);
    crate::learned::freeze_context(ctx, &run_id, item, &spec).await?;
    let header = &spec.file.pipeline;
    if header.requires_repo && env.repo.is_none() {
        return Err(ArchonError::Invalid(format!(
            "pipeline {:?} needs a repository",
            item.pipeline
        )));
    }
    env.policy(&ctx.config).check_plan(&item.model_plan)?;
    let team = spec.file.stages.iter().any(|s| s.kind == StageKind::Team);
    let workspace = workspace(ctx, &env, item, header.workspace, team).await?;
    let added = workspace.added.clone();
    let experiments = crate::experiments::item_dir(&ctx.paths, &item.id);
    let in_git = env.repo.is_some() && header.workspace != WorkspaceMode::Scratch;
    let assembled = fork_commit(&workspace, in_git)
        .await
        .and_then(|fork| assemble(item, &env, spec, run_id, (workspace, &fork), experiments));
    match assembled {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            abandon_worktree(ctx, added).await?;
            Err(error)
        }
    }
}

/// `item` with each tiered choice resolved again from Aether's catalog as it
/// is now, for `spec`'s floors and routed to the provider the item holds, and
/// stored so a recovered run keeps it. With no catalog read yet, the last
/// resolution stands.
async fn tiers_at_dispatch(
    ctx: &DaemonContext,
    env: &RepoEnv,
    item: &QueueItem,
    spec: &PipelineSpec,
) -> Result<QueueItem, ArchonError> {
    let tiered = item
        .model_plan
        .0
        .values()
        .flatten()
        .any(|c| c.tier.is_some());
    let catalog = match tiered {
        true => crate::learned::catalog(ctx).await?,
        false => return Ok(item.clone()),
    };
    if catalog.is_empty() {
        return Ok(item.clone());
    }
    let policy = env.policy(&ctx.config);
    let weights = env.provider_weights(&ctx.config);
    let quota = crate::quota::current(ctx, weights.as_ref()).await;
    let routing = (weights.as_ref(), quota.as_ref());
    let mut store = ctx.store.lock().await;
    let mut stored = store.item(&item.id)?;
    let (plan_before, held_before) = (stored.model_plan.clone(), stored.routed_provider.clone());
    let held = knobs::item_provider(routing, &catalog, &policy, ItemRoute::new(&stored));
    let route = ItemRoute {
        id: &item.id,
        held: held.as_deref(),
    };
    let tiers = knobs::TierScope::new(&ctx.config, &policy, &catalog)
        .routed(route.by(routing.0, routing.1));
    stored.model_plan.resolve_tiers(|key, tier, provider| {
        tiers.resolve((tier, spec.tier_floor(key, tier)), provider)
    })?;
    stored.routed_provider = held.clone();
    let moved = (held_before.as_deref(), held.as_deref());
    if let Some(note) = crate::quota::switch_note(moved, quota.as_ref(), "its run") {
        stored.provider_switch = Some(note);
    }
    if stored.model_plan != plan_before || stored.routed_provider != held_before {
        tracing::info!(item = %item.id, provider = ?held, "re-resolved tiered model choices at dispatch");
        stored.updated_at = archon_core::now();
        store.update_item(&stored)?;
    }
    assert_eq!(stored.id, item.id, "the dispatched item's own record");
    Ok(QueueItem {
        model_plan: stored.model_plan,
        routed_provider: stored.routed_provider,
        provider_switch: stored.provider_switch,
        ..item.clone()
    })
}

/// `item` as its run reads it: a 1.0 item queued for a built-in keeps no
/// choice for the ship agent archond replaced. The stored item is unchanged.
fn without_retired_ship(item: &QueueItem, spec: &PipelineSpec) -> QueueItem {
    let mut read = item.clone();
    if spec.drop_retired_ship(&mut read.model_plan, read.model_reasons.as_mut()) {
        tracing::info!(item = %item.id, pipeline = %item.pipeline, "dropped the retired ship stage from a 1.0 model plan");
    }
    read
}

/// The run in `workspace` and its driver, which may still refuse the
/// pipeline's plan, prompts or gates. Its reviewers experiment in `experiments`.
fn assemble(
    item: &QueueItem,
    env: &RepoEnv,
    spec: Arc<PipelineSpec>,
    run_id: PipelineRunId,
    (workspace, base_commit): (Workspace, &str),
    experiments: PathBuf,
) -> Result<Prepared, ArchonError> {
    assert!(!spec.file.stages.is_empty(), "pipelines have a first stage");
    assert!(workspace.path.is_absolute(), "run workspaces are absolute");
    assert!(
        workspace.base.is_empty() || !base_commit.is_empty(),
        "a run with a base knows where it forked"
    );
    let repo = env.repo.as_ref();
    let base_vars = RunVars {
        task_title: &item.title,
        task_brief: &item.brief,
        repo_name: env.repo_name(),
        repo_path: repo.map(|r| r.path.as_path()),
        workspace: &workspace.path,
        branch: &workspace.branch,
        base_branch: &workspace.base,
        base_commit,
        ship_mode: repo.map_or(ShipMode::Local, |r| r.ship),
        auto_merge: item.autonomy.merge == MergeMode::Auto,
        run_id: &run_id,
        item_id: &item.id,
    }
    .to_map();
    let gates = OperatorGates::for_run(&spec, &item.autonomy);
    let driver = RunDriver::new(spec.clone(), item.model_plan.clone(), base_vars.clone())?
        .with_gates(gates)?
        .with_experiments(experiments);
    let run = PipelineRun {
        id: run_id,
        item: item.id.clone(),
        pipeline: item.pipeline.clone(),
        shipped: false,
        aether_run: None,
        workspace: workspace.path,
        team_root: workspace.team_root,
        branch: workspace.branch,
        state: RunState::Running {
            stage_id: spec.file.stages[0].id.clone(),
            attempt: 1,
        },
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    };
    Ok(Prepared {
        run,
        driver,
        base_vars,
        added: workspace.added,
    })
}

/// Where the run's change starts: the newest commit of its base, local or
/// fetched from `origin`, that the branch holds, so a stale local base adds
/// no commits to its diff. The base's name outside git or with no fork found.
async fn fork_commit(workspace: &Workspace, in_git: bool) -> Result<String, ArchonError> {
    assert!(workspace.path.is_absolute(), "run workspaces are absolute");
    if !in_git || workspace.base.is_empty() {
        return Ok(workspace.base.clone());
    }
    let (dir, base) = (workspace.path.clone(), workspace.base.clone());
    let fork =
        tokio::task::spawn_blocking(move || crate::worktree::branch_fork(&dir, &base, "HEAD"))
            .await
            .map_err(|e| ArchonError::io("fork point task", std::io::Error::other(e)))??;
    assert!(
        fork.as_ref()
            .is_none_or(|fork| matches!(fork.len(), 40 | 64)),
        "merge-base prints a full commit id"
    );
    Ok(fork.unwrap_or_else(|| workspace.base.clone()))
}

async fn workspace(
    ctx: &DaemonContext,
    env: &RepoEnv,
    item: &QueueItem,
    mode: WorkspaceMode,
    team: bool,
) -> Result<Workspace, ArchonError> {
    assert!(
        !team || mode == WorkspaceMode::Worktree,
        "validated: team pipelines use worktrees"
    );
    let tiers = env.tiers()?;
    // An `onto` item's change is for its pull request's base.
    let onto_base = item.onto.as_ref().map(|onto| onto.base_ref.clone());
    let base = onto_base.or_else(|| env.fork_base(&tiers));
    // Only an integration branch is created when it is missing.
    let release = tiers
        .integration_branch
        .as_ref()
        .map(|_| tiers.release_branch().to_owned());
    if let Some(repo) = &env.repo {
        let mode = env.config.build_cache(&ctx.config.worktrees);
        let lean = env.config.lean_builds(&ctx.config.worktrees);
        crate::build_cache::prepare(&ctx.paths, &repo.name, &item.id, mode, lean)?;
    }
    match (mode, &env.repo) {
        (WorkspaceMode::Worktree, Some(repo)) => {
            super::repo_identity::ensure_current(ctx, repo).await?;
            add_worktree(ctx, repo, item, (base, release), team).await
        }
        (WorkspaceMode::Worktree, None) => Err(ArchonError::Invalid(
            "a worktree pipeline needs a repository".into(),
        )),
        (WorkspaceMode::RepoReadonly, Some(repo)) => add_snapshot(ctx, repo, item, base).await,
        (WorkspaceMode::RepoReadonly, None) | (WorkspaceMode::Scratch, _) => {
            let path = ctx.paths.scratch.join(item.id.as_str());
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| ArchonError::io(format!("creating {}", path.display()), e))?;
            Ok(Workspace {
                path,
                team_root: None,
                branch: String::new(),
                base: base.unwrap_or_default(),
                added: None,
            })
        }
    }
}

/// The item's worktree, added now or reused from an earlier run; a team
/// run's is `main` under the item's directory. Each fresh retry of the
/// item gives it a new branch. With a `release` branch, `base` is the
/// integration branch, created from it when missing. An `onto` item's
/// branch starts from its pull request's frozen head, for that pull
/// request's base.
async fn add_worktree(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    (base, release): (Option<String>, Option<String>),
    team: bool,
) -> Result<Workspace, ArchonError> {
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let target = ctx.store.lock().await.stack_fix(&item.id)?;
    let parents = parents(ctx, repo, item, target.as_ref(), base.as_deref()).await?;
    let stacked_on = parents.iter().find(|p| p.is_open());
    let branch = target
        .as_ref()
        .map(|t| t.branch.clone())
        .unwrap_or_else(|| manager.branch_name(&item.id, &item.title, item.fresh_retries));
    let fork = match (&target, &item.onto, stacked_on, base.as_deref()) {
        (None, Some(onto), ..) => onto_fork(ctx, repo, onto, &branch).await?,
        (None, None, None, Some(base)) => {
            fork_point(ctx, repo, item, (base, release.as_deref()), &branch).await?
        }
        _ => None,
    };
    let base = stacked_on.map(|p| p.parent_branch.clone()).or(base);
    let start = fork.clone().or_else(|| base.clone());
    let item_id = target
        .as_ref()
        .map(|t| t.owner.clone())
        .unwrap_or_else(|| item.id.clone());
    let team = target.as_ref().map_or(team, |t| t.team);
    let cleanup_owner = item_id.clone();
    let source_repo = repo.clone();
    let repo = repo.clone();
    let repo_path = repo.path.clone();
    let mut tree = tokio::task::spawn_blocking(move || match team {
        true => {
            manager.create_team_run(&repo.path, &repo.name, &item_id, &branch, start.as_deref())
        }
        false => manager.create(&repo.path, &repo.name, &item_id, &branch, start.as_deref()),
    })
    .await
    .map_err(|e| ArchonError::io("worktree creation task", std::io::Error::other(e)))??;
    // Forked from `origin`'s commit, the change is still for the base branch by name.
    if let (Some(_), Some(base)) = (&fork, base) {
        tree.base = base;
    }
    assert!(
        tree.path.starts_with(&ctx.paths.worktrees),
        "worktrees are added under the worktree root"
    );
    let workspace = worktree_workspace(tree, repo_path, cleanup_owner, team);
    if target.is_none() {
        merge_stack(ctx, &source_repo, &workspace, &parents).await?;
    }
    Ok(workspace)
}

/// The parents `item`'s branch stacks on: the stack owner's for a stack fix
/// `target`, none for an `onto` item, else its unmerged dependencies'.
async fn parents(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    target: Option<&StackFix>,
    base: Option<&str>,
) -> Result<Vec<StackParent>, ArchonError> {
    assert!(
        item.onto.is_none() || item.dependencies.is_empty(),
        "refused at enqueue: an `onto` item has no dependency"
    );
    match (target, &item.onto) {
        (Some(target), _) => ctx.store.lock().await.stack_parents(&target.owner),
        (None, Some(_)) => Ok(Vec::new()),
        (None, None) => super::restack::parents(ctx, item, repo, base).await,
    }
}

/// The frozen head of `onto`, fetched when missing, where a new branch of an
/// `onto` item starts; `None` when `branch` exists already.
async fn onto_fork(
    ctx: &DaemonContext,
    repo: &Repo,
    onto: &OntoPullRequest,
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    assert!(!branch.is_empty(), "a worktree run has a branch");
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let (onto, branch) = (onto.clone(), branch.to_owned());
    let head = tokio::task::spawn_blocking(move || -> Result<_, ArchonError> {
        if crate::worktree::branch_exists(&path, &branch)? {
            return Ok(None);
        }
        super::onto::fetch_head(&data_dir, &path, &onto).map(Some)
    })
    .await
    .map_err(|e| ArchonError::io("onto head fetch task", std::io::Error::other(e)))??;
    assert!(head.as_deref().is_none_or(stack::is_commit_id));
    Ok(head)
}

/// Where a new, unstacked branch of `item` starts: `origin`'s head of `base` in
/// `pr` mode, else the local `base` (`None`). With `release`, `base` is the
/// integration branch, created from `release` where it is missing.
async fn fork_point(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    (base, release): (&str, Option<&str>),
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    let fork = match repo.ship {
        ShipMode::Pr => remote_fork(ctx, repo, item, (base, release), branch).await?,
        ShipMode::Local => None,
    };
    if let (None, Some(release)) = (&fork, release) {
        let (path, base, release) = (repo.path.clone(), base.to_owned(), release.to_owned());
        let created = tokio::task::spawn_blocking(move || local_branch(&path, &base, &release))
            .await
            .map_err(|e| ArchonError::io("integration branch task", std::io::Error::other(e)))??;
        if let Some(text) = created {
            tracing::info!(item = %item.id, repo = %repo.name, "{text}");
        }
    }
    Ok(fork)
}

/// Create the local `branch` at `from` when it is missing; what was done, if anything.
fn local_branch(
    repo: &std::path::Path,
    branch: &str,
    from: &str,
) -> Result<Option<String>, ArchonError> {
    if crate::worktree::branch_exists(repo, branch)? {
        return Ok(None);
    }
    let Some(sha) = crate::worktree::commit_id(repo, from)? else {
        return Err(ArchonError::Invalid(format!(
            "neither the integration branch `{branch}` nor the release branch `{from}` exists \
             in {}",
            repo.display()
        )));
    };
    assert!(stack::is_commit_id(&sha), "commit_id names a full commit");
    let reference = format!("refs/heads/{branch}");
    // The empty old value refuses to move a branch made meanwhile.
    let args = [
        "update-ref",
        "-m",
        "archon: integration branch",
        &reference,
        &sha,
        "",
    ];
    crate::worktree::git(repo, &args)?;
    Ok(Some(format!(
        "created the integration branch `{branch}` from `{from}`"
    )))
}

/// The commit a new branch of `item` forks from in `pr` mode: `origin`'s head of
/// `base`, fetched now, as the local `base` may be behind what GitHub merged; with
/// `release`, an integration `base` missing on `origin` is first pushed there from
/// it. `None` keeps the local `base`: the branch exists already, the repository has
/// no `origin`, or the fetch failed, which the item's feed says.
async fn remote_fork(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    (base, release): (&str, Option<&str>),
    branch: &str,
) -> Result<Option<String>, ArchonError> {
    assert_eq!(
        repo.ship,
        ShipMode::Pr,
        "only a pr repository forks from `origin`"
    );
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let (base_name, branch) = (base.to_owned(), branch.to_owned());
    let from = release.map(str::to_owned);
    // Only an integration branch made on `origin` is pushed, as the repository's login.
    let token = match from {
        Some(_) => super::gh::token(&ctx.config.github.binary, repo).await,
        None => Ok(None),
    };
    let fetched = tokio::task::spawn_blocking(move || -> Result<_, ArchonError> {
        if crate::worktree::branch_exists(&path, &branch)? {
            return Ok(Ok((false, None)));
        }
        let shadow = Shadow::of(&data_dir, &path)?;
        let created = match &from {
            Some(from) => token.and_then(|token| {
                stack::ensure_remote_branch(&shadow, &path, (&base_name, from), token.as_ref())
            }),
            None => Ok(false),
        };
        Ok(created.and_then(|created| {
            stack::fetch_head(&shadow, &path, &base_name).map(|fork| (created, fork))
        }))
    })
    .await
    .map_err(|e| ArchonError::io("base fetch task", std::io::Error::other(e)))??;
    let error = match fetched {
        Ok((created, fork)) => {
            if created {
                created_notice(ctx, item, base, release).await?;
            }
            return Ok(fork);
        }
        Err(error) => error,
    };
    tracing::warn!(item = %item.id, %base, %error, "base not fetched; forking from the local branch");
    let text = format!(
        "Could not fetch `origin/{base}` before forking {}: {error}. It forks from the local \
         `{base}`, which may be behind GitHub; `refresh_item` merges the newer base in later.",
        item.id
    );
    super::finish::notify(ctx, NotificationKind::Decision, &text, Some(&item.id), None).await?;
    Ok(None)
}

/// The operator hears that Archon made the integration branch `base` on `origin`.
async fn created_notice(
    ctx: &DaemonContext,
    item: &QueueItem,
    base: &str,
    release: Option<&str>,
) -> Result<(), ArchonError> {
    let release = release.expect("only an integration branch is created");
    assert_ne!(base, release, "validated: distinct branches");
    let text = format!(
        "Created the integration branch `origin/{base}` from `origin/{release}` for {}: \
         items fork from it and their pull requests target it.",
        item.id
    );
    super::finish::notify(ctx, NotificationKind::Decision, &text, Some(&item.id), None).await
}

async fn merge_stack(
    ctx: &DaemonContext,
    repo: &Repo,
    workspace: &Workspace,
    parents: &[StackParent],
) -> Result<(), ArchonError> {
    assert!(workspace.path.starts_with(&ctx.paths.worktrees));
    assert!(!workspace.branch.is_empty());
    let result =
        super::restack::merge_others(ctx, repo, &workspace.path, &workspace.branch, parents).await;
    if let Err(error) = result {
        let error = match abandon_worktree(ctx, workspace.added.clone()).await {
            Ok(()) => error,
            Err(cleanup) => error.with_detail(&format!("worktree cleanup also failed: {cleanup}")),
        };
        return Err(error);
    }
    Ok(())
}

fn worktree_workspace(
    tree: crate::worktree::Worktree,
    repo_path: PathBuf,
    item: QueueItemId,
    team: bool,
) -> Workspace {
    assert!(tree.path.is_absolute());
    assert!(!tree.branch.is_empty());
    let added = tree.added.then(|| Cleanup {
        item,
        repo_path,
        worktree: tree.path.clone(),
        snapshot: false,
    });
    let team_root = team.then(|| {
        let root = archon_pipeline::team::team_root(&tree.path);
        root.expect("create_team_run adds <item>/main").to_owned()
    });
    Workspace {
        path: tree.path,
        team_root,
        branch: tree.branch,
        base: tree.base,
        added,
    }
}

/// A detached snapshot of the repository's `HEAD` commit for a
/// `repo-readonly` run, so its agent never works in the operator's checkout.
async fn add_snapshot(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    base: Option<String>,
) -> Result<Workspace, ArchonError> {
    // Adding one replaces a leftover snapshot, which a live run would still be
    // working in. `claim_item` already refuses that; this guards later callers.
    let runs = ctx.store.lock().await.runs_for_item(&item.id)?;
    if runs.iter().any(|r| !r.state.is_terminal()) {
        return Err(ArchonError::Invalid(format!(
            "item {} has a live run, whose snapshot is not replaced",
            item.id
        )));
    }
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let (repo_path, name, item_id) = (repo.path.clone(), repo.name.clone(), item.id.clone());
    let snapshot =
        tokio::task::spawn_blocking(move || manager.create_snapshot(&repo_path, &name, &item_id))
            .await
            .map_err(|e| ArchonError::io("snapshot creation task", std::io::Error::other(e)))??;
    assert!(
        snapshot.path.starts_with(&ctx.paths.worktrees),
        "snapshots are added under the worktree root"
    );
    assert!(
        is_snapshot_of(&snapshot.path, &item.id),
        "a snapshot is named for its item, so run end and gc know it"
    );
    tracing::info!(item = %item.id, snapshot = %snapshot.path.display(), commit = %snapshot.commit, "added a snapshot");
    let added = Cleanup {
        item: item.id.clone(),
        repo_path: repo.path.clone(),
        worktree: snapshot.path.clone(),
        snapshot: true,
    };
    Ok(Workspace {
        path: snapshot.path,
        team_root: None,
        branch: String::new(),
        base: base.unwrap_or_default(),
        added: Some(added),
    })
}

/// Insert the run and move the item to `running` under the run table, so a
/// cancel in between is seen: a cancelled item is left alone.
async fn launch(
    ctx: &DaemonContext,
    table: &mut RunTable,
    prepared: Prepared,
) -> Result<FollowUp, ArchonError> {
    let Prepared {
        run,
        mut driver,
        base_vars,
        added,
    } = prepared;
    let inserted = {
        let mut store = ctx.store.lock().await;
        let item = store.item(&run.item)?;
        let live = store
            .runs_for_item(&run.item)?
            .iter()
            .any(|r| !r.state.is_terminal());
        if item.state != QueueState::Dispatching || live {
            tracing::info!(item = %run.item, state = item.state.kind(), "item changed while starting; not starting it");
            drop(store);
            frozen::discard(&ctx.paths.data_dir, &run.id);
            // A live run of the item works in the item's worktree.
            if !live {
                abandon_worktree(ctx, added).await?;
            }
            return Ok(FollowUp::default());
        }
        recover::save_run_vars(&mut store, &run.id, &base_vars)?;
        if let Err(error) = store.insert_run(&run) {
            drop(store);
            frozen::discard(&ctx.paths.data_dir, &run.id);
            abandon_worktree(ctx, added).await?;
            let message = format!("inserting its run failed: {error}");
            return fail_unstarted_locked(ctx, &run.item, &message).await;
        }
        let running = QueueState::Running {
            run: run.id.clone(),
        };
        let started = store.transition_item(&run.item, running, archon_core::now());
        started.and_then(|item| super::steer::correct_shaped(&store, &run).map(|()| item))
    };
    // The run exists now, so only finish_run can settle the item.
    if let Err(error) = inserted {
        let stage = first_stage(&run);
        let message = format!("could not start the run: {error}");
        return finish::fail_run(ctx, table, run, stage, message, true).await;
    }
    let actions = driver.start();
    table.drivers.insert(run.id.clone(), driver);
    settle::apply(ctx, table, run, actions).await
}

/// Remove the worktree a start added before it gave up, since no run row
/// leads `archon gc` to it. A reused worktree belongs to an earlier run of
/// the item and is left alone.
async fn abandon_worktree(ctx: &DaemonContext, added: Option<Cleanup>) -> Result<(), ArchonError> {
    let Some(tree) = added else {
        return Ok(());
    };
    assert!(
        tree.worktree.starts_with(&ctx.paths.worktrees),
        "only Archon's own worktrees are abandoned"
    );
    tracing::info!(worktree = %tree.worktree.display(), "removing the worktree of an abandoned start");
    finish::remove_worktree(ctx, tree, ABANDONED, None).await
}

fn first_stage(run: &PipelineRun) -> String {
    let stage = run.state.live_stage().expect("a starting run is live");
    stage.to_owned()
}

/// `dispatching → failed` before any run exists, with a `Blocker`.
pub(super) async fn fail_unstarted(
    ctx: &DaemonContext,
    id: &QueueItemId,
    error: &str,
) -> Result<FollowUp, ArchonError> {
    let _table = ctx.runs.lock().await;
    fail_unstarted_locked(ctx, id, error).await
}

/// [`fail_unstarted`] for a caller that holds the run table.
async fn fail_unstarted_locked(
    ctx: &DaemonContext,
    id: &QueueItemId,
    error: &str,
) -> Result<FollowUp, ArchonError> {
    let item = {
        let mut store = ctx.store.lock().await;
        if store.item(id)?.state != QueueState::Dispatching {
            return Ok(FollowUp::default());
        }
        let failed = QueueState::Failed {
            run: None,
            error: format!(
                "could not start: {error}. `archon queue retry {id}` queues it again once fixed"
            ),
        };
        store.transition_item(id, failed, archon_core::now())?
    };
    tracing::warn!(item = %id, %error, "item failed to start");
    finish::announce(ctx, &item, None, true, None).await?;
    Ok(FollowUp::poke())
}
