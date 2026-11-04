//! The change stream and item history (CONTRACTS.md §1.3, owner AR-C):
//! `subscribe_changes`, `changes_page`, `item_history` and the `as_of` of
//! `list_item_summaries`. The store is the source of truth: the pump hands
//! every committed change, once and in order, to live subscribers, and one
//! that falls behind the bounded buffer is closed, never buffered further.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::changes::MAX_CHANGES_PAGE;
use archon_core::protocol::{Change, ErrorReply, ItemQuery, Reply, ServerFrame, feature};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::context::DaemonContext;

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[feature::CHANGES, feature::ITEM_HISTORY];

/// Changes buffered per live subscriber; one that falls further behind is closed.
pub const CHANGE_BUFFER: usize = 1024;
/// How often the pump looks for committed changes.
const PUMP_EVERY: Duration = Duration::from_millis(100);
/// Changes read from the store per query.
const BATCH: usize = 500;
/// How often retention trims the oldest changes, and how many per write.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
const PRUNE_BATCH: usize = 5_000;

/// The live side of the stream: what the pump read, for every subscriber.
/// `sent` outlives a pump restart, so no committed change is skipped.
#[derive(Debug)]
pub struct ChangeHub {
    live: broadcast::Sender<Change>,
    /// The last seq handed to subscribers; the pump reads after it.
    sent: AtomicU64,
}

impl ChangeHub {
    /// A hub whose subscribers are closed once `buffer` changes behind, with
    /// every change after `sent` still to hand out.
    pub fn new(buffer: usize, sent: u64) -> Self {
        assert!(buffer > 0, "a subscriber buffers a change");
        Self {
            live: broadcast::channel(buffer).0,
            sent: AtomicU64::new(sent),
        }
    }

    /// The last seq the pump handed to subscribers.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.live.subscribe()
    }
}

/// From the first change: for a fresh store, or a test's.
impl Default for ChangeHub {
    fn default() -> Self {
        Self::new(CHANGE_BUFFER, 0)
    }
}

/// One `subscribe_changes` on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeSubscription {
    /// Numbered per connection, beside its feed subscriptions.
    pub id: u64,
    /// The request's id, for the `subscribed` reply.
    pub request: u64,
    pub after: Option<u64>,
}

/// A page or history limit: 1 to [`MAX_CHANGES_PAGE`], the most by default.
fn page_limit(limit: Option<u32>) -> Result<usize, ArchonError> {
    let limit = limit.unwrap_or(MAX_CHANGES_PAGE);
    if limit == 0 || limit > MAX_CHANGES_PAGE {
        return Err(ArchonError::Invalid(format!(
            "limit must be 1 to {MAX_CHANGES_PAGE}, not {limit}"
        )));
    }
    Ok(limit as usize)
}

/// Served by the connection, which owns the frames a stream sends.
pub(super) async fn subscribe(
    _ctx: &DaemonContext,
    _after: Option<u64>,
) -> Result<Reply, ArchonError> {
    Err(ArchonError::Invalid(
        "subscribe_changes is served by the connection, not as a plain request".into(),
    ))
}

pub(super) async fn page(
    ctx: &DaemonContext,
    after: u64,
    limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    let limit = page_limit(limit)?;
    let store = ctx.read_store().await?;
    let changes = store.changes_after(after, limit)?;
    let last_seq = store.last_change_seq()?;
    drop(store);
    assert!(changes.len() <= limit, "a page holds at most its limit");
    assert!(
        changes.last().is_none_or(|c| c.seq <= last_seq),
        "a page and its last_seq are read in one snapshot"
    );
    Ok(Reply::Changes { changes, last_seq })
}

pub(super) async fn item_history(
    ctx: &DaemonContext,
    item: &QueueItemId,
    limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    let limit = page_limit(limit)?;
    let transitions = ctx.read_store().await?.item_history(item, limit)?;
    assert!(transitions.len() <= limit);
    Ok(Reply::ItemHistory {
        item: item.clone(),
        transitions,
    })
}

/// `list_item_summaries` with `with_as_of`: the summaries and the last change
/// seq, both read in one snapshot, so `seq > as_of` is exactly what they miss.
pub(super) async fn summaries_at(
    ctx: &DaemonContext,
    query: &ItemQuery,
) -> Result<Reply, ArchonError> {
    assert!(query.with_as_of, "routed only for with_as_of");
    let waiting = super::waiting::WaitingView::read(ctx).await?;
    let estimates = ctx.config.limits.count_estimated_cost;
    let store = ctx.read_store().await?;
    let as_of = store.last_change_seq()?;
    let summaries = super::item_views::summaries_in(&store, query, estimates, &waiting)?;
    assert_eq!(
        store.last_change_seq()?,
        as_of,
        "the snapshot holds while its summaries are read"
    );
    drop(store);
    Ok(Reply::ItemSummariesAt { summaries, as_of })
}

/// Hands every change committed after the hub's `sent` to live subscribers,
/// in seq order, whether or not anyone listens: a subscriber's own catch-up
/// covers what was sent before it subscribed, and nothing later is skipped.
/// It also applies retention, at start and every [`PRUNE_EVERY`].
pub async fn pump(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut tick = tokio::time::interval(PUMP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pruned_at: Option<Instant> = None;
    loop {
        tick.tick().await;
        while send_batch(&ctx).await? == BATCH {}
        if pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
            prune(&ctx).await?;
            pruned_at = Some(Instant::now());
        }
    }
}

