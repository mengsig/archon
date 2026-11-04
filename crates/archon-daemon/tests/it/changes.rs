//! The change stream as served (CONTRACTS.md §1.3): a subscription carries
//! every committed change once and in order, one that falls behind is
//! closed, and pages, history and summaries read one snapshot.

use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    Change, ChangeSubject, ErrorCode, ErrorReply, ItemQuery, QueueStateKind, Reply, Request,
    ServerFrame, TransitionActor,
};
use archon_core::queue::QueueItem;
use archon_daemon::handlers::changes::{self, CHANGE_BUFFER, ChangeSubscription};
use tokio::sync::mpsc;

use crate::common::Harness;

const WAIT: Duration = Duration::from_secs(10);

fn subscription(after: Option<u64>) -> ChangeSubscription {
    ChangeSubscription {
        id: 3,
        request: 7,
        after,
    }
}

/// Runs `sub` with a frame buffer of `capacity`, as a connection would.
fn open(h: &Harness, sub: ChangeSubscription, capacity: usize) -> mpsc::Receiver<ServerFrame> {
    let (frames, received) = mpsc::channel(capacity);
    tokio::spawn(changes::stream(h.ctx.clone(), sub, frames));
    received
}

async fn next(frames: &mut mpsc::Receiver<ServerFrame>) -> Option<ServerFrame> {
    tokio::time::timeout(WAIT, frames.recv())
        .await
        .expect("a frame in time")
}

/// The subscribe's own reply: the head it opened at, or its refusal.
async fn opened(frames: &mut mpsc::Receiver<ServerFrame>) -> Result<u64, ErrorReply> {
    match next(frames).await {
        Some(ServerFrame::Reply { id: 7, result }) => result.map(|reply| match reply {
            Reply::Subscribed { id: 3, last_seq } => last_seq,
            other => panic!("expected subscribed, got {other:?}"),
        }),
        other => panic!("expected the subscribe reply, got {other:?}"),
    }
}

fn change_of(frame: Option<ServerFrame>) -> Change {
    match frame {
        Some(ServerFrame::Change { sub: 3, change }) => change,
        other => panic!("expected a change, got {other:?}"),
    }
}

/// `count` queued copies of `like`, written straight to the store.
async fn insert_copies(h: &Harness, like: &QueueItem, count: usize) -> Vec<QueueItemId> {
    let mut store = h.ctx.store.lock().await;
    (0..count)
        .map(|_| {
            let copy = QueueItem {
                id: QueueItemId::new(),
                ..like.clone()
            };
            store.insert_item(&copy, None).expect("insert").id
        })
        .collect()
}

async fn last_seq(h: &Harness) -> u64 {
    h.ctx.read_store().await.unwrap().last_change_seq().unwrap()
}

async fn page(
    h: &Harness,
    after: u64,
    limit: Option<u32>,
) -> Result<(Vec<Change>, u64), ArchonError> {
    match h.cli(Request::ChangesPage { after, limit }).await? {
        Reply::Changes { changes, last_seq } => Ok((changes, last_seq)),
        other => panic!("expected changes, got {other:?}"),
    }
}

