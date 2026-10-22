//! Team stages (ARCHITECTURE §13.1): each role's worker worktree, made
//! ready before its agent is issued, and the merge of the roles' branches
//! onto the run branch once every role passed.
//!
//! A merge runs git without any lock. Its outcome is persisted in `kv`
//! before the driver acts on it, so a rebuilt driver replays it; a crash
//! before that repeats the merge, which skips branches already merged.

use std::path::PathBuf;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::notify::NotificationKind;
use archon_core::run::PipelineRun;
use archon_pipeline::RunDriver;
use archon_pipeline::team::{MergeOutcome, MergePlan};
use archon_queue::Store;

use super::{FollowUp, RunTable, finish, recover, settle};
use crate::context::DaemonContext;
use crate::resolve;
use crate::worktree::team::{self as git_team, SavedWork, TeamMerge};

/// `kv`: `team_merge:<run>:<stage>:<attempt>` → the JSON [`MergeOutcome`].
const MERGE_KEY_PREFIX: &str = "team_merge:";

fn merge_key(run: &PipelineRunId, stage_id: &str, attempt: u32) -> String {
    format!("{MERGE_KEY_PREFIX}{run}:{stage_id}:{attempt}")
}

/// A team slot's worker worktree, as its stage run's claim found it.
#[derive(Debug, Clone)]
pub(super) struct Worker {
    pub item: QueueItemId,
    pub run: PipelineRunId,
    /// `None` when the item lost its repository; the slot is refused then.
    pub repo_path: Option<PathBuf>,
    /// The run worktree, which must be committed before a fork.
    pub run_tree: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// The run branch a new worker branch forks from.
    pub start: String,
}

/// Make `worker`'s worktree ready: on its branch, forked from the run
/// branch when new. Git runs off the async runtime and without locks.
/// What an earlier role left there, committed or moved aside by the
/// daemon, goes to the run's progress.
pub(super) async fn ready_worker(ctx: &DaemonContext, worker: Worker) -> Result<(), ArchonError> {
    let Some(repo_path) = worker.repo_path.clone() else {
        return Err(ArchonError::Invalid(
            "a team slot needs its item's repository".into(),
        ));
    };
    assert!(worker.path.is_absolute(), "worker worktrees are absolute");
    assert!(!worker.start.is_empty(), "team runs have a run branch");
    let (item, run) = (worker.item.clone(), worker.run.clone());
    let kept = tokio::task::spawn_blocking(move || {
        let (tree, path) = (&worker.run_tree, &worker.path);
        git_team::ensure_worker(&repo_path, tree, path, &worker.branch, &worker.start)
    })
    .await
    .map_err(|e| ArchonError::io("worker worktree task", std::io::Error::other(e)))??;
    if let Some(kept) = kept {
        post_kept(ctx, &item, &run, &kept.describe()).await;
    }
    Ok(())
}

/// Tell the operator, in the run's progress, what the daemon committed for
/// roles that left work uncommitted.
async fn report_saved(
    ctx: &DaemonContext,
    item: &QueueItemId,
    run: &PipelineRunId,
    saved: &[SavedWork],
) {
    for work in saved {
        assert!(!work.files.is_empty(), "only committed files are reported");
        post_kept(ctx, item, run, &work.describe()).await;
    }
}

/// Post `text`, saying what the daemon did with a role's work, to the
/// run's progress. The work was committed or moved already, and a retry
/// finds nothing left to do, so a notice that cannot be posted goes to
/// the daemon's log instead of failing the slot or the merge.
async fn post_kept(ctx: &DaemonContext, item: &QueueItemId, run: &PipelineRunId, text: &str) {
    assert!(!text.is_empty(), "a notice says what was done");
    let posted = finish::notify(ctx, NotificationKind::Progress, text, Some(item), Some(run)).await;
    if let Err(error) = posted {
        tracing::error!(%run, %error, notice = %text, "kept-work notice not posted");
    }
}

/// A driver asked for `plan`: remember it until the merge ran.
pub(super) fn queue(table: &mut RunTable, run: &PipelineRun, plan: MergePlan) -> FollowUp {
    assert!(run.team_root.is_some(), "only team runs merge");
    assert_eq!(plan.into, run.branch, "a team merges onto its run branch");
    table.merges.insert(run.id.clone(), plan);
    FollowUp {
        merge: vec![run.id.clone()],
        ..FollowUp::default()
    }
}

