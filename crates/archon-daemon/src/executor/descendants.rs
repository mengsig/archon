//! Stopping what a delegating stage agent left running (ARCHITECTURE §6.3): its
//! settled run creates no agent until Aether holds nothing live beneath it.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::notify::NotificationKind;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::{FollowUp, finish, load_spawn, run_node};
use crate::context::DaemonContext;
use crate::feed;

/// `kv`: `descendant_stop:<stage run>` → the JSON [`OwedStop`].
const STOP_KEY_PREFIX: &str = "descendant_stop:";
const REASON: &str = "the stage agent that delegated to it has ended";
/// How long a cancelled subtree may take to end before the operator is told
/// what the run waits for. Ending is a few seconds unless a tool call hangs.
const SLOW_STOP_SECS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OwedStop {
    run: PipelineRunId,
    agent: AetherAgentId,
    /// When its stage settled; its turn may go on a little longer.
    settled_at: Timestamp,
    /// When Aether accepted the cancel; one is enough, as Aether keeps it
    /// pending until the whole subtree has ended.
    #[serde(default)]
    cancelled_at: Option<Timestamp>,
    /// The operator was told that the subtree is slow to end.
    #[serde(default)]
    reported: bool,
    /// The agent is kept for its stage's next attempt (§6.7): what it
    /// delegated is stopped from its children down, not through it.
    #[serde(default)]
    keep: bool,
}

