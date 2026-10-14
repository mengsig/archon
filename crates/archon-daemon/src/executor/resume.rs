//! One agent through a fix loop (ARCHITECTURE §6.7). The agent of a work
//! stage that a later stage can send back is created long-lived: it outlives
//! its task, and the next attempt of its slot hands it the findings instead
//! of starting a cold agent. An attempt resumed this way has the agent and
//! no task of its own; it settles on what the agent says when its turn ends.

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::ports::{AgentUpdate, SpawnRequest};
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_pipeline::SpawnPlan;
use archon_queue::Store;

use super::spawn::{self, KeptAgent};
use super::{FollowUp, load_spawn, run_node, settle, steer, sweep};
use crate::context::DaemonContext;

/// How long a kept agent may wait for findings before Aether ends it. Its
/// run ends it sooner: it is a child of the run's node.
const PARKED_DAYS: i64 = 7;
const REPLACED: &str = "a fresh agent takes over its stage";
const ENDED: &str = "its stage attempt ended";

/// What becomes of a kept agent once an attempt of its settled.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kept {
    /// The attempt completed: it waits for findings, or for its run to end.
    Park(AetherAgentId),
    /// The attempt ended any other way, so no later one resumes it.
    End(AetherAgentId),
}

/// Whether Aether gave no answer, so the same call can be repeated.
pub(super) fn unanswered(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Unavailable(_) | ArchonError::Protocol(_) | ArchonError::Io { .. }
    )
}

/// How the next stage run of a slot gets its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Takeover {
    /// The slot holds no kept agent: a first attempt, or one that ended.
    Fresh,
    /// The slot's kept agent goes on, told what to fix.
    Resume(KeptAgent),
    /// The slot's kept agent cannot go on: it is stopped for a fresh one.
    Replace(AetherAgentId),
}

/// What the stage run reserved for `plan` does about the agent kept in its
/// slot. `lost` is an infrastructure replacement of the slot's live attempt.
pub(super) fn takeover(
    store: &Store,
    run: &PipelineRunId,
    plan: &SpawnPlan,
    lost: bool,
) -> Result<Takeover, ArchonError> {
    assert!(plan.attempt >= 1, "attempts are 1-based");
    assert!(
        plan.resume.is_none() || (plan.keep && plan.attempt > 1),
        "only the kept agent of an earlier attempt is resumed"
    );
    let before = if lost { plan.attempt } else { plan.attempt - 1 };
    let runs = store.stage_runs(run)?;
    let last = runs
        .iter()
        .filter(|s| s.stage_id == plan.stage_id && s.fan_index == plan.fan_index)
        .filter(|s| s.attempt == before)
        .max_by_key(|s| s.started_at);
    let Some(last) = last else {
        return Ok(Takeover::Fresh);
    };
    // An attempt that ended any other way had its agent stopped as it settled.
    let waits = lost || last.state == StageState::Completed;
    let Some(agent) = last.aether_agent.clone().filter(|_| waits) else {
        return Ok(Takeover::Fresh);
    };
    if !is_kept(store, &last.id)? {
        return Ok(Takeover::Fresh);
    }
    // A steer that changed the stage's model wants the new one from the start.
    let same_model = last.choice == plan.choice;
    let (Some(message), Some(parked_at)) = (&plan.resume, last.finished_at) else {
        return Ok(Takeover::Replace(agent));
    };
    if lost || !same_model {
        return Ok(Takeover::Replace(agent));
    }
    // Notes steered in while it was parked reached the reviewers, not it.
    let item = store.run(run)?.item;
    let mut message = message.clone();
    for note in steer::notes_since(store, &item, parked_at)? {
        message.push_str(&format!("\n\n{}", steer::note_message(&note)));
    }
    Ok(Takeover::Resume(KeptAgent { agent, message }))
}

/// Record that the agent of resumed attempt `stage` was told `text` after
/// its findings: the attempt settles on what it says once it took that.
pub(super) fn told(store: &mut Store, stage: &StageRunId, text: &str) -> Result<(), ArchonError> {
    assert!(!text.trim().is_empty(), "an agent is told something");
    let mut stored = load_spawn(store, stage)?;
    let Some(kept) = &mut stored.resume else {
        return Ok(());
    };
    kept.message = text.to_owned();
    spawn::save_spawn(store, stage, &stored)
}

