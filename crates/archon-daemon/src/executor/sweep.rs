//! Driving live stage runs from the store: re-issuing `spawning` ones,
//! stopping agents of finished runs, and the deadline timer. Everything
//! here is derived from persisted state, so it also repairs a crash.

use std::collections::BTreeMap;

use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::run::{StageOutcome, StageRun, StageState};

use super::{
    FollowUp, descendants, load_spawn, resume, run_follow_up, run_node, settle, ship, siblings,
    team,
};
use crate::context::DaemonContext;

/// Aether settles a timed-out agent itself; the daemon steps in after this.
const DEADLINE_GRACE_SECS: i64 = 60;

/// One pass over every live stage run (ARCHITECTURE §6.3, §16).
pub(crate) async fn sweep_live_stages(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let now = archon_core::now();
    let grace = chrono::Duration::seconds(DEADLINE_GRACE_SECS);
    let (in_flight, merge, ship) = {
        let table = ctx.runs.lock().await;
        let due = ship::stalled(&table, std::time::Instant::now());
        (table.in_flight.clone(), team::stalled(&table), due)
    };
    let mut follow = FollowUp {
        merge,
        ship,
        ..FollowUp::default()
    };
    {
        let store = ctx.store.lock().await;
        let mut finished: BTreeMap<PipelineRunId, bool> = BTreeMap::new();
        for stage in store.live_stage_runs()? {
            if in_flight.contains(&stage.id) {
                continue;
            }
            if stage.state == StageState::Spawning {
                follow.issue.push(stage.id);
                continue;
            }
            let run_done = match finished.get(&stage.run) {
                Some(done) => *done,
                None => {
                    let done = store.run(&stage.run)?.state.is_terminal();
                    *finished.entry(stage.run.clone()).or_insert(done)
                }
            };
            if run_done {
                follow.stop.push(stage.run.clone());
                continue;
            }
            follow.roster.push(stage.run.clone());
            if stage.aether_agent.is_some() && stage.aether_task.is_none() {
                follow.resumed.push(stage.id.clone());
            }
            let deadline = match load_spawn(&store, &stage.id) {
                Ok(spawn) => spawn.request.deadline,
                Err(ArchonError::NotFound(what)) => {
                    tracing::warn!(stage = %stage.id, "no {what}; no deadline timer");
                    None
                }
                Err(e) => return Err(e),
            };
            if deadline.is_some_and(|d| d + grace < now) {
                follow.time_out.push(stage.id);
            }
        }
        follow.retire = run_node::runs_to_retire(&store)?;
        follow.sibling_stops = siblings::owed(&store)?;
        follow.steers = super::steer::owed(&store)?;
        follow.descendant_stops = descendants::owed(&store)?;
    }
    run_follow_up(ctx, follow).await
}

/// A spawn whose task already settled (its event may predate the stage
/// run knowing its agent).
pub(super) async fn check_outcome(
    ctx: &DaemonContext,
    stage: &StageRunId,
    task: &AetherTaskId,
) -> Result<FollowUp, ArchonError> {
    match ctx.backend.task_outcome(task).await {
        Ok(Some(outcome)) => settle::settle(ctx, stage, outcome).await,
        Ok(None) => Ok(FollowUp::default()),
        Err(error) => {
            // The event pump's reconciliation covers this task on reconnect.
            tracing::warn!(%stage, %task, %error, "could not check a new stage task");
            Ok(FollowUp::default())
        }
    }
}

