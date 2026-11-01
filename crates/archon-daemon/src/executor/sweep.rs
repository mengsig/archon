//! Driving live stage runs from the store: re-issuing `spawning` ones,
//! stopping agents of finished runs, the deadline timer and warning, and the
//! stage watchdog. Everything
//! here is derived from persisted state, so it also repairs a crash.

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_queue::Store;

use super::{
    FollowUp, descendants, forewarn, held, load_spawn, resume, run_follow_up, run_node, settle,
    ship, siblings, stall, team,
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
    let (running, expired, nearing) = {
        let store = ctx.store.lock().await;
        let mut finished: BTreeMap<PipelineRunId, bool> = BTreeMap::new();
        let mut running: Vec<StageRun> = Vec::new();
        let (mut expired, mut nearing) = (Vec::new(), Vec::new());
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
            let deadline = stored_deadline(&store, &stage)?;
            if deadline.is_some_and(|d| d + grace < now) {
                expired.push(stage);
                continue;
            }
            if stage.state == StageState::Running {
                if deadline.is_some_and(|d| held::near(d, now)) {
                    nearing.push(stage.clone());
                }
                running.push(stage);
            }
        }
        follow.retire = run_node::runs_to_retire(&store)?;
        follow.sibling_stops = siblings::owed(&store)?;
        follow.steers = super::steer::owed(&store)?;
        follow.descendant_stops = descendants::owed(&store)?;
        follow.release_builds = crate::build_cache::owed_releases(&store)?
            .into_iter()
            .map(|(item, repo_name)| super::ItemBuild { item, repo_name })
            .collect();
        (running, expired, nearing)
    };
    follow.time_out = held::sweep(ctx, expired, &nearing, now).await?;
    forewarn::sweep(ctx, &running, now).await?;
    let stalled = settle_stalled(ctx, &running, now, &mut follow).await;
    run_follow_up(ctx, follow).await?;
    stalled
}