/// Give the kept agent its stage deadline, its new grant and the findings,
/// keyed by the stage run. `false` when it cannot go on: a fresh agent takes
/// the attempt. `request` is the attempt's own, never sent as a spawn.
pub(super) async fn hand_over(
    ctx: &DaemonContext,
    stage: &StageRun,
    kept: &KeptAgent,
    request: &SpawnRequest,
) -> Result<bool, ArchonError> {
    assert_eq!(stage.state, StageState::Spawning, "a reserved attempt");
    assert!(!kept.message.trim().is_empty(), "resumed with findings");
    let agent = &kept.agent;
    if !ctx.backend.is_live(agent).await? {
        tracing::info!(stage = %stage.id, %agent, "kept agent is gone; spawning a fresh one");
        return Ok(false);
    }
    let cost_budget_usd = {
        let store = ctx.store.lock().await;
        regranted_cap(&store, stage, agent, request.cost_budget_usd)?
    };
    let update = AgentUpdate {
        deadline: request.deadline,
        cost_budget_usd,
        ..AgentUpdate::default()
    };
    let key = stage.id.as_str();
    let handed = match ctx.backend.update_agent(agent, update).await {
        Ok(()) => ctx.backend.send_message(agent, &kept.message, key).await,
        Err(error) => Err(error),
    };
    let refusal = match handed {
        Ok(()) => return Ok(true),
        Err(error) if unanswered(&error) => return Err(error),
        Err(refusal) => refusal,
    };
    tracing::info!(stage = %stage.id, %agent, %refusal, "kept agent cannot be resumed; spawning a fresh one");
    replace(ctx, agent).await?;
    Ok(false)
}

/// Aether's new cap for `agent`, resumed on `stage` with `grant`. Aether's cap
/// counts the agent's subtree over its whole life, so it is what the earlier
/// attempts on the agent spent plus the grant. `None` when it has no cap.
fn regranted_cap(
    store: &Store,
    stage: &StageRun,
    agent: &AetherAgentId,
    grant: Option<f64>,
) -> Result<Option<f64>, ArchonError> {
    let Some(grant) = grant else {
        return Ok(None);
    };
    assert!(grant.is_finite(), "a grant is an amount, not {grant}");
    let runs = store.stage_runs(&stage.run)?;
    let earlier: Vec<&StageRun> = runs
        .iter()
        .filter(|s| s.id != stage.id && s.aether_agent.as_ref() == Some(agent))
        .collect();
    let Some(created) = earlier.iter().min_by_key(|s| s.attempt) else {
        return Ok(None);
    };
    // The cap its create sent, as `stage_create_agent` decides it.
    let spawned = load_spawn(store, &created.id)?.request;
    if spawned.cost_budget_usd.is_none() || spawned.aether_cost_budget == Some(false) {
        return Ok(None);
    }
    let spent: f64 = earlier.iter().map(|s| s.cost_usd).sum();
    assert!(spent.is_finite() && spent >= 0.0, "spend is an amount");
    Ok(Some((spent + grant).max(steer::MIN_GRANT_USD)))
}

/// Stop the kept agent a fresh one takes over from, so the fresh one has
/// the worktree to itself. Safe to repeat: one already ended is done.
pub(super) async fn replace(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    end(ctx, agent, REPLACED).await
}

/// Record the kept agent on the attempt it was resumed for. Aether opened
/// no task for it, so the attempt holds the agent alone.
pub(super) async fn resumed_ok(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: AetherAgentId,
) -> Result<FollowUp, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut current = store.stage_run(&stage.id)?;
    if current.state != StageState::Spawning {
        tracing::info!(stage = %stage.id, %agent, "resumed an agent for a settled stage run; stopping it");
        return Ok(FollowUp {
            kept: vec![Kept::End(agent)],
            ..FollowUp::default()
        });
    }
    assert!(current.aether_task.is_none(), "a resume opens no task");
    current.aether_agent = Some(agent);
    current.state = StageState::Running;
    store.upsert_stage_run(&current)?;
    let run = store.run(&stage.run)?;
    if run.state.is_terminal() {
        return Ok(FollowUp {
            stop: vec![run.id],
            ..FollowUp::default()
        });
    }
    Ok(FollowUp {
        resumed: vec![current.id],
        roster: vec![run.id],
        ..FollowUp::default()
    })
}

