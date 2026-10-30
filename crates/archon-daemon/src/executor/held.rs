//! A stage whose agents Aether holds (`blocked` on a usage limit or
//! credentials, or waiting for an approval) is suspended, not failed: its
//! deadline is moved on before it passes, so neither Aether nor the sweep
//! ends it for time it could not work. An agent blocked for inspection counts
//! only while the watchdog is on, which resumes it or fails its stage; with
//! the watchdog off it keeps its deadline and cannot wait forever.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::{AgentUpdate, DescendantState};
use archon_core::run::{StageRun, StageState};

use super::{resume, spawn, stall};
use crate::context::DaemonContext;

/// A held stage whose deadline is this near has it moved.
pub(super) const MARGIN_MINUTES: i64 = 10;
/// How far past `now` a held stage's deadline is moved.
const EXTENSION_MINUTES: i64 = 30;

/// Whether `deadline` is near enough at `now` for a held stage to move it.
pub(super) fn near(deadline: Timestamp, now: Timestamp) -> bool {
    deadline < now + chrono::Duration::minutes(MARGIN_MINUTES)
}

/// Move the deadline of `stage` if Aether holds its tree; `Some(true)` when it
/// now lies ahead, `None` when Aether did not answer whether it holds it. An
/// update it refuses leaves it as is; a later one a steer stored is kept.
pub(super) async fn hold_deadline(
    ctx: &DaemonContext,
    stage: &StageRun,
    now: Timestamp,
) -> Result<Option<bool>, ArchonError> {
    assert_eq!(
        stage.state,
        StageState::Running,
        "only a running stage is held"
    );
    let Some(lead) = &stage.aether_agent else {
        return Ok(Some(false));
    };
    let pulse = match ctx.backend.pulse(lead).await {
        Ok(pulse) => pulse,
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(%lead, %error, "pulse unanswered; whether it is held is read again next sweep");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let watched = stall::Policy::of(&ctx.config.limits).is_some();
    if !pulse.is_some_and(|pulse| pulse.held || (pulse.inspection && watched)) {
        return Ok(Some(false));
    }
    match move_deadline(ctx, stage, lead, now).await {
        Ok(ahead) => Ok(Some(ahead)),
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(%lead, %error, "deadline move unanswered; tried again next sweep");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Move held `stage`'s deadline, Aether's first, to 30 min past `now`;
/// `true` when it now lies ahead.
async fn move_deadline(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(lead),
        "the stage's own lead"
    );
    let until = now + chrono::Duration::minutes(EXTENSION_MINUTES);
    // A steer's owed deadline is sent under the same lock, so neither lowers the other.
    let _steering = ctx.coordination.steering.hold(&stage.id).await;
    let Some(stored) = running_deadline(ctx, &stage.id).await? else {
        return Ok(false);
    };
    if stored >= until {
        return Ok(true);
    }
    if !raise(ctx, lead, until).await? {
        return Ok(false);
    }
    let kept = spawn::move_deadline(&mut *ctx.store.lock().await, &stage.id, until)?;
    // A steer stored a later one while ours was sent; Aether gets that again.
    if kept > until && !raise(ctx, lead, kept).await? {
        return Ok(false);
    }
    tracing::info!(stage = %stage.id, deadline = %kept, "agent held by Aether; deadline moved");
    Ok(true)
}

/// The deadline in stage `id`'s stored request as it is now; `None` once the
/// stage stopped running or has none.
async fn running_deadline(
    ctx: &DaemonContext,
    id: &StageRunId,
) -> Result<Option<Timestamp>, ArchonError> {
    let store = ctx.store.lock().await;
    if store.stage_run(id)?.state != StageState::Running {
        return Ok(None);
    }
    Ok(spawn::load_spawn(&store, id)?.request.deadline)
}

/// Raise the deadline of `lead` and the agents beneath it to `until`;
/// `false` when Aether refuses, most likely because the agent ended.
async fn raise(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    until: Timestamp,
) -> Result<bool, ArchonError> {
    let update = AgentUpdate {
        deadline: Some(until),
        ..AgentUpdate::default()
    };
    assert!(!update.is_empty(), "a raise changes the deadline");
    let raised = match ctx.backend.supports_subtree_update().await? {
        true => ctx.backend.update_subtree(lead, update).await,
        false => raise_each(ctx, lead, update).await,
    };
    match raised {
        Ok(()) => Ok(true),
        Err(error) if resume::unanswered(&error) || error.is_store() => Err(error),
        Err(refusal) => {
            tracing::warn!(%lead, %refusal, "held agent's deadline not moved");
            Ok(false)
        }
    }
}

/// Raise `lead`, then each live helper beneath it whose deadline is lower,
/// for an Aether that cannot update a subtree: a held helper may be the one
/// Aether holds. A helper that ended refuses; one with no deadline keeps none.
async fn raise_each(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    update: AgentUpdate,
) -> Result<(), ArchonError> {
    let until = update.deadline.expect("a raise sets a deadline");
    ctx.backend.update_agent(lead, update.clone()).await?;
    let helpers = ctx.backend.descendants(lead).await?;
    let lower = helpers.iter().filter(|helper| {
        helper.state == DescendantState::Live && helper.deadline.is_some_and(|d| d < until)
    });
    for helper in lower {
        assert_ne!(&helper.agent, lead, "an agent is not beneath itself");
        match ctx
            .backend
            .update_agent(&helper.agent, update.clone())
            .await
        {
            Ok(()) => {}
            Err(error) if resume::unanswered(&error) || error.is_store() => return Err(error),
            Err(refusal) => {
                tracing::warn!(agent = %helper.agent, %refusal, "held helper's deadline not moved; it has most likely ended");
            }
        }
    }
    Ok(())
}

/// Hold the deadlines of the `nearing` stages Aether holds, each read at
/// most once a minute, and return the `expired` ones it says it does not
/// hold; one it did not answer for waits for the next sweep.
pub(super) async fn sweep(
    ctx: &DaemonContext,
    expired: Vec<StageRun>,
    nearing: &[StageRun],
    now: Timestamp,
) -> Result<Vec<StageRunId>, ArchonError> {
    for stage in nearing {
        assert_eq!(
            stage.state,
            StageState::Running,
            "only a running stage nears its deadline"
        );
        let readable = stage
            .aether_agent
            .as_ref()
            .is_some_and(|lead| ctx.activity.may_read_deadline(lead, now));
        if readable {
            hold_deadline(ctx, stage, now).await?;
        }
    }
    let mut due = Vec::with_capacity(expired.len());
    for stage in expired {
        let held = match stage.state {
            StageState::Running => hold_deadline(ctx, &stage, now).await?,
            _ => Some(false),
        };
        if held == Some(false) {
            due.push(stage.id);
        }
    }
    Ok(due)
}
