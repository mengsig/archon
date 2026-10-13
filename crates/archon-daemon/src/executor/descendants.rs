//! Stopping what a delegating stage agent left running (ARCHITECTURE §6.3):
//! its settled run owes a cancel of it and creates no agent until that is made.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::{FollowUp, finish, load_spawn, run_node};
use crate::context::DaemonContext;

/// `kv`: `descendant_stop:<stage run>` → the JSON [`OwedStop`].
const STOP_KEY_PREFIX: &str = "descendant_stop:";
const REASON: &str = "the stage agent that delegated to it has ended";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OwedStop {
    run: PipelineRunId,
    agent: AetherAgentId,
    /// When its stage settled; its turn may go on a little longer.
    settled_at: Timestamp,
}

fn stop_key(stage: &StageRunId) -> String {
    format!("{STOP_KEY_PREFIX}{stage}")
}

fn parse(key: &str, raw: &str) -> Result<OwedStop, ArchonError> {
    serde_json::from_str(raw).map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))
}

/// Owe the stop of what `settled`'s agent delegated, if it could delegate. Call
/// it before storing the settle: a crash leaves a stop too many, not too few.
pub(super) fn owe(store: &mut Store, settled: &StageRun) -> Result<bool, ArchonError> {
    assert!(
        settled.state.is_terminal(),
        "only a settled stage owes a stop"
    );
    let Some(agent) = &settled.aether_agent else {
        return Ok(false);
    };
    let leaf = match load_spawn(store, &settled.id) {
        Ok(spawn) => spawn.request.leaf,
        // Without its request, nothing says it could not delegate.
        Err(ArchonError::NotFound(_)) => false,
        Err(error) => return Err(error),
    };
    if leaf {
        return Ok(false);
    }
    let owed = OwedStop {
        run: settled.run.clone(),
        agent: agent.clone(),
        settled_at: settled.finished_at.expect("a settled stage has finished"),
    };
    store.kv_set(&stop_key(&settled.id), &serde_json::to_string(&owed)?)?;
    Ok(true)
}

/// Stage runs whose descendants are still to be stopped.
pub(super) fn owed(store: &Store) -> Result<Vec<StageRunId>, ArchonError> {
    let mut stages = Vec::new();
    for (key, _) in store.kv_prefix(STOP_KEY_PREFIX)? {
        let raw = key.strip_prefix(STOP_KEY_PREFIX).expect("ranged by prefix");
        let stage = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Store(format!("descendant stop key {key}: {e}")))?;
        stages.push(stage);
    }
    Ok(stages)
}

/// Whether `run` still owes a stop, so that none of its agents is created.
pub(super) fn holds(store: &Store, run: &PipelineRunId) -> Result<bool, ArchonError> {
    for (key, raw) in store.kv_prefix(STOP_KEY_PREFIX)? {
        if parse(&key, &raw)?.run == *run {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Cancel what the agent of `stage` left running, once its own turn has ended.
/// Done when Aether confirms or forgot the agent; anything else leaves it owed.
pub(super) async fn stop(ctx: &DaemonContext, stage: &StageRunId) -> Result<FollowUp, ArchonError> {
    let key = stop_key(stage);
    let Some(raw) = ctx.store.lock().await.kv_get(&key)? else {
        return Ok(FollowUp::default());
    };
    let owed = parse(&key, &raw)?;
    if turn_still_ending(ctx, &owed, archon_core::now()).await? {
        return Ok(FollowUp::default());
    }
    match ctx.backend.cancel(&owed.agent, REASON).await {
        Ok(()) => {}
        Err(ArchonError::NotFound(why)) => {
            tracing::info!(agent = %owed.agent, %why, "settled stage agent already gone");
        }
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => {
            tracing::info!(agent = %owed.agent, %error, "descendant stop failed; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(refusal) => return refused(ctx, stage, &owed, refusal).await,
    }
    let mut store = ctx.store.lock().await;
    store.kv_delete(&key)?;
    let waiting = store.stage_runs(&owed.run)?.into_iter();
    Ok(FollowUp {
        issue: waiting
            .filter(|s| s.state == StageState::Spawning)
            .map(|s| s.id)
            .collect(),
        poke: true,
        ..FollowUp::default()
    })
}

/// Whether the agent is still ending the turn its task settled in, which a
/// cancel would cut short (§4.5). Unknown counts as still ending.
async fn turn_still_ending(
    ctx: &DaemonContext,
    owed: &OwedStop,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let grace = chrono::Duration::seconds(run_node::TURN_END_GRACE_SECS);
    if owed.settled_at + grace < now {
        return Ok(false);
    }
    match ctx.backend.is_live(&owed.agent).await {
        Ok(live) => Ok(live),
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(agent = %owed.agent, %why, "stage agent state unknown; its descendants are stopped later");
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Aether refused the cancel outright. The run does not go on beside agents
/// nobody stopped: it fails, and its node's retirement cancels the subtree.
async fn refused(
    ctx: &DaemonContext,
    stage: &StageRunId,
    owed: &OwedStop,
    refusal: ArchonError,
) -> Result<FollowUp, ArchonError> {
    tracing::error!(agent = %owed.agent, %refusal, "Aether refused to stop a settled stage agent's descendants");
    let mut table = ctx.runs.lock().await;
    let (run, stage_id) = {
        let mut store = ctx.store.lock().await;
        let stage_id = store.stage_run(stage)?.stage_id;
        store.kv_delete(&stop_key(stage))?;
        (store.run(&owed.run)?, stage_id)
    };
    assert_eq!(run.id, owed.run, "the store returns the run asked for");
    let error = format!(
        "Archon could not stop the agents stage `{stage_id}` delegated to, so the run does not \
         go on beside them: {refusal}"
    );
    finish::fail_run(ctx, &mut table, run, stage_id, error, true).await
}