/// The deadline in `stage`'s stored request; `None` without one.
fn stored_deadline(store: &Store, stage: &StageRun) -> Result<Option<Timestamp>, ArchonError> {
    match load_spawn(store, &stage.id) {
        Ok(spawn) => Ok(spawn.request.deadline),
        Err(ArchonError::NotFound(what)) => {
            tracing::warn!(stage = %stage.id, "no {what}; no deadline timer");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Settle the stages of `running` the watchdog finds stalled, adding what
/// each owes to `follow`. One that fails leaves the others to settle; the
/// first error is kept, as a follow-up round keeps it.
async fn settle_stalled(
    ctx: &DaemonContext,
    running: &[StageRun],
    now: Timestamp,
    follow: &mut FollowUp,
) -> Result<(), ArchonError> {
    let mut first_error = None;
    let escalations = stall::owed_escalations(&*ctx.store.lock().await, running)?;
    let watched: Vec<StageRun> = running
        .iter()
        .filter(|s| !escalations.iter().any(|(id, _)| *id == s.id))
        .cloned()
        .collect();
    for (stage, reason) in escalations {
        match stop_marked(ctx, &stage, &reason).await {
            Ok(more) => follow.merge(more),
            Err(error) => {
                tracing::error!(stage = %stage, %error, "stopping an escalated stage failed");
                first_error.get_or_insert(error);
            }
        }
    }
    for (stage, reason) in stall::scan(ctx, &watched, now).await? {
        match stall(ctx, &stage, &reason).await {
            Ok(more) => follow.merge(more),
            Err(error) => {
                tracing::error!(stage = %stage, %error, "settling a stalled stage failed");
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
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

/// Fail a `running` stage whose agents the watchdog found stalled: an
/// infrastructure failure, which the stage's retry budget may re-spawn.
async fn stall(
    ctx: &DaemonContext,
    id: &StageRunId,
    reason: &str,
) -> Result<FollowUp, ArchonError> {
    tracing::warn!(stage = %id, reason, "stage watchdog fired");
    stop_marked(ctx, id, reason).await
}

/// Fail a `running` stage whose review failed too many rounds with the
/// `ESCALATE:` `reason`, which ends its run; a second call is a retry.
pub(super) async fn stop_review_loop(
    ctx: &DaemonContext,
    id: &StageRunId,
    reason: &str,
) -> Result<FollowUp, ArchonError> {
    assert!(
        StageOutcome::ends_run(reason),
        "a stopped review loop escalates"
    );
    tracing::warn!(stage = %id, reason, "review round cap reached; stopping the stage");
    stop_marked(ctx, id, reason).await
}

/// Cancel a `running` stage's agent and settle it failed with `reason`, or
/// with the escalation it is already marked with, which outlasts a stall.
/// Marked first so the cancel's own events cannot settle it as cancelled,
/// and under the run table, so a settle in flight sees the mark or the
/// stage is already settled.
async fn stop_marked(
    ctx: &DaemonContext,
    id: &StageRunId,
    reason: &str,
) -> Result<FollowUp, ArchonError> {
    assert!(!reason.trim().is_empty(), "a stop names its reason");
    let (stage, reason) = {
        let _table = ctx.runs.lock().await;
        let mut store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        if stage.state != StageState::Running {
            return Ok(FollowUp::default());
        }
        let reason = stall::prevailing(&store, id, reason)?;
        stall::mark_intent(&mut store, id, &reason)?;
        (stage, reason)
    };
    assert_eq!(stage.id, *id, "the stage asked for");
    let failed = || StageOutcome::Failed {
        error: reason.clone(),
        infra: stall::is_stall(&reason),
    };
    match (&stage.aether_agent, &stage.aether_task) {
        (Some(agent), Some(task)) => {
            cancel_and_settle(ctx, id, agent, task, &reason, failed()).await
        }
        (Some(agent), None) => resume::stop(ctx, id, agent, &reason, failed()).await,
        (None, _) => Ok(FollowUp::default()),
    }
}

/// Settle with `outcome` once Aether confirms the cancel, unless the agent
/// had already produced its own result: a task that completed or failed
/// before the cancel (its earlier settle failed, or its event was missed)
/// keeps that result rather than becoming the synthetic `outcome`. A task
/// that reads cancelled is our own cancel, so `outcome` stands.
/// A refused cancel usually means the task already settled; its own outcome
/// is used then. An agent or task Aether no longer knows has nothing left
/// to stop, so it settles with `outcome` instead of holding its slot forever.
async fn cancel_and_settle(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    task: &AetherTaskId,
    reason: &str,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    let refusal = match ctx.backend.cancel(agent, reason).await {
        Ok(()) => return settle_after_cancel(ctx, stage, agent, task, outcome).await,
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%agent, %why, "cancel unanswered; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::warn!(%agent, %why, "Aether no longer knows the agent; settling its stage");
            return settle_after_cancel(ctx, stage, agent, task, outcome).await;
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

/// Settle a stage whose agent is stopped or gone: with the result its task
/// holds if the agent produced one, else with the synthetic `outcome`.
/// A lookup that failed for a passing reason leaves the stage `running` for
/// the next sweep, which cancels again (harmless) and reads the task:
/// settling now could discard a verdict the agent did produce. A task Aether
/// no longer knows, or refuses to read, settles with `outcome`.
async fn settle_after_cancel(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    task: &AetherTaskId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(
        !matches!(outcome, StageOutcome::Completed { .. }),
        "a synthetic outcome is never the agent's own result"
    );
    let settled = match ctx.backend.task_outcome(task).await {
        Ok(found) => found,
        Err(error) if is_passing(&error) => {
            tracing::warn!(%agent, %error, "task lookup failed after cancel; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::warn!(%task, %why, "Aether no longer knows the task; settling its stage");
            None
        }
        Err(error) => {
            tracing::warn!(%task, %error, "Aether refused to read the task after its cancel; settling its stage");
            None
        }
    };
    let own_result = matches!(
        settled,
        Some(StageOutcome::Completed { .. } | StageOutcome::Failed { .. })
    );
    let chosen = match settled {
        Some(own) if own_result => {
            tracing::info!(%stage, %agent, "agent finished before its cancel; keeping its result");
            own
        }
        _ => outcome,
    };
    assert!(
        own_result || !matches!(chosen, StageOutcome::Completed { .. }),
        "only the agent's own result can complete a stage here"
    );
    settle::settle(ctx, stage, chosen).await
}

/// A lookup failure a later sweep may not repeat: no answer, local I/O, a
/// garbled reply or a failure inside Aether.
fn is_passing(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Unavailable(_) | ArchonError::Io { .. } | ArchonError::Protocol(_)
    ) || archon_aether::wire::is_internal(error)
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
