//! An operator message starts an orchestrator turn: the news held for it,
//! quiet news too, is delivered first, so the turn reads it before the message.

use std::time::Duration;

use archon_core::error::ArchonError;

use super::{read_outbox, write_outbox};
use crate::context::DaemonContext;

/// Longest an operator message waits behind held news. Delivery retries
/// while Aether is unreachable, and then the message cannot go either.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// Send every held event now, quiet news included, and return once Aether
/// has it, so a message sent next reaches the orchestrator after it.
pub async fn flush_quiet(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let Some(target) = mark_flush(ctx).await? else {
        return Ok(());
    };
    ctx.triggers.urgent.notify_waiters();
    ctx.triggers.news.notify_one();
    let mut settled = ctx.triggers.settled.subscribe();
    let reached = tokio::time::timeout(FLUSH_WAIT, settled.wait_for(|count| *count >= target));
    match reached.await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(closed)) => unreachable!("the sender lives in the context: {closed}"),
        Err(_) => {
            tracing::warn!(
                waited = ?FLUSH_WAIT,
                "held news is not delivered yet; the operator message goes first"
            );
            Ok(())
        }
    }
}

/// Mark held news to go out at once, and the settled-batch count reached
/// once it has: `None` when nothing is held or in flight.
async fn mark_flush(ctx: &DaemonContext) -> Result<Option<u64>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    // Keying takes every pending event, so they leave in one batch after the one in flight.
    let ahead = u64::from(outbox.in_flight.is_some()) + u64::from(!outbox.pending.is_empty());
    if ahead == 0 {
        return Ok(None);
    }
    if !outbox.pending.is_empty() && !outbox.flush {
        outbox.flush = true;
        write_outbox(&mut store, &outbox)?;
    }
    assert!(
        outbox.pending.is_empty() || outbox.is_due(archon_core::now()),
        "flushed news is due"
    );
    // Read under the store lock, which every settled batch is counted under.
    let target = *ctx.triggers.settled.borrow() + ahead;
    assert!(ahead <= 2, "one batch in flight and one keyed next at most");
    Ok(Some(target))
}
