//! One operator conversation: archond's posts land in the orchestrator's
//! transcript in feed order, and `conversation` reads back exactly the
//! important entries Aether's TUI `/focus` shows, for `archon chat` and the phone.

use std::sync::Arc;

use crate::link_common::{self, Harness, WAIT};

use archon_aether::fake::Scripted;
use archon_core::conversation::{ConversationEntry, ConversationPage, EntryKind};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::posts;
use archon_queue::store::keys;
use serde_json::{Value, json};

const ORCH: &str = "agt_orch";

async fn harness() -> Harness {
    let harness = Harness::new().await;
    let features = ["notes", "conversation"].map(str::to_owned).to_vec();
    harness.fake.set_features(features);
    harness.fake.set_default(
        "append_note",
        Scripted::reply("note_appended", json!({"seq": 1})),
    );
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness
}

/// The poster, from the feed's current end on.
async fn start_posting(harness: &Harness) -> tokio::task::JoinHandle<Result<(), ArchonError>> {
    let end = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness.set_kv(posts::POSTED_KEY, &end.to_string()).await;
    tokio::spawn(posts::run(Arc::clone(&harness.ctx)))
}

async fn request(harness: &Harness, request: Request) -> Result<Reply, ArchonError> {
    archon_daemon::handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await
}

async fn notify(harness: &Harness, kind: NotificationKind, text: &str, item: Option<QueueItemId>) {
    let reply = request(
        harness,
        Request::Notify {
            kind,
            text: text.into(),
            item,
        },
    )
    .await;
    assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
}

/// The posts archond appended: text, and whether each was important.
fn appended(harness: &Harness) -> Vec<(String, bool)> {
    let notes = harness.fake.received_named("append_note");
    let post = |n: &Value| {
        assert_eq!(n["agent"], ORCH, "{n}");
        let important = n.get("important").and_then(Value::as_bool).unwrap_or(false);
        (n["text"].as_str().unwrap().to_owned(), important)
    };
    notes.iter().map(post).collect()
}

fn entry(seq: u64, kind: EntryKind, text: &str) -> ConversationEntry {
    ConversationEntry {
        seq,
        part: 0,
        at: archon_core::now(),
        kind,
        important: true,
        text: text.into(),
        links: Vec::new(),
    }
}

fn page(entries: Vec<ConversationEntry>, to_seq: u64) -> ConversationPage {
    ConversationPage {
        from_seq: entries.first().map_or(to_seq, |e| e.seq),
        to_seq,
        last_seq: to_seq,
        entries,
        ..ConversationPage::default()
    }
}

fn scripted(page: &ConversationPage) -> Scripted {
    Scripted::reply("conversation", serde_json::to_value(page).unwrap())
}

async fn read(harness: &Harness, after: Option<u64>, wait: bool) -> ConversationPage {
    let ask = Request::Conversation {
        after,
        before: None,
        limit: after.is_none().then_some(200),
        wait,
    };
    match request(harness, ask).await {
        Ok(Reply::Conversation(page)) => page,
        other => panic!("not a page: {other:?}"),
    }
}

/// A mixed turn: text between tool calls, a notify and an item outcome. The
/// posts reach the transcript in order, important but progress, and the
/// chat's read is exactly Aether's important entries, nothing added or lost.
#[tokio::test]
async fn a_mixed_turn_reads_back_as_the_focused_view_shows_it() {
    let harness = harness().await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let poster = start_posting(&harness).await;
    notify(
        &harness,
        NotificationKind::Decision,
        "build runs on opus",
        None,
    )
    .await;
    notify(&harness, NotificationKind::Progress, "review started", None).await;
    notify(
        &harness,
        NotificationKind::Outcome,
        "PR #7 merged",
        Some(item.clone()),
    )
    .await;
    harness
        .eventually("three posts", || async { appended(&harness).len() == 3 })
        .await;
    let decision = "**Decision**: build runs on opus".to_owned();
    let outcome = format!("**Outcome** ({item}): PR #7 merged");
    let progress = "**Progress**: review started".to_owned();
    let want = [
        (decision.clone(), true),
        (progress, false),
        (outcome.clone(), true),
    ];
    assert_eq!(appended(&harness), want);
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    assert_eq!(harness.kv(posts::POSTED_KEY).await, Some(last.to_string()));

    let focused = vec![
        entry(10, EntryKind::Operator, "add caching to api"),
        entry(11, EntryKind::Assistant, "Looking at the api repo first."),
        entry(14, EntryKind::Assistant, "Queued it on api."),
        entry(15, EntryKind::Post, &decision),
        entry(19, EntryKind::Post, &outcome),
    ];
    harness
        .fake
        .script("get_conversation", scripted(&page(focused.clone(), 19)));
    let read_back = read(&harness, None, false).await;
    assert_eq!(read_back.entries, focused, "1 to 1 with the focused view");
    let asked = harness.fake.received_named("get_conversation");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["agent"], ORCH);
    assert_eq!(asked[0]["important_only"], true, "{}", asked[0]);
    assert_eq!(asked[0]["limit"], 200);
    poster.abort();
}

/// A waiting read holds until the conversation grows, then returns what
/// followed the cursor; with nothing new it ends empty at the cursor.
#[tokio::test]
async fn a_waiting_read_returns_what_follows_once_the_conversation_grows() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    harness
        .fake
        .script("get_conversation", scripted(&page(Vec::new(), 0)));
    let next = vec![entry(21, EntryKind::Post, "**Blocker**: CI is red")];
    harness
        .fake
        .script("get_conversation", scripted(&page(next.clone(), 21)));
    let waiting = {
        let harness = &harness;
        async move { read(harness, Some(20), true).await }
    };
    let grow = async {
        harness
            .eventually("the first read", || async {
                !harness.fake.received_named("get_conversation").is_empty()
            })
            .await;
        notify(&harness, NotificationKind::Blocker, "CI is red", None).await;
    };
    let (read_back, ()) = tokio::time::timeout(WAIT, async { tokio::join!(waiting, grow) })
        .await
        .expect("the post wakes the waiting read");
    assert_eq!(read_back.entries, next);
    assert_eq!(read_back.to_seq, 21);
    let asked = harness.fake.received_named("get_conversation");
    assert_eq!(
        asked.len(),
        2,
        "one read, then one after the wake: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .all(|a| a["after"] == 20 && a["important_only"] == true)
    );
    poster.abort();
}

#[tokio::test]
async fn a_read_refuses_a_wait_without_a_cursor_and_an_oversized_page() {
    let harness = harness().await;
    for ask in [
        Request::Conversation {
            after: None,
            before: None,
            limit: None,
            wait: true,
        },
        Request::Conversation {
            after: None,
            before: None,
            limit: Some(1001),
            wait: false,
        },
    ] {
        let refused = request(&harness, ask).await;
        assert!(
            matches!(refused, Err(ArchonError::Invalid(_))),
            "{refused:?}"
        );
    }
    assert!(harness.fake.received_named("get_conversation").is_empty());
}
