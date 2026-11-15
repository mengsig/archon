//! What the stall watch reads from Aether: the orchestrator's turn, and,
//! once after a restart, whether that turn is open and has mail unread.

use archon_aether::client::AetherClient;
use archon_aether::events::{EventKind, parse_event};
use archon_aether::wire::EventQuery;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::OpenToolCall;

use super::{after, call_bound, lock};
use crate::context::DaemonContext;

/// How many of the orchestrator's newest inbox events the restart probe reads.
const INBOX_PAGE: usize = 50;

/// The turn as Aether saw it: its newest event, and the open call that
/// holds operator mail longest, `Err` when that cannot be told.
pub(super) type Turn = (Option<Timestamp>, Result<Option<OpenToolCall>, ArchonError>);

/// What Aether saw of `agent`'s turn. Only a store failure is returned as
/// one; any other failure is the turn's unknown call.
pub(super) async fn read_turn(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Turn, ArchonError> {
    match ctx.backend.turn_activity(agent).await {
        Ok(activity) if activity.call_known => {
            let longest = activity
                .open_calls
                .into_iter()
                .max_by_key(|call| after(call.requested_at, call_bound(call)));
            Ok((activity.last_event_at, Ok(longest)))
        }
        Ok(activity) => {
            let unknown = "its turn began before the newest events Aether returned";
            Ok((
                activity.last_event_at,
                Err(ArchonError::Unavailable(unknown.into())),
            ))
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "could not read the orchestrator's turn from Aether");
            Ok((None, Err(error)))
        }
    }
}

/// Learn once from Aether whether the orchestrator is mid-turn and has
/// operator mail unread, for an archond that restarted into a turn whose
/// events it had already consumed. Returns the current orchestrator.
pub(super) async fn probe_after_restart(
    ctx: &DaemonContext,
    now: Timestamp,
) -> Result<Option<AetherAgentId>, ArchonError> {
    let Some(agent) = super::super::current_orchestrator(ctx).await? else {
        return Ok(None);
    };
    if !lock(ctx).needs_probe(&agent) {
        return Ok(Some(agent));
    }
    let client = match ctx.aether.client() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "could not reach Aether for the stall check");
            return Ok(Some(agent));
        }
    };
    let running = match client.get_agent(&agent).await {
        Ok(found) => found.status["state"] == "running",
        Err(error) => {
            tracing::warn!(%error, "could not read the orchestrator's state for the stall check");
            return Ok(Some(agent));
        }
    };
    let mail = match unread_operator_mail(&client, &agent).await {
        Ok(mail) => mail,
        Err(error) => {
            // Mail unseen keeps the 30-minute rule: slower, never wrong.
            tracing::warn!(%error, "could not read the orchestrator's inbox for the stall check");
            None
        }
    };
    lock(ctx).probed(&agent, running, mail, now);
    Ok(Some(agent))
}

/// The newest operator mail of `agent` that no `inbox.consumed` has reached
/// yet, from its newest inbox events.
async fn unread_operator_mail(
    client: &AetherClient,
    agent: &AetherAgentId,
) -> Result<Option<u64>, ArchonError> {
    let query = EventQuery {
        agent: Some(agent.clone()),
        types: vec!["inbox.delivered".into(), "inbox.consumed".into()],
        ..EventQuery::default()
    };
    let newest_first = client.latest_events_raw(query, INBOX_PAGE).await?;
    assert!(
        newest_first.len() <= INBOX_PAGE,
        "the client bounds the page"
    );
    let mut newest_mail = None;
    for raw in newest_first {
        let event = parse_event(raw)
            .map_err(|error| ArchonError::Protocol(format!("an inbox event: {error}")))?;
        if event.agent.as_ref() != Some(agent) {
            continue;
        }
        match event.kind {
            EventKind::InboxConsumed { up_to } => {
                return Ok(newest_mail.filter(|seq| *seq > up_to));
            }
            EventKind::InboxDelivered {
                seq,
                from_operator: true,
                ..
            } => {
                newest_mail.get_or_insert(seq);
            }
            _ => {}
        }
    }
    Ok(newest_mail)
}