/// What asking Aether for the cancel came to.
enum Requested {
    Accepted(OwedStop),
    /// The agent's turn is still ending, or Aether did not answer.
    Later,
    Refused(ArchonError),
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
    let (leaf, kept) = match load_spawn(store, &settled.id) {
        Ok(spawn) => (spawn.request.leaf, spawn.request.keep),
        // Without its request, nothing says it could not delegate.
        Err(ArchonError::NotFound(_)) => (false, false),
        Err(error) => return Err(error),
    };
    if leaf {
        return Ok(false);
    }
    let owed = OwedStop {
        run: settled.run.clone(),
        agent: agent.clone(),
        settled_at: settled.finished_at.expect("a settled stage has finished"),
        cancelled_at: None,
        reported: false,
        keep: kept && settled.state == StageState::Completed,
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

/// Cancel what the agent of `stage` left running and wait until Aether holds nothing
/// live beneath it. Done only then, or when Aether forgot the agent; else still owed.
pub(super) async fn stop(ctx: &DaemonContext, stage: &StageRunId) -> Result<FollowUp, ArchonError> {
    let key = stop_key(stage);
    let Some(raw) = ctx.store.lock().await.kv_get(&key)? else {
        return Ok(FollowUp::default());
    };
    let now = archon_core::now();
    let owed = match parse(&key, &raw)? {
        cancelled @ OwedStop {
            cancelled_at: Some(_),
            ..
        } => cancelled,
        fresh => match request_cancel(ctx, &key, fresh.clone(), now).await? {
            Requested::Accepted(cancelled) => cancelled,
            Requested::Later => return Ok(FollowUp::default()),
            Requested::Refused(refusal) => return refused(ctx, stage, &fresh, refusal).await,
        },
    };
    let Some(beneath) = beneath(ctx, &owed.agent).await? else {
        return Ok(FollowUp::default());
    };
    let live: Vec<AetherAgentId> = beneath
        .iter()
        .filter(|descendant| descendant.state == DescendantState::Live)
        .map(|descendant| descendant.agent.clone())
        .collect();
    if !live.is_empty() {
        return still_owed(ctx, &key, stage, owed, &live, now).await;
    }
    stopped(ctx, &key, stage, &owed, &beneath).await
}

/// Ask Aether to cancel the agent's subtree, and record that it accepted.
async fn request_cancel(
    ctx: &DaemonContext,
    key: &str,
    mut owed: OwedStop,
    now: Timestamp,
) -> Result<Requested, ArchonError> {
    assert!(
        owed.cancelled_at.is_none(),
        "an accepted cancel is not repeated"
    );
    // A kept agent's turn never ends it, and it is not the one cancelled.
    if !owed.keep && turn_still_ending(ctx, &owed, now).await? {
        return Ok(Requested::Later);
    }
    let cancelled = match owed.keep {
        true => cancel_children(ctx, &owed.agent).await,
        false => ctx.backend.cancel(&owed.agent, REASON).await,
    };
    match cancelled {
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
            return Ok(Requested::Later);
        }
        Err(refusal) => return Ok(Requested::Refused(refusal)),
    }
    owed.cancelled_at = Some(now);
    let mut store = ctx.store.lock().await;
    // A concurrent pass may have finished the stop: it is not owed again.
    if store.kv_get(key)?.is_some() {
        store.kv_set(key, &serde_json::to_string(&owed)?)?;
    }
    Ok(Requested::Accepted(owed))
}

/// Cancel what `agent` delegated and leave it alive: each of its live
/// children, which takes that child's own subtree with it.
async fn cancel_children(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    let beneath = ctx.backend.descendants(agent).await?;
    let children = beneath
        .iter()
        .filter(|d| d.parent == *agent && d.state == DescendantState::Live);
    for child in children {
        assert_ne!(child.agent, *agent, "an agent is not its own child");
        match ctx.backend.cancel(&child.agent, REASON).await {
            Ok(()) | Err(ArchonError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Every agent beneath `agent` as Aether holds it; `None` when Aether did
/// not answer, which confirms nothing.
async fn beneath(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<Vec<Descendant>>, ArchonError> {
    let found = match ctx.backend.descendants(agent).await {
        Ok(found) => found,
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => {
            tracing::info!(%agent, %error, "cannot confirm a descendant stop; the next sweep asks again");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    assert!(
        found.iter().all(|descendant| descendant.agent != *agent),
        "an agent is not its own descendant"
    );
    Ok(Some(found))
}

/// Agents the ended one delegated to are still ending, so its run creates
/// none. Past `SLOW_STOP_SECS` the operator is told once what it waits for.
async fn still_owed(
    ctx: &DaemonContext,
    key: &str,
    stage: &StageRunId,
    mut owed: OwedStop,
    live: &[AetherAgentId],
    now: Timestamp,
) -> Result<FollowUp, ArchonError> {
    assert!(!live.is_empty(), "nothing live is a stop made");
    let since = owed
        .cancelled_at
        .expect("only a cancelled subtree is waited for");
    tracing::debug!(agent = %owed.agent, live = live.len(), "delegated agents are still ending");
    if owed.reported || since + chrono::Duration::seconds(SLOW_STOP_SECS) >= now {
        return Ok(FollowUp::default());
    }
    owed.reported = true;
    let (item, stage_id) = {
        let mut store = ctx.store.lock().await;
        if store.kv_get(key)?.is_none() {
            return Ok(FollowUp::default());
        }
        store.kv_set(key, &serde_json::to_string(&owed)?)?;
        (store.run(&owed.run)?.item, store.stage_run(stage)?.stage_id)
    };
    let names: Vec<&str> = live.iter().map(AetherAgentId::as_str).collect();
    let text = format!(
        "Stage `{stage_id}` ended {} s ago, but {} it delegated to still run in Aether after their cancel ({}). The item waits for them to end; `archon aether agents` shows them.",
        (now - since).num_seconds(),
        agents(live.len()),
        names.join(", ")
    );
    feed::notify(
        ctx,
        NotificationKind::Blocker,
        text,
        Some(item),
        Some(owed.run),
    )
    .await?;
    Ok(FollowUp::default())
}

fn agents(count: usize) -> String {
    match count {
        1 => "1 agent".to_owned(),
        n => format!("{n} agents"),
    }
}

/// Aether holds nothing live beneath the agent: the run's spawns and its delivery go on. What
/// it delegated to is recorded as Aether holds it, for the stage's cost.
async fn stopped(
    ctx: &DaemonContext,
    key: &str,
    stage: &StageRunId,
    owed: &OwedStop,
    beneath: &[Descendant],
) -> Result<FollowUp, ArchonError> {
    assert!(owed.cancelled_at.is_some(), "a stop is made by a cancel");
    assert!(
        beneath.iter().all(|d| d.state != DescendantState::Live),
        "a stop is done once nothing beneath the agent runs"
    );
    let (waiting, stage_id) = {
        let mut store = ctx.store.lock().await;
        for ended in beneath {
            store.record_delegated(&ended.agent, &ended.parent, &ended.model, ended.effort)?;
        }
        store.kv_delete(key)?;
        (
            store.stage_runs(&owed.run)?,
            store.stage_run(stage)?.stage_id,
        )
    };
    if owed.reported {
        let text =
            format!("The agents stage `{stage_id}` delegated to have ended; its item goes on.");
        feed::system(ctx, text).await?;
    }
    Ok(FollowUp {
        issue: waiting
            .into_iter()
            .filter(|s| s.state == StageState::Spawning)
            .map(|s| s.id)
            .collect(),
        // A delivery archond makes itself waited for the stop as well.
        ship: vec![owed.run.clone()],
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