/// The kept agent a running attempt was resumed on and what it was told;
/// `None` for a stage run that waits for a task.
fn resumed_on(
    store: &Store,
    id: &StageRunId,
) -> Result<Option<(AetherAgentId, String)>, ArchonError> {
    let stage = store.stage_run(id)?;
    if stage.state != StageState::Running || stage.aether_task.is_some() {
        return Ok(None);
    }
    let Some(agent) = stage.aether_agent else {
        return Ok(None);
    };
    let kept = match load_spawn(store, id) {
        Ok(spawn) => spawn.resume,
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    assert!(
        kept.as_ref().is_none_or(|kept| kept.agent == agent),
        "an attempt runs on the agent it was resumed on"
    );
    Ok(kept.map(|kept| (agent, kept.message)))
}

/// Settle a resumed attempt once its agent answered; nothing while it works.
pub(super) async fn check(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
    let Some((agent, message)) = resumed_on(&*ctx.store.lock().await, id)? else {
        return Ok(FollowUp::default());
    };
    let outcome = match ctx.backend.resumed_outcome(&agent, &message).await {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return Ok(FollowUp::default()),
        Err(error) if unanswered(&error) => {
            tracing::info!(stage = %id, %agent, %error, "resumed agent not read; the next sweep asks again");
            return Ok(FollowUp::default());
        }
        Err(error) => return Err(error),
    };
    // An infrastructure failure is replaced by a fresh agent, which stops
    // this one before it starts ([`Takeover::Replace`]).
    settle::settle(ctx, id, outcome).await
}

/// Stop a resumed attempt's agent and settle the attempt with `outcome`.
/// An agent Aether no longer holds live has nothing left to stop.
pub(super) async fn stop(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    reason: &str,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(
        !matches!(outcome, StageOutcome::Completed { .. }),
        "a stopped attempt did not complete"
    );
    let refusal = match ctx.backend.cancel(agent, reason).await {
        Ok(()) | Err(ArchonError::NotFound(_)) => return settle::settle(ctx, stage, outcome).await,
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%agent, %why, "cancel unanswered; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(refusal) => refusal,
    };
    if ctx.backend.is_live(agent).await? {
        tracing::warn!(%agent, %refusal, "cancel refused for a live agent; the next sweep retries");
        return Ok(FollowUp::default());
    }
    settle::settle(ctx, stage, outcome).await
}

/// Whether the stage run's agent was created to outlive its task. A request
/// already pruned says no, which only costs its run a turn-end wait.
fn is_kept(store: &Store, stage: &StageRunId) -> Result<bool, ArchonError> {
    match load_spawn(store, stage) {
        Ok(spawn) => Ok(spawn.request.keep),
        Err(ArchonError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether the request of settled `stage` is still read: its agent is kept,
/// and the retirement of its run's node asks which agents are.
pub(super) fn request_in_use(store: &Store, stage: &StageRun) -> Result<bool, ArchonError> {
    assert!(
        stage.state.is_terminal(),
        "a live stage run always has its request"
    );
    Ok(is_kept(store, &stage.id)? && run_node::has_node(store, &stage.run)?)
}

/// The kept agents among the settled `stages`: live in Aether with no turn
/// to end, until their run's node is cancelled.
pub(super) fn parked(
    store: &Store,
    stages: &[StageRun],
) -> Result<BTreeSet<AetherAgentId>, ArchonError> {
    let mut parked = BTreeSet::new();
    for stage in stages.iter().filter(|s| s.state.is_terminal()) {
        let Some(agent) = &stage.aether_agent else {
            continue;
        };
        if is_kept(store, &stage.id)? {
            parked.insert(agent.clone());
        }
    }
    assert!(
        parked.len() <= stages.len(),
        "one agent per stage run at most"
    );
    Ok(parked)
}

/// What becomes of the agent of `settled`: `None` for one that ends with its
/// task, as every agent not kept does.
pub(super) fn after_settle(store: &Store, settled: &StageRun) -> Result<Option<Kept>, ArchonError> {
    assert!(settled.state.is_terminal(), "only a settled attempt");
    let Some(agent) = settled.aether_agent.clone() else {
        return Ok(None);
    };
    if !is_kept(store, &settled.id)? {
        return Ok(None);
    }
    Ok(Some(match settled.state {
        StageState::Completed => Kept::Park(agent),
        _ => Kept::End(agent),
    }))
}

/// Park or end a kept agent, as [`after_settle`] found.
pub(super) async fn settle_kept(ctx: &DaemonContext, kept: Kept) -> Result<FollowUp, ArchonError> {
    match kept {
        Kept::Park(agent) => park(ctx, &agent).await,
        Kept::End(agent) => end(ctx, &agent, ENDED).await,
    }?;
    Ok(FollowUp::default())
}

/// Move the agent's deadline past the reviews it now waits for. One that
/// cannot be parked ends at its stage deadline and is replaced: slower only.
async fn park(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    let until = archon_core::now() + chrono::Duration::days(PARKED_DAYS);
    let update = AgentUpdate {
        deadline: Some(until),
        ..AgentUpdate::default()
    };
    if let Err(error) = ctx.backend.update_agent(agent, update).await {
        tracing::warn!(%agent, %error, "kept agent not parked; its next attempt may start fresh");
    }
    Ok(())
}

/// Stop a kept agent no attempt will resume; one already ended is done.
async fn end(ctx: &DaemonContext, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError> {
    assert!(!reason.trim().is_empty(), "a stop says why");
    if !ctx.backend.is_live(agent).await? {
        return Ok(());
    }
    let refusal = match sweep::cancel_stray(ctx, agent, reason).await {
        Ok(_) => return Ok(()),
        Err(error) if unanswered(&error) => return Err(error),
        Err(refusal) => refusal,
    };
    // Aether refuses to cancel an agent that ended in the meantime.
    if ctx.backend.is_live(agent).await? {
        return Err(refusal);
    }
    Ok(())
}
