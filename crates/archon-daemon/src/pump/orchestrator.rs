//! Orchestrator events: waking `conversation` readers, the operator's chat
//! commands, its `ask_user` questions, its spend, reviving it when it stops, and adopting
//! the agent Aether restarted it into.

use archon_aether::AetherEvent;
use archon_aether::events::{EventKind, InboxMessage};
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef};
use archon_core::error::ArchonError;

use super::fleet;
use crate::context::DaemonContext;
use crate::feed;
use crate::{operator_commands, supervisor, triggers};

/// Aether's event type for an agent whose older context became a summary.
const COMPACTION_COMPLETED: &str = "compaction.completed";

pub(super) async fn handle(ctx: &DaemonContext, event: &AetherEvent) -> Result<(), ArchonError> {
    let Some(agent) = &event.agent else {
        return Ok(());
    };
    // Events of a replaced orchestrator arrive until the stream switches.
    if supervisor::current_orchestrator(ctx).await?.as_ref() != Some(agent) {
        return Ok(());
    }
    ctx.supervisor.saw_event(agent, &event.kind);
    if grows_conversation(&event.kind) {
        ctx.conversation.send_modify(|grown| *grown += 1);
    }
    match &event.kind {
        EventKind::InboxDelivered {
            from_operator: true,
            message: Some(message),
            seq,
        } => operator_message(ctx, event, *seq, message).await,
        EventKind::AgentTurnCompleted { next, .. } => {
            // Undoes, and reports, a write into a mirror made in that turn.
            ctx.mirrors.request();
            supervisor::turn_ended(ctx, next.as_deref()).await
        }
        EventKind::QuestionAsked { question } => {
            let aether = AetherQuestionRef {
                agent: agent.clone(),
                seq: event.seq,
            };
            feed::aether_question(ctx, aether, question, None)
                .await
                .map(drop)
        }
        EventKind::AgentFailed { .. }
        | EventKind::AgentCancelled { .. }
        | EventKind::AgentCompleted => stopped(ctx, agent).await,
        EventKind::AgentRestarted { from, to } if from == agent => {
            supervisor::adopt_restart(ctx, from, to).await
        }
        EventKind::AgentUsageUpdated { usage } => {
            fleet::record_spend(ctx, event, agent, None, usage).await
        }
        // What it was told of the directives may not be in its summary.
        EventKind::Other { type_name } if type_name == COMPACTION_COMPLETED => {
            triggers::restate_directives(ctx, "your context was compacted").await
        }
        _ => Ok(()),
    }
}

/// Events that add to the conversation `conversation` readers wait on.
fn grows_conversation(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::AgentMessage { .. }
            | EventKind::InboxConsumed { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::AgentFailed { .. }
    )
}

/// The first time an operator's message is seen, apply any daemon chat
/// command it contains.
async fn operator_message(
    ctx: &DaemonContext,
    event: &AetherEvent,
    seq: u64,
    message: &InboxMessage,
) -> Result<(), ArchonError> {
    assert!(seq > 0, "inbox seqs start at 1");
    // Marked seen first: a crash here drops the command, never repeats it.
    if !feed::first_sight(ctx, event.seq).await? {
        return Ok(());
    }
    operator_commands::handle(ctx, event, seq, message).await
}

/// A long-lived orchestrator never stops on purpose; revive it. Failing to
/// revive is reported by the supervisor, so only store errors propagate.
async fn stopped(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    match supervisor::ensure_orchestrator(ctx).await {
        Ok(revived) => {
            tracing::info!(%agent, %revived, "orchestrator stopped and was revived");
            Ok(())
        }
        Err(error) if error.is_store() => Err(error),
        Err(ArchonError::Unavailable(reason)) if !ctx.aether.is_connected() => {
            Err(ArchonError::Unavailable(reason))
        }
        Err(error) => {
            tracing::warn!(%agent, %error, "orchestrator could not be revived");
            Ok(())
        }
    }
}