#[tokio::test]
async fn a_subscription_catches_up_then_follows_every_commit_in_order() {
    let h = Harness::new().await;
    let first = h.enqueue("first").await;
    let pump = tokio::spawn(changes::pump(h.ctx.clone()));
    let mut frames = open(&h, subscription(Some(0)), 16);
    let head = opened(&mut frames).await.expect("subscribed");
    assert_eq!(head, last_seq(&h).await);
    let copies = insert_copies(&h, &first, 40).await;
    let end = last_seq(&h).await;
    let mut streamed = Vec::new();
    while streamed.last().is_none_or(|c: &Change| c.seq < end) {
        streamed.push(change_of(next(&mut frames).await));
    }
    let seqs: Vec<u64> = streamed.iter().map(|c| c.seq).collect();
    assert_eq!(seqs, (1..=end).collect::<Vec<_>>(), "once each, in order");
    let (paged, paged_last) = page(&h, 0, None).await.unwrap();
    assert_eq!(paged, streamed, "the stream is the store's");
    assert_eq!(paged_last, end);
    let told: Vec<QueueItemId> = streamed
        .iter()
        .filter_map(|c| match &c.subject {
            ChangeSubject::Item { item, .. } if *item != first.id => Some(item.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(told, copies);
    pump.abort();
}

#[tokio::test]
async fn a_subscription_that_falls_behind_is_closed_not_buffered() {
    let h = Harness::new().await;
    let first = h.enqueue("first").await;
    let pump = tokio::spawn(changes::pump(h.ctx.clone()));
    let mut frames = open(&h, subscription(None), 1);
    let head = opened(&mut frames).await.expect("subscribed");
    insert_copies(&h, &first, CHANGE_BUFFER + 50).await;
    let end = last_seq(&h).await;
    assert!(end > head + CHANGE_BUFFER as u64);
    let pumped = async {
        while h.ctx.changes.sent() < end {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(WAIT, pumped)
        .await
        .expect("pumped in time");
    let mut delivered = 0;
    let refusal = loop {
        match next(&mut frames).await {
            Some(ServerFrame::Change { .. }) => delivered += 1,
            Some(ServerFrame::Reply {
                id: 0,
                result: Err(refusal),
            }) => break refusal,
            other => panic!("expected a change or the close, got {other:?}"),
        }
    };
    assert!(
        delivered < CHANGE_BUFFER,
        "closed, not buffered: {delivered}"
    );
    assert_eq!(refusal.code, ErrorCode::Unavailable, "{refusal:?}");
    assert!(
        refusal.message.contains("change subscription 3 ended")
            && refusal.message.contains("resubscribe with after"),
        "{refusal:?}"
    );
    assert!(next(&mut frames).await.is_none(), "the stream ended");
    pump.abort();
}

#[tokio::test]
async fn a_cursor_the_store_does_not_hold_is_refused_on_subscribe() {
    let h = Harness::new().await;
    h.enqueue("first").await;
    let end = last_seq(&h).await;
    let mut frames = open(&h, subscription(Some(end + 1_000)), 4);
    let refusal = opened(&mut frames)
        .await
        .expect_err("not this store's cursor");
    let ErrorCode::HistoryExpired(kept) = refusal.code else {
        panic!("expected history_expired, got {refusal:?}");
    };
    assert_eq!((kept.first_seq, kept.last_seq), (1, end));
    assert!(next(&mut frames).await.is_none(), "a refused stream ends");
    let paged = page(&h, end + 1_000, None).await;
    assert!(
        matches!(paged, Err(ArchonError::HistoryExpired { .. })),
        "{paged:?}"
    );
}

#[tokio::test]
async fn history_pages_and_summaries_read_the_same_store() {
    let h = Harness::new().await;
    let item = h.enqueue("first").await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("not needed".into()),
    };
    h.cli(cancel).await.expect("cancel");
    let history = Request::ItemHistory {
        item: item.id.clone(),
        limit: None,
    };
    let Reply::ItemHistory { transitions, .. } = h.cli(history).await.unwrap() else {
        panic!("expected item history");
    };
    let first = transitions.first().expect("its creation");
    let last = transitions.last().expect("its cancel");
    assert_eq!(first.from, None);
    assert_eq!(last.to, QueueStateKind::Cancelled);
    assert_eq!(last.by, TransitionActor::Cli, "the CLI cancelled it");
    assert_eq!(last.reason.as_deref(), Some("not needed"));
    assert!(transitions.windows(2).all(|w| w[0].seq < w[1].seq));
    let query = ItemQuery {
        with_as_of: true,
        ..ItemQuery::default()
    };
    let reply = h.cli(Request::ListItemSummaries(query)).await.unwrap();
    let Reply::ItemSummariesAt { summaries, as_of } = reply else {
        panic!("expected summaries with as_of, got {reply:?}");
    };
    assert_eq!(as_of, last_seq(&h).await);
    assert_eq!(as_of, last.seq, "the cancel is the last change");
    assert!(summaries.iter().any(|s| s.item.id == item.id));
    let (_, head) = page(&h, as_of, Some(1)).await.unwrap();
    assert_eq!(head, as_of);
    let refused = page(&h, 0, Some(0)).await;
    assert!(
        matches!(refused, Err(ArchonError::Invalid(_))),
        "{refused:?}"
    );
}
