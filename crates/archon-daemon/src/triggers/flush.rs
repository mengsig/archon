//! An operator message starts an orchestrator turn: the news held for it,
//! quiet news too, is delivered first, so the turn reads it before the message.

use std::time::Duration;

use archon_core::error::ArchonError;

use super::{Outbox, read_outbox, write_outbox};
use crate::context::DaemonContext;

/// Longest an operator message waits behind held news. Delivery retries
/// while Aether is unreachable, and then the message cannot go either.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// Send every held event now, quiet news included, and return once Aether
/// has it, so a message sent next reaches the orchestrator after it.
pub async fn flush_quiet(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let Some(through) = mark_flush(ctx).await? else {
        return Ok(());
    };
    ctx.triggers.urgent.notify_waiters();
    ctx.triggers.news.notify_one();
    match tokio::time::timeout(FLUSH_WAIT, delivered_through(ctx, through)).await {
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

/// Wait until no news numbered `through` or lower is held or in flight.
/// Only a settled batch can end that, so each settle is a cue to look again.
async fn delivered_through(ctx: &DaemonContext, through: u64) -> Result<(), ArchonError> {
    assert!(through > 0, "news is numbered from 1");
    let mut settled = ctx.triggers.settled.subscribe();
    loop {
        settled.borrow_and_update();
        let outbox = read_outbox(&*ctx.store.lock().await)?;
        if !waits_on(&outbox, through) {
            return Ok(());
        }
        let changed = settled.changed().await;
        assert!(changed.is_ok(), "the sender lives in the context");
    }
}

/// Whether `outbox` still holds news numbered `through` or lower. Unnumbered
/// news came after every flush; a compaction's digest takes its earliest number.
fn waits_on(outbox: &Outbox, through: u64) -> bool {
    let early = |seq: u64| seq > 0 && seq <= through;
    let in_flight = outbox.in_flight.as_ref();
    in_flight.is_some_and(|batch| early(batch.first_seq))
        || outbox.pending.iter().any(|news| early(news.seq))
}

/// Number the batch in flight and every held event not yet numbered, and
/// mark the news to go out at once: the last number, `None` when nothing is held.
async fn mark_flush(ctx: &DaemonContext) -> Result<Option<u64>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    if outbox.in_flight.is_none() && outbox.pending.is_empty() {
        return Ok(None);
    }
    let mut last = outbox.last_seq;
    let batch = outbox
        .in_flight
        .as_mut()
        .filter(|batch| batch.first_seq == 0);
    if let Some(batch) = batch {
        last += 1;
        batch.first_seq = last;
    }
    for news in outbox.pending.iter_mut().filter(|news| news.seq == 0) {
        last += 1;
        news.seq = last;
    }
    outbox.last_seq = last;
    outbox.flush = outbox.flush || !outbox.pending.is_empty();
    write_outbox(&mut store, &outbox)?;
    assert!(
        last > 0 && waits_on(&outbox, last),
        "the flush waits for what it numbered"
    );
    assert!(
        outbox.pending.is_empty() || outbox.is_due(archon_core::now()),
        "flushed news is due"
    );
    Ok(Some(last))
}