/// Run `run`'s queued merge once: git without locks, then persist the
/// outcome and feed it to the driver. A git failure fails the run; a
/// failure to record the outcome queues the merge again. An outcome
/// already persisted is not merged again: the driver may have moved on,
/// and the next attempt's roles fork from the run worktree meanwhile.
pub(super) async fn merge(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let Some((plan, run, repo_path, recorded)) = claim(ctx, run_id).await? else {
        return Ok(FollowUp::default());
    };
    let merged = match recorded {
        Some(outcome) => Ok(TeamMerge {
            outcome,
            saved: Vec::new(),
        }),
        None => run_merge(repo_path.clone(), run.workspace.clone(), plan.clone()).await,
    };
    let mut table = ctx.runs.lock().await;
    let released = table.merging.remove(run_id);
    assert!(released, "a claimed merge is in flight until it ends");
    if table.merges.get(run_id) != Some(&plan) {
        // The run finished or was resumed meanwhile; its replay owns it now.
        return Ok(FollowUp::poke());
    }
    table.merges.remove(run_id);
    let stage_id = plan.stage_id.clone();
    let mut cleanup = None;
    let result = match merged {
        Ok(merged) => {
            if merged.outcome == MergeOutcome::Merged {
                cleanup = Some((run.workspace.clone(), run.item.clone()));
            }
            record(ctx, &mut table, run, &plan, merged).await
        }
        Err(error) => {
            let message = format!("merging the team's branches failed: {error}");
            finish::fail_run(ctx, &mut table, run, stage_id, message, true).await
        }
    };
    if result.is_err() {
        requeue(&mut table, run_id, plan);
        return result;
    }
    drop(table);
    if let Some((tree, item)) = cleanup {
        delete_branches(ctx, &item, run_id, repo_path, tree, plan).await;
    }
    result
}

/// Delete the branches of a team that merged, now its outcome is
/// persisted. The merge is done and a retry finds the branches gone or
/// deletes them again, so a failure is posted to the run's progress rather
/// than failing the run.
async fn delete_branches(
    ctx: &DaemonContext,
    item: &QueueItemId,
    run: &PipelineRunId,
    repo_path: Option<PathBuf>,
    tree: PathBuf,
    plan: MergePlan,
) {
    let Some(repo_path) = repo_path else {
        return;
    };
    assert!(tree.is_absolute(), "run worktrees are absolute");
    let stage = plan.stage_id.clone();
    let cleaned = tokio::task::spawn_blocking(move || {
        git_team::delete_merged_branches(&repo_path, &tree, &plan)
    })
    .await
    .map_err(|e| ArchonError::io("team cleanup task", std::io::Error::other(e)))
    .and_then(|done| done);
    if let Err(error) = cleaned {
        let text =
            format!("The merged role branches of team stage `{stage}` were not deleted: {error}");
        post_kept(ctx, item, run, &text).await;
    }
}

/// Merge `plan` in the run worktree `tree` of the repository at
/// `repo_path`, off the async runtime.
async fn run_merge(
    repo_path: Option<PathBuf>,
    tree: PathBuf,
    plan: MergePlan,
) -> Result<TeamMerge, ArchonError> {
    assert!(tree.is_absolute(), "run worktrees are absolute");
    assert!(!plan.roles.is_empty(), "a team has roles to merge");
    let Some(repo_path) = repo_path else {
        return Err(ArchonError::Invalid(
            "a team merge needs its item's repository".into(),
        ));
    };
    tokio::task::spawn_blocking(move || git_team::merge_team(&repo_path, &tree, &plan))
        .await
        .map_err(|e| ArchonError::io("team merge task", std::io::Error::other(e)))?
}

/// Recording a merge failed part way: queue it again for the sweep, and
/// drop the driver, which may have acted already, so the retry rebuilds
/// it from the store. An outcome persisted already is reused rather than
/// merged again, and replayed, so the retry is safe to repeat.
fn requeue(table: &mut RunTable, run_id: &PipelineRunId, plan: MergePlan) {
    assert!(
        !table.merging.contains(run_id),
        "the failed merge is not in flight"
    );
    tracing::warn!(run = %run_id, stage = %plan.stage_id, "team merge not recorded; retrying");
    table.drivers.remove(run_id);
    table.merges.entry(run_id.clone()).or_insert(plan);
    debug_assert!(stalled(table).contains(run_id), "the sweep retries it");
}

/// A claimed merge: its plan, run, the item's repository (`None` when it
/// lost it) and the outcome persisted already, if any.
type ClaimedMerge = (
    MergePlan,
    PipelineRun,
    Option<PathBuf>,
    Option<MergeOutcome>,
);

