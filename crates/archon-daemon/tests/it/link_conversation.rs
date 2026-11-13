//! One operator conversation: archond's posts land in the orchestrator's
//! transcript in feed order, and `conversation` reads back exactly the
//! important entries Aether's TUI `/focus` shows, for `archon chat` and the phone.

use std::sync::Arc;

use crate::link_common::{self, Harness, WAIT};

use archon_aether::events::parse_event;
use archon_aether::fake::Scripted;
use archon_core::conversation::{ConversationEntry, ConversationPage, EntryKind};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, DeviceRole, Reply, Request, WelcomeDevice};
use archon_daemon::{handlers, posts, pump};
use archon_queue::store::keys;
use serde_json::{Value, json};

const ORCH: &str = "agt_orch";

async fn harness() -> Harness {
    harness_serving(&["notes", "conversation"]).await
}

async fn harness_serving(features: &[&str]) -> Harness {
    let harness = Harness::new().await;
    let features = features.iter().map(|f| (*f).to_owned()).collect();
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

/// An aetherd without posts holds them back: nothing is skipped, and the
/// cursor stays before them until an aetherd that keeps posts comes.
#[tokio::test]
async fn posts_wait_for_an_aetherd_that_keeps_them() {
    let harness = harness_serving(&["notes"]).await;
    let before = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    let poster = start_posting(&harness).await;
    notify(&harness, NotificationKind::Blocker, "CI is red", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(appended(&harness).is_empty(), "{:?}", appended(&harness));
    assert_eq!(
        harness.kv(posts::POSTED_KEY).await,
        Some(before.to_string())
    );
    poster.abort();
}

/// A post Aether refuses as invalid is skipped, so it cannot hold the rest.
#[tokio::test]
async fn a_refused_post_does_not_hold_back_the_next() {
    let harness = harness().await;
    harness
        .fake
        .script("append_note", Scripted::refuse("invalid_argument", "no"));
    let poster = start_posting(&harness).await;
    notify(&harness, NotificationKind::Decision, "first", None).await;
    notify(&harness, NotificationKind::Decision, "second", None).await;
    harness
        .eventually("both tried", || async { appended(&harness).len() == 2 })
        .await;
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness
        .eventually("the cursor passes both", || async {
            harness.kv(posts::POSTED_KEY).await == Some(last.to_string())
        })
        .await;
    poster.abort();
}

fn operator_says(seq: u64, text: &str) -> archon_aether::AetherEvent {
    let kind = json!({"kind": "message", "message_id": "msg_1", "from": {"actor": "user"},
                      "content": [{"type": "text", "text": text}]});
    parse_event(json!({
        "id": format!("evt_{seq}"), "seq": seq, "at": "2026-10-12T10:00:00Z",
        "actor": {"type": "system"}, "agent_id": ORCH,
        "kind": {"type": "inbox.delivered", "item": {
            "id": format!("inb_{seq}"), "agent_id": ORCH, "seq": seq,
            "delivered_at": "2026-10-12T10:00:00Z", "kind": kind}}
    }))
    .expect("a valid event")
}

/// A command's result goes through the feed, so it is posted once and in
/// order, with no second copy of its audit line.
#[tokio::test]
async fn a_command_result_reaches_the_conversation_once() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    pump::handle_orchestrator_event(&harness.ctx, &operator_says(1, "/answer 99 hi"))
        .await
        .expect("the pump handles the event");
    harness
        .eventually("the result post", || async {
            !appended(&harness).is_empty()
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let posts = appended(&harness);
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (text, important) = &posts[0];
    assert!(text.starts_with("**Archon:** Refused: /answer"), "{text}");
    assert!(important);
    poster.abort();
}

async fn ask_ship(harness: &Harness) -> u64 {
    let ask = Request::AskOperator {
        text: "Ship to prod?".into(),
        item: None,
        options: vec!["yes".into(), "no".into()],
        caller: None,
    };
    let Ok(Reply::Question(asked)) = request(harness, ask).await else {
        panic!("the question is recorded")
    };
    asked.number
}

/// `/answer` typed in the phone's chat, as an operator device sends it.
async fn answer_from_the_phone(harness: &Harness, number: u64) {
    let phone = WelcomeDevice {
        id: "dev_phone".into(),
        name: "phone".into(),
        role: DeviceRole::Operator,
    };
    let say = Request::Say {
        text: format!("/answer {number} yes"),
        images: Vec::new(),
    };
    let reply = handlers::handle_device(&harness.ctx, &phone, say).await;
    assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
}

/// `/answer` typed in `archon chat`, which sends the expanded answer.
async fn answer_from_archon_chat(harness: &Harness, number: u64) {
    let open = harness.ctx.store.lock().await.questions(true).unwrap();
    let asked = open.into_iter().find(|q| q.number == number).unwrap();
    let answer = Request::Answer {
        question: asked.id,
        text: "yes".into(),
        chat: true,
    };
    let reply = request(harness, answer).await;
    assert!(matches!(reply, Ok(Reply::Question(_))), "{reply:?}");
}

/// The posts of one question, answered `yes`: asked, answered, applied.
fn answered_posts(n: u64) -> Vec<(String, bool)> {
    let asked = format!(
        "**Q{n}:** Ship to prod?\n1. yes\n2. no\n\nAnswer with `/answer {n} …` or `archon answer {n} …`."
    );
    vec![
        (asked, true),
        (format!("**Answered Q{n}:** yes"), true),
        (format!("**Archon:** Applied: /answer Q{n}: yes"), true),
    ]
}

/// An answer from the phone and one from `archon chat` post the same:
/// exactly one "Answered" and one "Applied" each, in the same order.
#[tokio::test]
async fn a_phone_and_an_archon_chat_answer_post_the_same() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    let phone = ask_ship(&harness).await;
    answer_from_the_phone(&harness, phone).await;
    let chat = ask_ship(&harness).await;
    answer_from_archon_chat(&harness, chat).await;
    harness
        .eventually("both answers", || async { appended(&harness).len() >= 5 })
        .await;
    // Room for a missing or doubled post to show in the comparison.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let mut want = answered_posts(phone);
    want.extend(answered_posts(chat));
    assert_eq!(appended(&harness), want);
    poster.abort();
}
