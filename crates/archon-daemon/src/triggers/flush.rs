//! An operator message starts an orchestrator turn: the news held for it,
//! quiet news too, is delivered first, so the turn reads it before the message.

use std::time::Duration;

use archon_core::error::ArchonError;

use super::{batch_count, read_outbox, write_outbox};
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
    let reached = tokio::time::timeout(FLUSH_WAIT, settled_or_drained(ctx, target));
    match reached.await {
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

/// Wait until `target` batches have settled, or nothing is held at all:
/// compaction while waiting can leave fewer batches than counted.
async fn settled_or_drained(ctx: &DaemonContext, target: u64) -> Result<(), ArchonError> {
    let mut settled = ctx.triggers.settled.subscribe();
    loop {
        if *settled.borrow_and_update() >= target {
            return Ok(());
        }
        let outbox = read_outbox(&*ctx.store.lock().await)?;
        if outbox.in_flight.is_none() && outbox.pending.is_empty() {
            return Ok(());
        }
        let changed = settled.changed().await;
        assert!(changed.is_ok(), "the sender lives in the context");
    }
}

/// Mark held news to go out at once, and the settled-batch count reached
/// once it has: `None` when nothing is held or in flight.
async fn mark_flush(ctx: &DaemonContext) -> Result<Option<u64>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    // Pending news leaves in as many batches as its size splits it into.
    let ahead = u64::from(outbox.in_flight.is_some()) + batch_count(&outbox.pending)?;
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
    assert!(target >= ahead, "the settled count never wraps");
    Ok(Some(target))
}
