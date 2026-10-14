//! Starting an item: pipeline, workspace, run, driver, first spawns.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use archon_core::autonomy::MergeMode;
use archon_core::catalog::{StageKind, WorkspaceMode, is_snapshot_of};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, RunState};
use archon_pipeline::autonomy::OperatorGates;
use archon_pipeline::{PipelineSpec, RunDriver, RunVars};

use super::{Cleanup, FollowUp, RunTable, finish, frozen, recover, settle};
use crate::context::DaemonContext;
use crate::knobs;
use crate::resolve::RepoEnv;
use crate::worktree::WorktreeManager;

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
    let prepared = match prepare(ctx, &item, run_id.clone()).await {
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
    let collecting = ctx.collecting.contains(id);
    match item.state {
        QueueState::Queued if collecting => Ok(None),
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
    knobs::check_ceiling(&env.autonomy(&ctx.config), &env, &base, item.autonomy)?;
    let spec = Arc::new(frozen::freeze(&ctx.paths.data_dir, &run_id, &current).await?);
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
    match assemble(item, &env, spec, run_id, workspace) {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            abandon_worktree(ctx, added).await?;
            Err(error)
        }
    }
}

/// The run in `workspace` and its driver, which may still refuse the
/// pipeline's plan, prompts or gates.
fn assemble(
    item: &QueueItem,
    env: &RepoEnv,
    spec: Arc<PipelineSpec>,
    run_id: PipelineRunId,
    workspace: Workspace,
) -> Result<Prepared, ArchonError> {
    assert!(!spec.file.stages.is_empty(), "pipelines have a first stage");
    assert!(workspace.path.is_absolute(), "run workspaces are absolute");
    let repo = env.repo.as_ref();
    let base_vars = RunVars {
        task_title: &item.title,
        task_brief: &item.brief,
        repo_name: env.repo_name(),
        repo_path: repo.map(|r| r.path.as_path()),
        workspace: &workspace.path,
        branch: &workspace.branch,
        base_branch: &workspace.base,
        ship_mode: repo.map_or(ShipMode::Local, |r| r.ship),
        auto_merge: item.autonomy.merge == MergeMode::Auto,
        run_id: &run_id,
        item_id: &item.id,
    }
    .to_map();
    let gates = OperatorGates::for_run(&spec, &item.autonomy);
    let driver = RunDriver::new(spec.clone(), item.model_plan.clone(), base_vars.clone())?
        .with_gates(gates)?;
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
    let base = env
        .repo
        .as_ref()
        .and_then(|r| r.base_branch.clone())
        .or_else(|| env.config.repo.base_branch.clone());
    match (mode, &env.repo) {
        (WorkspaceMode::Worktree, Some(repo)) => add_worktree(ctx, repo, item, base, team).await,
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
/// item gives it a new branch.
async fn add_worktree(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItem,
    base: Option<String>,
    team: bool,
) -> Result<Workspace, ArchonError> {
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let branch = manager.branch_name(&item.id, &item.title, item.fresh_retries);
    let (repo, item_id) = (repo.clone(), item.id.clone());
    let repo_path = repo.path.clone();
    let tree = tokio::task::spawn_blocking(move || match team {
        true => manager.create_team_run(&repo.path, &repo.name, &item_id, &branch, base.as_deref()),
        false => manager.create(&repo.path, &repo.name, &item_id, &branch, base.as_deref()),
    })
    .await
    .map_err(|e| ArchonError::io("worktree creation task", std::io::Error::other(e)))??;
    assert!(
        tree.path.starts_with(&ctx.paths.worktrees),
        "worktrees are added under the worktree root"
    );
    let added = tree.added.then(|| Cleanup {
        repo_path,
        worktree: tree.path.clone(),
        snapshot: false,
    });
    let team_root = team.then(|| {
        let root = archon_pipeline::team::team_root(&tree.path);
        root.expect("create_team_run adds <item>/main").to_owned()
    });
    Ok(Workspace {
        path: tree.path,
        team_root,
        branch: tree.branch,
        base: tree.base,
        added,
    })
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
        store.transition_item(&run.item, running, archon_core::now())
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
    finish::remove_worktree(ctx, tree, ABANDONED).await
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
            error: format!("could not start: {error}"),
        };
        store.transition_item(id, failed, archon_core::now())?
    };
    tracing::warn!(item = %id, %error, "item failed to start");
    finish::announce(ctx, &item, None, true, None).await?;
    Ok(FollowUp::poke())
}