/// The queued merge of a live run, marked in flight; `None` when there is
/// none or another caller runs it.
async fn claim(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<Option<ClaimedMerge>, ArchonError> {
    let mut table = ctx.runs.lock().await;
    let Some(plan) = table.merges.get(run_id).cloned() else {
        return Ok(None);
    };
    if table.merging.contains(run_id) {
        return Ok(None);
    }
    let (run, repo_path, recorded) = {
        let store = ctx.store.lock().await;
        let key = merge_key(run_id, &plan.stage_id, plan.attempt);
        let run = store.run(run_id)?;
        let repo = store.item(&run.item)?.repo;
        let repo_path = repo
            .map(|r| store.repo(&resolve::repo_ref(&r)).map(|found| found.path))
            .transpose()?;
        (run, repo_path, store.kv_get(&key)?)
    };
    if run.state.is_terminal() {
        table.merges.remove(run_id);
        return Ok(None);
    }
    assert!(run.team_root.is_some(), "only team runs queue merges");
    let recorded = recorded
        .map(|raw| serde_json::from_str::<MergeOutcome>(&raw))
        .transpose()
        .map_err(|e| ArchonError::Corrupt(format!("team merge outcome of run {run_id}: {e}")))?;
    table.merging.insert(run_id.clone());
    Ok(Some((plan, run, repo_path, recorded)))
}

/// Persist the merge's outcome, report what it committed for roles, then
/// let the driver go on or loop back with it. A retry after a failure here
/// reuses the persisted outcome, so the report is never repeated; it is
/// made once the outcome is persisted and cannot fail ([`report_saved`]).
async fn record(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    plan: &MergePlan,
    merged: TeamMerge,
) -> Result<FollowUp, ArchonError> {
    let TeamMerge { outcome, saved } = merged;
    let all = {
        let mut store = ctx.store.lock().await;
        let key = merge_key(&run.id, &plan.stage_id, plan.attempt);
        store.kv_set(&key, &serde_json::to_string(&outcome)?)?;
        store.stage_runs(&run.id)?
    };
    report_saved(ctx, &run.item, &run.id, &saved).await;
    match &outcome {
        MergeOutcome::Merged => {}
        MergeOutcome::Conflict { role, files } => {
            tracing::info!(run = %run.id, %role, files = files.len(), "team merge conflicted");
        }
        MergeOutcome::Refused { role, reason } => {
            tracing::info!(run = %run.id, %role, %reason, "team merge refused");
        }
    }
    let Some(driver) = table.drivers.get_mut(&run.id) else {
        // An earlier rebuild failed; retry it, which replays the outcome.
        return recover::resume_run(ctx, table, run, &all).await;
    };
    let actions = driver.on_team_merged(&plan.stage_id, plan.attempt, outcome, &all)?;
    settle::apply(ctx, table, run, actions).await
}

/// Feed `driver` every merge outcome persisted for `run`.
pub(super) fn replay_merges(
    store: &Store,
    run: &PipelineRunId,
    driver: &mut RunDriver,
) -> Result<(), ArchonError> {
    let prefix = format!("{MERGE_KEY_PREFIX}{run}:");
    for (key, raw) in store.kv_prefix(&prefix)? {
        let rest = key.strip_prefix(&prefix).expect("ranged by prefix");
        let parsed = rest
            .rsplit_once(':')
            .and_then(|(stage, attempt)| Some((stage, attempt.parse::<u32>().ok()?)));
        let Some((stage, attempt)) = parsed else {
            return Err(ArchonError::Corrupt(format!(
                "malformed team merge key {key}"
            )));
        };
        let outcome: MergeOutcome = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key}: {e}")))?;
        driver.record_merge(stage, attempt, outcome)?;
    }
    Ok(())
}

/// Delete the merge outcomes of finished runs, which only rebuild a live
/// run's driver, and of runs that no longer exist. Returns how many went.
pub(crate) fn prune_finished_merges(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(MERGE_KEY_PREFIX)? {
        let rest = key
            .strip_prefix(MERGE_KEY_PREFIX)
            .expect("ranged by prefix");
        let raw_run = rest.split(':').next().unwrap_or_default();
        let run = PipelineRunId::parse(raw_run)
            .map_err(|e| ArchonError::Corrupt(format!("team merge key {key}: {e}")))?;
        let finished = match store.run(&run) {
            Ok(found) => found.state.is_terminal(),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if finished {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Runs whose merge is queued but not running: the sweep retries them.
pub(super) fn stalled(table: &RunTable) -> Vec<PipelineRunId> {
    table
        .merges
        .keys()
        .filter(|run| !table.merging.contains(*run))
        .cloned()
        .collect()
}
