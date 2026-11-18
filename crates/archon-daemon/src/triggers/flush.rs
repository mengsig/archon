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
    if !mark_flush(ctx).await? {
        return Ok(());
    }
    ctx.triggers.urgent.notify_waiters();
    ctx.triggers.news.notify_one();
    match tokio::time::timeout(FLUSH_WAIT, flushed(ctx)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                waited = ?FLUSH_WAIT,
                "held news is not delivered yet; the operator message goes first"
            );
            Ok(())
        }
    }
}

/// Wait until no news a flush marked is held or in flight. Only a settled
/// batch can end that, so each settle is a cue to look again.
async fn flushed(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let mut settled = ctx.triggers.settled.subscribe();
    loop {
        settled.borrow_and_update();
        let outbox = read_outbox(&*ctx.store.lock().await)?;
        let in_flight = outbox.in_flight.as_ref().is_some_and(|batch| batch.flushed);
        if outbox.flush_left == 0 && !in_flight {
            return Ok(());
        }
        assert!(
            outbox.flush_left == 0 || outbox.flush,
            "news a flush waits for goes out at once"
        );
        let changed = settled.changed().await;
        assert!(changed.is_ok(), "the sender lives in the context");
    }
}

/// Mark the batch in flight and every held event as news the flush waits
/// for, to go out at once: `false` when nothing is held or in flight.
async fn mark_flush(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    if outbox.in_flight.is_none() && outbox.pending.is_empty() {
        return Ok(false);
    }
    if let Some(batch) = outbox.in_flight.as_mut() {
        batch.flushed = true;
    }
    // Keying takes from the front and later news joins the back.
    outbox.flush_left = outbox.pending.len();
    outbox.flush = outbox.flush || !outbox.pending.is_empty();
    write_outbox(&mut store, &outbox)?;
    assert!(
        outbox.pending.is_empty() || outbox.is_due(archon_core::now()),
        "flushed news is due"
    );
    Ok(true)
}
