//! Orchestrator events: the quiet chat's replies (via `ReplyTracker`), its
//! `ask_user` questions, its spend, reviving it when it stops, and adopting
//! the agent Aether restarted it into.

use archon_aether::AetherEvent;
use archon_aether::events::{EventKind, InboxMessage};
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef};
use archon_core::error::ArchonError;
use archon_core::notify::FeedEntry;

use super::fleet;
use crate::context::DaemonContext;
use crate::feed::{self, REPLY_STATE_KEY, ReplyTracker};
use crate::{chat_answer, supervisor, triggers};

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
    match &event.kind {
        EventKind::InboxDelivered {
            seq,
            from_operator,
            message,
        } => delivered(ctx, event, *seq, *from_operator, message.as_ref()).await,
        EventKind::InboxConsumed { up_to } => {
            track_once(ctx, event, |replies| replies.consumed(*up_to))
                .await
                .map(drop)
        }
        EventKind::AgentMessage {
            assistant: true,
            text,
        } => track_once(ctx, event, |replies| replies.assistant_text(text))
            .await
            .map(drop),
        EventKind::AgentWaiting { idle } => {
            track_once(ctx, event, |replies| replies.waiting(*idle))
                .await
                .map(drop)
        }
        EventKind::AgentTurnCompleted { next, .. } => {
            turn_completed(ctx, event.seq, next.as_deref()).await?;
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

/// Track one delivery to the orchestrator's inbox and, the first time an
/// operator's message is seen, apply the `/answer` command it may be.
async fn delivered(
    ctx: &DaemonContext,
    event: &AetherEvent,
    seq: u64,
    from_operator: bool,
    message: Option<&InboxMessage>,
) -> Result<(), ArchonError> {
    let fresh = track_once(ctx, event, |replies| {
        replies.delivered(seq, from_operator);
    })
    .await?;
    // Marked seen first: a crash here drops the command, never repeats it.
    match message {
        Some(message) if fresh && from_operator => chat_answer::handle(ctx, &message.text).await,
        _ => Ok(()),
    }
}

/// Apply one stream event to the reply tracker, unless it is a replay.
/// `true` when the event was new.
async fn track_once(
    ctx: &DaemonContext,
    event: &AetherEvent,
    change: impl FnOnce(&mut ReplyTracker),
) -> Result<bool, ArchonError> {
    feed::track(ctx, |replies| {
        let fresh = replies.first_sight(event.seq);
        if fresh {
            change(replies);
        }
        fresh
    })
    .await
}

/// Post the turn's reply when it answers the operator. The reply and the
/// tracker that no longer awaits it are written under one store lock.
async fn turn_completed(
    ctx: &DaemonContext,
    seq: u64,
    next: Option<&str>,
) -> Result<(), ArchonError> {
    let mut replies = ctx.replies.lock().await;
    let before = replies.clone();
    let reply = if replies.first_sight(seq) {
        replies.turn_completed(next)
    } else {
        None
    };
    if *replies == before {
        assert!(reply.is_none(), "a shown reply always changes the tracker");
        return Ok(());
    }
    let raw = serde_json::to_string(&*replies)?;
    let item = {
        let mut store = ctx.store.lock().await;
        let item = match reply {
            Some(text) => {
                assert!(!text.trim().is_empty(), "the tracker ignores blank text");
                let entry = FeedEntry::Orchestrator { text };
                Some(store.append_feed(entry, archon_core::now())?)
            }
            None => None,
        };
        store.kv_set(REPLY_STATE_KEY, &raw)?;
        item
    };
    if let Some(item) = item {
        // No receivers is normal (nobody has chat open); not an error.
        let _ = ctx.feed.send(item);
    }
    Ok(())
}

/// A long-lived orchestrator never stops on purpose; revive it. Failing to
/// revive is reported by the supervisor, so only store errors propagate.
async fn stopped(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    feed::track(ctx, ReplyTracker::interrupted).await?;
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