/// Deletes what retention no longer keeps, one bounded write at a time so
/// other writers get the store between them; how many changes it deleted.
async fn prune(ctx: &DaemonContext) -> Result<u64, ArchonError> {
    let now = archon_core::now();
    let mut total = 0;
    loop {
        let deleted = ctx.store.lock().await.prune_changes(now, PRUNE_BATCH)?;
        assert!(deleted <= PRUNE_BATCH as u64, "a prune stays in its batch");
        total += deleted;
        if deleted < PRUNE_BATCH as u64 {
            break;
        }
        tokio::task::yield_now().await;
    }
    if total > 0 {
        tracing::info!(deleted = total, "pruned changes past retention");
    }
    Ok(total)
}

/// One batch after `sent`; how many changes it held.
async fn send_batch(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let hub = &ctx.changes;
    let sent = hub.sent.load(Ordering::Acquire);
    let batch = match ctx.read_store().await?.changes_after(sent, BATCH) {
        Ok(batch) => batch,
        // Retention passed the pump: subscribers left behind see the gap,
        // read the store and are told the history expired.
        Err(ArchonError::HistoryExpired { kept, .. }) => {
            hub.sent.store(kept.last_seq, Ordering::Release);
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    let count = batch.len();
    for change in batch {
        let seq = change.seq;
        assert_eq!(
            seq,
            hub.sent.load(Ordering::Acquire) + 1,
            "one pump, in order"
        );
        // No receiver is no failure: a later subscriber reads the store.
        let _ = hub.live.send(change);
        hub.sent.store(seq, Ordering::Release);
    }
    Ok(count)
}

/// Runs one subscription until its frames close. A refused cursor is the
/// subscribe's reply; a failure later, a lag included, ends it with an
/// id-0 error naming the subscription, so the client resubscribes.
pub async fn stream(
    ctx: Arc<DaemonContext>,
    sub: ChangeSubscription,
    frames: mpsc::Sender<ServerFrame>,
) {
    let Err(error) = run(&ctx, &sub, &frames).await else {
        return;
    };
    tracing::debug!(sub = sub.id, %error, "change subscription ended");
    let mut refusal = ErrorReply::from(&error);
    refusal.message = format!("change subscription {} ended: {}", sub.id, refusal.message);
    let frame = ServerFrame::Reply {
        id: 0,
        result: Err(refusal),
    };
    let _gone = frames.send(frame).await;
}

async fn run(
    ctx: &DaemonContext,
    sub: &ChangeSubscription,
    frames: &mpsc::Sender<ServerFrame>,
) -> Result<(), ArchonError> {
    // Subscribe before reading the store, so nothing committed in between is missed.
    let mut live = ctx.changes.subscribe();
    let store = ctx.read_store().await?;
    let opened = match sub.after {
        Some(after) => store.check_change_cursor(after).map(|kept| kept.last_seq),
        None => store.last_change_seq(),
    };
    drop(store);
    let result = opened.as_ref().map(|&last_seq| Reply::Subscribed {
        id: sub.id,
        last_seq,
    });
    let reply = ServerFrame::Reply {
        id: sub.request,
        result: result.map_err(|e| ErrorReply::from(&*e)),
    };
    if frames.send(reply).await.is_err() {
        return Ok(());
    }
    let Ok(last_seq) = opened else {
        return Ok(());
    };
    let mut cursor = sub.after.unwrap_or(last_seq);
    if !catch_up(ctx, sub, frames, &mut cursor).await? {
        return Ok(());
    }
    follow(ctx, sub, frames, &mut live, &mut cursor).await
}

/// Live changes in seq order; a gap is read from the store.
async fn follow(
    ctx: &DaemonContext,
    sub: &ChangeSubscription,
    frames: &mpsc::Sender<ServerFrame>,
    live: &mut broadcast::Receiver<Change>,
    cursor: &mut u64,
) -> Result<(), ArchonError> {
    loop {
        let received = tokio::select! {
            received = live.recv() => received,
            () = frames.closed() => return Ok(()),
        };
        let open = match received {
            Ok(change) if change.seq <= *cursor => true,
            Ok(change) if change.seq == *cursor + 1 => deliver(sub, frames, cursor, change).await,
            Ok(_) => catch_up(ctx, sub, frames, cursor).await?,
            Err(RecvError::Lagged(missed)) => {
                return Err(ArchonError::Unavailable(format!(
                    "it fell {missed} changes behind; resubscribe with after: {cursor}"
                )));
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        if !open {
            return Ok(());
        }
    }
}

/// Send every stored change after `cursor`. `false` once the client is gone.
async fn catch_up(
    ctx: &DaemonContext,
    sub: &ChangeSubscription,
    frames: &mpsc::Sender<ServerFrame>,
    cursor: &mut u64,
) -> Result<bool, ArchonError> {
    loop {
        let batch = ctx.read_store().await?.changes_after(*cursor, BATCH)?;
        let done = batch.len() < BATCH;
        for change in batch {
            if !deliver(sub, frames, cursor, change).await {
                return Ok(false);
            }
        }
        if done {
            return Ok(true);
        }
    }
}

async fn deliver(
    sub: &ChangeSubscription,
    frames: &mpsc::Sender<ServerFrame>,
    cursor: &mut u64,
    change: Change,
) -> bool {
    assert_eq!(change.seq, *cursor + 1, "changes are delivered gapless");
    *cursor = change.seq;
    let frame = ServerFrame::Change {
        sub: sub.id,
        change,
    };
    frames.send(frame).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_one_to_a_page() {
        assert_eq!(page_limit(None).unwrap(), MAX_CHANGES_PAGE as usize);
        assert_eq!(page_limit(Some(1)).unwrap(), 1);
        assert!(page_limit(Some(0)).is_err());
        assert!(page_limit(Some(MAX_CHANGES_PAGE + 1)).is_err());
    }
}