/// Stop every live stage agent of a finished run, then retire its run node
/// once no stage agent is left (each stage settles with its own outcome
/// first). The sweep repeats the retirement until it is done.
pub(super) async fn stop_run(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let live = live_stages_of_finished(ctx, run).await?;
    let reason = format!("pipeline run {run} finished");
    let mut follow = FollowUp::default();
    for stage in live {
        match (&stage.aether_agent, &stage.aether_task) {
            (Some(agent), Some(task)) => {
                let cancelled = StageOutcome::Cancelled {
                    reason: Some(reason.clone()),
                };
                follow.merge(
                    cancel_and_settle(ctx, &stage.id, agent, task, &reason, cancelled).await?,
                );
            }
            // Resumed on a kept agent: it has no task to ask about (§6.7).
            (Some(agent), None) if stage.state == StageState::Running => {
                let cancelled = StageOutcome::Cancelled {
                    reason: Some(reason.clone()),
                };
                follow.merge(resume::stop(ctx, &stage.id, agent, &reason, cancelled).await?);
            }
            // Its create_agent may have reached Aether: issue it under the same
            // key, and the finished run then cancels the agent it gets back.
            _ => follow.issue.push(stage.id),
        }
    }
    let still_live = live_stages_of_finished(ctx, run).await?;
    if still_live.is_empty() {
        follow.retire.push(run.clone());
    }
    Ok(follow)
}

async fn live_stages_of_finished(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<Vec<StageRun>, ArchonError> {
    let store = ctx.store.lock().await;
    let state = store.run(run)?.state;
    assert!(state.is_terminal(), "only finished runs are stopped");
    Ok(store
        .stage_runs(run)?
        .into_iter()
        .filter(|s| !s.state.is_terminal())
        .collect())
}

/// Cancel a `running` stage agent that outlived its deadline and settle it
/// `timed_out`.
pub(super) async fn time_out(
    ctx: &DaemonContext,
    id: &StageRunId,
) -> Result<FollowUp, ArchonError> {
    let stage = ctx.store.lock().await.stage_run(id)?;
    if stage.state != StageState::Running {
        return Ok(FollowUp::default());
    }
    let reason = "stage deadline passed";
    match (&stage.aether_agent, &stage.aether_task) {
        (Some(agent), Some(task)) => {
            cancel_and_settle(ctx, id, agent, task, reason, StageOutcome::TimedOut).await
        }
        (Some(agent), None) => resume::stop(ctx, id, agent, reason, StageOutcome::TimedOut).await,
        (None, _) => Ok(FollowUp::default()),
    }
}

/// Settle with `outcome` once Aether confirms the cancel. A refused cancel
/// usually means the task already settled; its own outcome is used then.
/// An agent or task Aether no longer knows has nothing left to stop, so it
/// settles with `outcome` too instead of holding its slot forever.
async fn cancel_and_settle(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    task: &AetherTaskId,
    reason: &str,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    let refusal = match ctx.backend.cancel(agent, reason).await {
        Ok(()) => return settle::settle(ctx, stage, outcome).await,
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%agent, %why, "cancel unanswered; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::warn!(%agent, %why, "Aether no longer knows the agent; settling its stage");
            return settle::settle(ctx, stage, outcome).await;
        }
        Err(refusal) => refusal,
    };
    match ctx.backend.task_outcome(task).await {
        Ok(Some(settled)) => settle::settle(ctx, stage, settled).await,
        Ok(None) => {
            tracing::warn!(%agent, %refusal, "cancel refused for an open task; the next sweep retries");
            Ok(FollowUp::default())
        }
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%agent, %why, "task lookup unanswered; the next sweep retries");
            Ok(FollowUp::default())
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::warn!(%task, %why, "Aether no longer knows the task; settling its stage");
            settle::settle(ctx, stage, outcome).await
        }
        Err(error) => Err(error),
    }
}

/// An agent that no stage run owns any more; one already gone is done.
pub(super) async fn cancel_stray(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    reason: &str,
) -> Result<FollowUp, ArchonError> {
    match ctx.backend.cancel(agent, reason).await {
        Ok(()) => Ok(FollowUp::default()),
        Err(ArchonError::NotFound(why)) => {
            tracing::info!(%agent, %why, "stray agent already gone");
            Ok(FollowUp::default())
        }
        Err(error) => Err(error),
    }
}
