//! One operator conversation: archond's posts land in the orchestrator's
//! transcript in feed order, and `conversation` reads back exactly the
//! important entries Aether's TUI `/focus` shows, for `archon chat` and the phone.

use std::sync::Arc;

use crate::link_common::{self, Harness, WAIT};

use archon_aether::events::parse_event;
use archon_aether::fake::Scripted;
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef};
use archon_core::conversation::{ConversationEntry, ConversationPage, EntryKind};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, DeviceRole, Reply, Request, WelcomeDevice};
use archon_daemon::{handlers, posts, pump, supervisor};
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

/// `/answer` typed in `archon chat`, which sends it as an operator command.
async fn answer_from_archon_chat(harness: &Harness, number: u64) {
    let answer = Request::OperatorCommand {
        text: format!("/answer {number} yes"),
    };
    let reply = request(harness, answer).await;
    assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
}

/// The posts of one question, answered `yes`: asked, the operator's
/// command, answered, applied.
fn answered_posts(n: u64) -> Vec<(String, bool)> {
    let asked = format!(
        "**Q{n}:** Ship to prod?\n1. yes\n2. no\n\nAnswer with `/answer {n} …` or `archon answer {n} …`."
    );
    vec![
        (asked, true),
        (format!("**Operator:** /answer {n} yes"), true),
        (format!("**Answered Q{n}:** yes"), true),
        (format!("**Archon:** Applied: /answer Q{n}: yes"), true),
    ]
}

/// An answer from the phone and one from `archon chat` post the same: the
/// command, which archond runs itself, then one "Answered" and one "Applied".
#[tokio::test]
async fn a_phone_and_an_archon_chat_answer_post_the_same() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    let phone = ask_ship(&harness).await;
    answer_from_the_phone(&harness, phone).await;
    let chat = ask_ship(&harness).await;
    answer_from_archon_chat(&harness, chat).await;
    harness
        .eventually("both answers", || async { appended(&harness).len() >= 8 })
        .await;
    // Room for a missing or doubled post to show in the comparison.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let mut want = answered_posts(phone);
    want.extend(answered_posts(chat));
    assert_eq!(appended(&harness), want);
    poster.abort();
}

/// A command sent over the operator socket runs in archond and never reaches
/// Aether, so archond posts it once, as the operator's line, before its result.
#[tokio::test]
async fn a_socket_command_reaches_the_conversation_once() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    let command = Request::OperatorCommand {
        text: "/answer 99 hi".into(),
    };
    let reply = request(&harness, command).await;
    assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
    harness
        .eventually("the result post", || async {
            appended(&harness).len() >= 2
        })
        .await;
    // Room for a doubled post to show.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let posts = appended(&harness);
    assert_eq!(posts.len(), 2, "{posts:?}");
    assert_eq!(posts[0], ("**Operator:** /answer 99 hi".to_owned(), true));
    assert!(
        posts[1].0.starts_with("**Archon:** Refused: /answer"),
        "{posts:?}"
    );
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    assert_eq!(harness.kv(posts::POSTED_KEY).await, Some(last.to_string()));
    poster.abort();
}

async fn ask_user(harness: &Harness, agent: &str, text: &str) -> u64 {
    let aether = AetherQuestionRef {
        agent: AetherAgentId::parse(agent).unwrap(),
        seq: 7,
    };
    let asked = archon_daemon::feed::aether_question(&harness.ctx, aether, text, None)
        .await
        .expect("the question is recorded")
        .expect("a first ask");
    asked.number
}

/// The orchestrator's own `ask_user` is in its transcript already, so it is
/// not posted again; a stage agent's is not, so it is posted once, numbered.
#[tokio::test]
async fn only_a_stage_agents_ask_user_is_posted() {
    let harness = harness().await;
    let poster = start_posting(&harness).await;
    ask_user(&harness, ORCH, "Which repo?").await;
    let stage = ask_user(&harness, "agt_stage", "Keep the old API?").await;
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness
        .eventually("the cursor passes both", || async {
            harness.kv(posts::POSTED_KEY).await == Some(last.to_string())
        })
        .await;
    // Room for a doubled post to show.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let asked = format!(
        "**Q{stage}:** Keep the old API?\n\nAnswer with `/answer {stage} …` or `archon answer {stage} …`."
    );
    assert_eq!(appended(&harness), [(asked, true)]);
    poster.abort();
}

