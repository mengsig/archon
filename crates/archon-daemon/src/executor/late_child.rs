//! A child a stage's lead delegates is given its lead's deadline by Aether
//! as the lead's row stands when the child is created. A stage deadline
//! moved by a steer or a hold reaches the agents already live beneath the
//! lead; one created from the lead's row before the move landed would keep
//! the old deadline and stop early. So each new child of a running stage's
//! lead is raised to the stage's stored deadline when Aether holds it lower.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{AgentUpdate, DescendantState};
use archon_core::run::StageState;

use super::{resume, spawn};
use crate::context::DaemonContext;

/// Raise `child`, just delegated by `parent`, to the deadline stored for the
/// running stage `parent` leads, when Aether holds the child's lower. `Err`
/// only while Aether does not answer, so the event is handled again.
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
    let held = beneath.iter().find(|d| d.agent == *child);
    let Some(held) = held.filter(|d| d.state == DescendantState::Live) else {
        return Ok(());
    };
    // A child with no deadline of its own, or a later one, keeps it.
    if held.deadline.is_none_or(|own| own >= stored) {
        return Ok(());
    }
    let update = AgentUpdate {
        deadline: Some(stored),
        ..AgentUpdate::default()
    };
    match ctx.backend.update_agent(child, update).await {
        Ok(()) => {
            tracing::info!(stage = %stage.id, %child, deadline = %stored, "new child raised to its stage's deadline");
            Ok(())
        }
        Err(error) if resume::unanswered(&error) || error.is_store() => Err(error),
        Err(refusal) => {
            tracing::warn!(%child, %refusal, "new child's deadline not raised; it has most likely ended");
            Ok(())
        }
    }
}
