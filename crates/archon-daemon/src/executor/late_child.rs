//! A child a stage's lead delegates is given its lead's deadline by Aether
//! as the lead's row stands when the child is created. A stage deadline
//! moved by a steer or a hold reaches the agents already live beneath the
//! lead; one created from the lead's row before the move landed would keep
//! the old deadline and stop early, as would any agent it delegated before
//! the event came. So when a running stage's lead gets a new child, every
//! live agent beneath the lead is raised to the stage's stored deadline when
//! Aether holds it lower.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::{AgentUpdate, Descendant, DescendantState};
use archon_core::run::StageState;

use super::{resume, spawn};
use crate::context::DaemonContext;

/// Raise `child`, just delegated by `parent`, and every other live agent
/// beneath `parent`, to the deadline stored for the running stage `parent`
/// leads, where Aether holds it lower. `Err` only while Aether does not
/// answer or the store fails, so the event is handled again.
pub(crate) async fn lift_new_child(
    ctx: &DaemonContext,
    child: &AetherAgentId,
    parent: &AetherAgentId,
) -> Result<(), ArchonError> {
    assert_ne!(child, parent, "an agent is not its own parent");
    let stage = ctx.store.lock().await.stage_run_by_agent(parent)?;
    let Some(stage) = stage.filter(|s| s.state == StageState::Running) else {
        return Ok(());
    };
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(parent),
        "the stage's lead"
    );
    // Under the lock a steer and a hold move the stage's deadline with.
    let _steering = ctx.coordination.steering.hold(&stage.id).await;
    let stored = spawn::load_spawn(&*ctx.store.lock().await, &stage.id)?
        .request
        .deadline;
    let Some(stored) = stored.filter(|at| *at > archon_core::now()) else {
        return Ok(());
    };
    let beneath = ctx.backend.descendants(parent).await?;
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
        assert!(agent.agent != *parent, "the lead is not beneath itself");
        raise(ctx, &stage.id, &agent.agent, stored).await?;
    }
    Ok(())
}

/// Move the live `agent` of `stage` up to `stored`. `Err` only while Aether
/// does not answer or the store fails, so the event is handled again.
async fn raise(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    stored: Timestamp,
) -> Result<(), ArchonError> {
    // Every Archon deadline move stores first under the steering lock, and an
    // agent cannot move another's deadline, so nothing raises it meanwhile.
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