/// The orchestrator is replaced while a post is on its way to it, after the
/// poster read which one is current: the successor gets the post too before
/// the cursor passes it, a repeat on the retired transcript but no loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_made_during_a_replacement_reaches_the_successor() {
    const SUCCESSOR: &str = "agt_successor";
    let harness = harness().await;
    let ctx = Arc::clone(&harness.ctx);
    let replace = move |note: &Value| {
        if note["agent"] == ORCH {
            // The fake answers synchronously; the append's reply waits for this.
            tokio::task::block_in_place(|| {
                let mut store = ctx.store.blocking_lock();
                store.kv_set(keys::ORCHESTRATOR_AGENT, SUCCESSOR).unwrap();
            });
        }
        Scripted::reply("note_appended", json!({"seq": 1}))
    };
    harness.fake.set_responder("append_note", Arc::new(replace));
    let poster = start_posting(&harness).await;
    notify(&harness, NotificationKind::Blocker, "CI is red", None).await;
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness
        .eventually("the cursor passes the post", || async {
            harness.kv(posts::POSTED_KEY).await == Some(last.to_string())
        })
        .await;
    let notes = harness.fake.received_named("append_note");
    let to: Vec<_> = notes.iter().map(|n| n["agent"].as_str().unwrap()).collect();
    assert_eq!(to, [ORCH, SUCCESSOR], "{notes:?}");
    assert!(
        notes.iter().all(|n| n["text"] == "**Blocker**: CI is red"),
        "{notes:?}"
    );
    poster.abort();
}

/// A switch under way once the post is appended, the stored orchestrator
/// still the old one: the cursor waits for it to finish, then the successor
/// gets the post too, so no switch can fall between the check and the cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_under_way_after_the_append_holds_the_cursor_until_the_successor_has_it() {
    const SUCCESSOR: &str = "agt_successor";
    let harness = harness().await;
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    // Taken by the first append, to the old orchestrator.
    let first = std::sync::Mutex::new(Some((held_tx, held_rx, release_rx)));
    let ctx = Arc::clone(&harness.ctx);
    let switch = move |note: &Value| {
        let once = (note["agent"] == ORCH).then(|| first.lock().unwrap().take());
        if let Some((held_tx, held_rx, release)) = once.flatten() {
            let ctx = Arc::clone(&ctx);
            tokio::spawn(async move {
                let _switch = supervisor::hold_switch(&ctx).await;
                held_tx.send(()).unwrap();
                release.await.unwrap();
                let mut store = ctx.store.lock().await;
                store.kv_set(keys::ORCHESTRATOR_AGENT, SUCCESSOR).unwrap();
            });
            // The append is answered only once the switch holds.
            tokio::task::block_in_place(|| held_rx.recv().unwrap());
        }
        Scripted::reply("note_appended", json!({"seq": 1}))
    };
    harness.fake.set_responder("append_note", Arc::new(switch));
    let before = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    let poster = start_posting(&harness).await;
    notify(&harness, NotificationKind::Blocker, "CI is red", None).await;
    harness
        .eventually("the first append", || async {
            !harness.fake.received_named("append_note").is_empty()
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let posted = harness.kv(posts::POSTED_KEY).await;
    assert_eq!(
        posted,
        Some(before.to_string()),
        "the cursor waits for the switch"
    );
    release_tx.send(()).unwrap();
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness
        .eventually("the cursor passes the post", || async {
            harness.kv(posts::POSTED_KEY).await == Some(last.to_string())
        })
        .await;
    let notes = harness.fake.received_named("append_note");
    let to: Vec<_> = notes.iter().map(|n| n["agent"].as_str().unwrap()).collect();
    assert_eq!(to, [ORCH, SUCCESSOR], "{notes:?}");
    poster.abort();
}

/// The orchestrator's own `ask_user` arrives while a switch is under way: the
/// skip waits for it, and the successor, whose transcript lacks the question,
/// gets it posted before the cursor passes it.
#[tokio::test]
async fn an_orchestrators_question_during_a_replacement_reaches_the_successor() {
    const SUCCESSOR: &str = "agt_successor";
    let harness = harness().await;
    let before = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    let poster = start_posting(&harness).await;
    let switch = supervisor::hold_switch(&harness.ctx).await;
    let number = ask_user(&harness, ORCH, "Which repo?").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        harness.kv(posts::POSTED_KEY).await,
        Some(before.to_string()),
        "the skip waits for the switch"
    );
    harness.set_kv(keys::ORCHESTRATOR_AGENT, SUCCESSOR).await;
    drop(switch);
    let last = harness.ctx.store.lock().await.last_feed_seq().unwrap();
    harness
        .eventually("the cursor passes the question", || async {
            harness.kv(posts::POSTED_KEY).await == Some(last.to_string())
        })
        .await;
    let notes = harness.fake.received_named("append_note");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["agent"], SUCCESSOR);
    let asked = format!("**Q{number}:** Which repo?");
    assert!(
        notes[0]["text"].as_str().unwrap().starts_with(&asked),
        "{notes:?}"
    );
    poster.abort();
}
