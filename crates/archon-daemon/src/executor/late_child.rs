//! A child delegated beneath a stage's lead is given its parent's deadline by
//! Aether as the parent's row stands when the child is created. A stage
//! deadline moved by a steer or a hold reaches the agents already live
//! beneath the lead; one created from a row before the move landed would keep
//! the old deadline and stop early, as would any agent it delegated before
//! the event came. So when any agent of a running stage's tree gets a new
//! child, every live agent beneath the lead is raised to the stage's stored
//! deadline when Aether holds it lower.
//!
//! Every Archon deadline move of a stage stores it and sends it under the
//! stage's steering lock, and the stored deadline only rises. The read of
//! the stored deadline and Aether's, and each raise, run under that lock too,
//! so together they are one compare-and-raise: no extension lands between.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::{AgentUpdate, Descendant, DescendantState};
use archon_core::run::{StageRun, StageState};

use super::{resume, spawn};
use crate::context::DaemonContext;

/// Raise `child`, just delegated by `parent`, and every other live agent
/// beneath the lead of the running stage `parent` belongs to, to that
/// stage's stored deadline where Aether holds it lower. `Err` only while
/// Aether does not answer or the store fails, so the event is handled again.
pub(crate) async fn lift_new_child(
    ctx: &DaemonContext,
    child: &AetherAgentId,
    parent: &AetherAgentId,
) -> Result<(), ArchonError> {
    assert_ne!(child, parent, "an agent is not its own parent");
    let Some(stage) = running_stage_of(ctx, parent).await? else {
        return Ok(());
    };
    let lead = stage.aether_agent.as_ref().expect("a running stage's lead");
    assert_ne!(lead, child, "a new child is not its stage's lead");
    let _steering = ctx.coordination.steering.hold(&stage.id).await;
    let stored = spawn::load_spawn(&*ctx.store.lock().await, &stage.id)?
        .request
        .deadline;
    let Some(stored) = stored.filter(|at| *at > archon_core::now()) else {
        return Ok(());
    };
    let beneath = ctx.backend.descendants(lead).await?;
    let is_live = |d: &&Descendant| d.state == DescendantState::Live;
    if !beneath.iter().filter(is_live).any(|d| d.agent == *child) {
        return Ok(());
    }
    // The child may have delegated before this event came: every live agent
    // of the stage below its deadline is raised. One with none keeps none.
    let lagging: Vec<&Descendant> = (beneath.iter().filter(is_live))
        .filter(|d| d.deadline.is_some_and(|own| own < stored))
        .collect();
    for agent in lagging {
        assert!(agent.agent != *lead, "the lead is not beneath itself");
        raise(ctx, &stage.id, &agent.agent, stored).await?;
    }
    Ok(())
}

/// The running stage whose lead is `agent` or delegated, at some depth, to
/// it, by the delegations Archon recorded; `None` for any other agent.
async fn running_stage_of(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<StageRun>, ArchonError> {
    let store = ctx.store.lock().await;
    let mut at = agent.clone();
    // One hop per delegation level, and one more for a corrupted cycle.
    for _ in 0..=ctx.config.limits.max_delegation_depth {
        if let Some(stage) = store.stage_run_by_agent(&at)? {
            assert_eq!(stage.aether_agent.as_ref(), Some(&at), "the stage's lead");
            return Ok(Some(stage).filter(|s| s.state == StageState::Running));
        }
        let Some(up) = store.delegated_parent(&at)? else {
            return Ok(None);
        };
        at = up;
    }
    tracing::warn!(%agent, "no stage lead found within the delegation depth");
    Ok(None)
}

/// Move the live `agent` of `stage` up to `stored`. `Err` only while Aether
/// does not answer or the store fails, so the event is handled again.
async fn raise(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    stored: Timestamp,
) -> Result<(), ArchonError> {
    // Under the steering lock, and an agent cannot move another's deadline,
    // so nothing raises it between the read and this update.
    let update = AgentUpdate {
        deadline: Some(stored),
        ..AgentUpdate::default()
    };
    match ctx.backend.update_agent(agent, update).await {
        Ok(()) => {
            tracing::info!(%stage, %agent, deadline = %stored, "late child raised to its stage's deadline");
            Ok(())
        }
        Err(error) if resume::unanswered(&error) || error.is_store() => Err(error),
        Err(refusal) => {
            tracing::warn!(%agent, %refusal, "late child's deadline not raised; it has most likely ended");
            Ok(())
        }
    }
}
