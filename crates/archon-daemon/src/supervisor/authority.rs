//! Who may change Archon over MCP (ARCHITECTURE §11, §18.4): delegated agents
//! inherit the `archon` tools, so only the orchestrator of now may.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use tokio::sync::RwLockReadGuard;

use super::callers;
use crate::context::DaemonContext;
use crate::pump::FleetRole;

/// Let `caller` change Archon only if the store says it is the orchestrator of
/// now. The guard returned keeps the orchestrator from being replaced mid-call.
pub(crate) async fn authorize<'a>(
    ctx: &'a DaemonContext,
    caller: Option<&AetherAgentId>,
) -> Result<RwLockReadGuard<'a, ()>, ArchonError> {
    let held = ctx.supervisor.authority.read().await;
    let orchestrator = super::current_orchestrator(ctx).await?;
    let Some(caller) = caller else {
        return match callers::unnamed_is_from(ctx, orchestrator.as_ref()).await? {
            true => Ok(held),
            false => Err(unnamed_refusal(ctx)),
        };
    };
    if orchestrator.as_ref() == Some(caller) {
        callers::named(ctx, caller).await?;
        return Ok(held);
    }
    drop(held);
    let who = describe(ctx, caller).await?;
    assert!(!who.is_empty(), "a refusal says who was refused");
    Err(ArchonError::Forbidden(format!(
        "This call changes Archon, which only the orchestrator may do, and {who} Nothing was \
         changed."
    )))
}

/// A call that needs no authority, such as a read. Named and from the
/// orchestrator, it shows as well as any other that Aether names callers.
pub(crate) async fn saw_call(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
) -> Result<(), ArchonError> {
    let Some(caller) = caller else {
        return Ok(());
    };
    if callers::are_named(ctx).await? {
        return Ok(());
    }
    let orchestrator = super::current_orchestrator(ctx).await?;
    if orchestrator.as_ref() == Some(caller) {
        callers::named(ctx, caller).await?;
    }
    Ok(())
}

/// Aether named no caller, and the orchestrator in charge can delegate (or
/// there is none): the call may be a helper's, so it is not made.
fn unnamed_refusal(ctx: &DaemonContext) -> ArchonError {
    let fix = callers::fix(ctx.config.aether.mode);
    ArchonError::Forbidden(format!(
        "This call changes Archon, and Aether did not say which agent makes it, so archond \
         cannot tell the orchestrator from an agent it delegated to. Tell the operator, who \
         has been notified: {fix} Meanwhile `archon orchestrator restart` starts an \
         orchestrator without native delegation, whose calls Archon accepts. Nothing was changed."
    ))
}

/// What Archon's records say `caller` is, and what it should do instead.
async fn describe(ctx: &DaemonContext, caller: &AetherAgentId) -> Result<String, ArchonError> {
    if super::retired::is_retired(ctx, caller).await? {
        return Ok(format!(
            "agent {caller} is an orchestrator Archon has replaced. Stop: nothing you do \
             reaches the operator any more."
        ));
    }
    Ok(match crate::pump::fleet_role(ctx, caller).await? {
        Some(FleetRole::Stage(item)) => format!(
            "agent {caller} is a stage agent of item {item}. Finish your stage; tell the \
             orchestrator what should be queued or changed with `message_orchestrator`."
        ),
        Some(FleetRole::StageChild(item)) => format!(
            "agent {caller} was delegated part of item {item}. Finish that task and report \
             what should be queued or changed to the agent that delegated it to you."
        ),
        Some(FleetRole::OrchestratorChild) => format!(
            "agent {caller} is one the orchestrator delegated to. Finish the task you were \
             given and put what should be queued or changed in your result."
        ),
        None => format!(
            "Archon does not know agent {caller}. If Aether restarted you as the orchestrator \
             moments ago, call once more in a few seconds; otherwise do not retry, and report \
             this to whoever gave you the task."
        ),
    })
}
