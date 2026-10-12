//! The orchestrator supervisor against the fake aetherd: creation keys,
//! reuse, wake, restart, adopting successors, revival backoff, and
//! operator messages.

mod link_common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::aether_ids::AetherQuestionRef;
use archon_core::error::ArchonError;
use archon_core::id::{FeedItemId, QuestionId};
use archon_core::notify::{AnswerVia, FeedEntry, NotificationKind, Question};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::triggers::TriggerEvent;
use archon_daemon::{handlers, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, WAIT, agent_id};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";

fn agent_in(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// How far ahead the deferred revival is scheduled.
fn next_revival_in(harness: &Harness) -> Duration {
    let at = harness
        .ctx
        .supervisor
        .next_revival()
        .expect("a revival is scheduled");
    (at - archon_core::now()).to_std().unwrap_or_default()
}

/// `get_agent` answers from a table the test edits as agents change state.
fn agent_table(harness: &Harness, initial: &[(&str, &str)]) -> Arc<Mutex<HashMap<String, String>>> {
    let states: HashMap<String, String> = initial
        .iter()
        .map(|(id, state)| ((*id).to_owned(), (*state).to_owned()))
        .collect();
    let states = Arc::new(Mutex::new(states));
    let table = states.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            match table.lock().unwrap().get(id) {
                Some(state) => Scripted::reply("agent", agent_in(id, state)),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    states
}

async fn create_key(harness: &Harness) -> String {
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH, "ready")),
    );
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let creates = harness.fake.received_named("create_agent");
    creates.last().unwrap()["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_first_ensure_creates_the_orchestrator_under_a_store_scoped_key() {
    let harness = Harness::new().await;
    harness.connect().await;
    let mut watch = harness.ctx.supervisor.watch();
    let key = create_key(&harness).await;
    let store = supervisor::store_id(&harness.ctx).await.unwrap();
    assert_eq!(key, format!("archon-orchestrator-{store}-1"));
    assert_eq!(*watch.borrow_and_update(), Some(agent_id(ORCH)));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert!(
        harness.system_notes().await.is_empty(),
        "a first start is not news"
    );
}

#[tokio::test]
async fn fresh_stores_never_reuse_an_orchestrator_key() {
    let first = Harness::new().await;
    first.connect().await;
    let second = Harness::new().await;
    second.connect().await;
    let (a, b) = (create_key(&first).await, create_key(&second).await);
    assert!(a.ends_with("-1") && b.ends_with("-1"), "{a} {b}");
    assert_ne!(a, b, "a fresh store must not get the old orchestrator back");
}

#[tokio::test]
async fn a_live_stored_orchestrator_is_reused_and_announced_to_the_pump() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "waiting")]);
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH));
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        *harness.ctx.supervisor.watch().borrow(),
        Some(agent_id(ORCH))
    );
}

#[tokio::test]
async fn a_failed_orchestrator_is_woken_and_a_cancelled_one_restarted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = agent_table(&harness, &[(ORCH, "failed")]);
    harness
        .fake
        .script("wake", Scripted::reply("agent", agent_in(ORCH, "ready")));
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        agent_id(ORCH)
    );
    assert_eq!(harness.fake.received_named("wake")[0]["agent"], ORCH);
    // It ends a turn well, so the next stop is revived at once.
    supervisor::turn_ended(&harness.ctx, Some("ready"))
        .await
        .unwrap();

    states
        .lock()
        .unwrap()
        .insert(ORCH.into(), "cancelled".into());
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].contains("woken") && notes[0].contains("failed"),
        "{notes:?}"
    );
    assert!(
        notes[1].contains("restarted") && notes[1].contains(ORCH_2),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_refused_wake_falls_back_to_restart() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "failed")]);
    harness.fake.script(
        "wake",
        Scripted::refuse("invalid_transition", "not wakeable"),
    );
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(harness.fake.received_named("restart").len(), 1);
}

#[tokio::test]
async fn a_successor_left_by_restart_is_followed_not_restarted_again() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    harness
        .fake
        .script("get_agent", Scripted::reply("agent", old));
    harness.fake.script(
        "get_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "running")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
}

#[tokio::test]
async fn an_orchestrator_aether_forgot_is_recreated_in_a_new_generation() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    agent_table(&harness, &[]);
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    let key = harness.fake.received_named("create_agent")[0]["idempotency_key"].clone();
    assert!(key.as_str().unwrap().ends_with("-2"), "{key}");
    assert!(harness.system_notes().await[0].contains("vanished"));
}

/// An orchestrator stuck `failed` by a transient engine error.
fn failing_orchestrator(harness: &Harness) -> Arc<Mutex<HashMap<String, String>>> {
    let states = agent_table(harness, &[]);
    let table = states.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            let Some(state) = table.lock().unwrap().get(id).cloned() else {
                return Scripted::refuse("not_found", "no such agent");
            };
            let mut agent = agent_in(id, &state);
            if state == "failed" {
                agent["status"]["error"] = json!("Failed to refresh OAuth token");
            }
            Scripted::reply("agent", agent)
        }),
    );
    states.lock().unwrap().insert(ORCH.into(), "failed".into());
    harness.fake.set_responder(
        "wake",
        Arc::new(|cmd| {
            let id = cmd["agent"].as_str().unwrap_or_default();
            Scripted::reply("agent", agent_in(id, "ready"))
        }),
    );
    states
}

async fn deferred(harness: &Harness) -> String {
    let err = supervisor::ensure_orchestrator(&harness.ctx)
        .await
        .unwrap_err();
    let ArchonError::Unavailable(reason) = err else {
        panic!("a deferred revival is unavailable, got {err}");
    };
    reason
}

#[tokio::test]
async fn a_failing_orchestrator_is_revived_with_growing_pauses_and_never_given_up() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = failing_orchestrator(&harness);
    let wakes = || harness.fake.received_named("wake").len();

    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(wakes(), 1, "the first revival of a streak is immediate");
    for (revival, pause) in supervisor::REVIVAL_BACKOFF.iter().take(4).enumerate() {
        let reason = deferred(&harness).await;
        assert!(reason.contains("archon orchestrator restart"), "{reason}");
        assert_eq!(wakes(), revival + 1, "nothing is woken before it is due");
        let ahead = next_revival_in(&harness);
        assert!(
            ahead <= *pause && ahead + Duration::from_secs(2) > *pause,
            "{ahead:?}"
        );
        harness.rewind_backoff(pause.as_secs_f64()).await;
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
        assert_eq!(wakes(), revival + 2);
    }
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "told once per streak: {blockers:?}");
    assert!(blockers[0].contains("OAuth token"), "{blockers:?}");
    let woken = harness.system_notes().await;
    assert_eq!(woken.len(), 2, "quiet once the Blocker is up: {woken:?}");

    // The transient error clears: a good turn ends the streak.
    states.lock().unwrap().insert(ORCH.into(), "ready".into());
    supervisor::turn_ended(&harness.ctx, Some("ready"))
        .await
        .unwrap();
    assert!(
        harness
            .system_notes()
            .await
            .last()
            .unwrap()
            .contains("working again")
    );
    assert_eq!(harness.ctx.supervisor.next_revival(), None);
    states.lock().unwrap().insert(ORCH.into(), "failed".into());
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(wakes(), 6, "a new streak starts without a pause");
}

#[tokio::test]
async fn a_failed_turn_does_not_end_the_streak() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    supervisor::turn_ended(&harness.ctx, Some("failed"))
        .await
        .unwrap();
    supervisor::turn_ended(&harness.ctx, None).await.unwrap();
    deferred(&harness).await;
    assert_eq!(harness.fake.received_named("wake").len(), 1);
}

#[tokio::test]
async fn a_deferred_revival_runs_by_itself_when_due() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = failing_orchestrator(&harness);
    let revivals = tokio::spawn(supervisor::revive_when_due(harness.ctx.clone()));
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    // Due in about 300ms rather than 10s.
    harness.rewind_backoff(9.7).await;
    deferred(&harness).await;
    let woken = states.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| {
            woken.lock().unwrap().insert(ORCH.into(), "ready".into());
            Scripted::reply("agent", agent_in(ORCH, "ready"))
        }),
    );
    harness.fake.wait_for("wake", 2, WAIT).await;
    harness
        .eventually("the revival is no longer pending", || async {
            harness.ctx.supervisor.next_revival().is_none()
        })
        .await;
    revivals.abort();
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        agent_id(ORCH)
    );
}

#[tokio::test]
async fn an_operator_restart_overrides_the_backoff() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    // A store left behind by the old give-up rule.
    harness.set_kv("orchestrator_gave_up", "1").await;
    let states = failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    deferred(&harness).await;

    harness.fake.script("cancel", Scripted::ok());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    states.lock().unwrap().insert(ORCH_2.into(), "ready".into());
    // Through the operator's request, as `archon orchestrator restart` sends it.
    let reply = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        Request::OrchestratorRestart,
    )
    .await
    .unwrap();
    let Reply::Orchestrator(info) = reply else {
        panic!("expected the orchestrator, got {reply:?}");
    };
    assert_eq!(info.agent, agent_id(ORCH_2));
    assert_eq!(harness.fake.received_named("cancel")[0]["agent"], ORCH);
    assert_eq!(harness.ctx.supervisor.next_revival(), None);
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        info.agent
    );
    states
        .lock()
        .unwrap()
        .insert(ORCH_2.into(), "failed".into());
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(
        harness.fake.received_named("wake").last().unwrap()["agent"],
        ORCH_2,
        "the fresh generation starts a fresh streak"
    );
}

#[tokio::test]
async fn a_restart_made_in_aether_while_archond_was_down_is_adopted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    harness.set_kv("orchestrator_gave_up", "1").await;
    harness
        .set_kv("orchestrator_blocker", "The orchestrator stopped.")
        .await;
    let mut old = agent_in(ORCH, "failed");
    old["restarted_as"] = json!(ORCH_2);
    let successor = agent_in(ORCH_2, "ready");
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| match cmd["id"].as_str() {
            Some(ORCH) => Scripted::reply("agent", old.clone()),
            _ => Scripted::reply("agent", successor.clone()),
        }),
    );
    let mut watch = harness.ctx.supervisor.watch();
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(*watch.borrow_and_update(), Some(agent_id(ORCH_2)));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("1"),
        "a restart continues the generation's conversation"
    );
    for cmd in ["wake", "restart", "create_agent"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(ORCH) && notes[0].contains(ORCH_2),
        "{notes:?}"
    );
    assert_eq!(
        harness.kv("orchestrator_blocker").await,
        None,
        "adoption deletes the blocker rather than blanking it"
    );
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        id
    );
    assert_eq!(harness.system_notes().await.len(), 1, "adopted once");
}

#[tokio::test]
async fn a_successor_that_is_not_an_orchestrator_is_never_adopted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    let stranger = fake::agent(ORCH_2, None);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| match cmd["id"].as_str() {
            Some(ORCH) => Scripted::reply("agent", old.clone()),
            _ => Scripted::reply("agent", stranger.clone()),
        }),
    );
    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id(ORCH_2))
        .await
        .unwrap();
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert!(harness.system_notes().await[0].contains("did not adopt"));

    harness.fake.script(
        "restart",
        Scripted::refuse("invalid_argument", "already restarted"),
    );
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in("agt_0003", "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id("agt_0003"), "a fresh generation instead");
}

#[tokio::test]
async fn a_restart_of_an_agent_that_is_no_longer_the_orchestrator_is_ignored() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id("agt_0003"))
        .await
        .unwrap();
    assert!(harness.fake.received_named("get_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
}

#[tokio::test]
async fn an_operator_message_is_keyed_by_its_feed_item() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "running")]);
    harness
        .fake
        .script("send_message", Scripted::reply("message_sent", json!({})));
    let item = FeedItemId::new();
    supervisor::deliver_operator_message(&harness.ctx, &item, "status?")
        .await
        .unwrap();
    let sent = &harness.fake.received_named("send_message")[0];
    assert_eq!(sent["idempotency_key"], item.as_str());
    assert_eq!(sent["to"], ORCH);
}

#[tokio::test]
async fn a_refused_operator_message_does_not_leave_the_chat_waiting() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "running")]);
    harness.fake.script(
        "send_message",
        Scripted::refuse("invalid_argument", "too long"),
    );
    let err = supervisor::deliver_operator_message(&harness.ctx, &FeedItemId::new(), "hi")
        .await
        .unwrap_err();
    assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
    assert!(!harness.ctx.replies.lock().await.awaiting_reply());
    assert_eq!(harness.kv(archon_daemon::feed::REPLY_STATE_KEY).await, None);
}

#[tokio::test]
async fn answers_go_to_aether_or_to_the_orchestrator_by_question_origin() {
    let mut harness = Harness::new().await;
    harness.connect().await;
    let aether = AetherQuestionRef {
        agent: agent_id(ORCH_2),
        seq: 17,
    };
    let from_aether = archon_daemon::feed::aether_question(&harness.ctx, aether, "Which?", None)
        .await
        .unwrap()
        .unwrap();
    let from_archon: Question =
        archon_daemon::feed::question(&harness.ctx, "Merge?".into(), vec!["yes".into()], None)
            .await
            .unwrap();
    harness.fake.script(
        "reply_to_question",
        Scripted::reply("message_sent", json!({})),
    );
    supervisor::route_answer(&harness.ctx, &from_aether, "the first", AnswerVia::Archon)
        .await
        .unwrap();
    supervisor::route_answer(&harness.ctx, &from_archon, "yes", AnswerVia::Archon)
        .await
        .unwrap();
    let replied = &harness.fake.received_named("reply_to_question")[0];
    assert_eq!(
        (replied["to"].as_str(), replied["question_seq"].as_u64()),
        (Some(ORCH_2), Some(17))
    );
    let answer = TriggerEvent::OperatorAnswer {
        question: from_archon.id.clone(),
        text: "yes".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![answer]);
}

async fn answer(
    harness: &Harness,
    question: &QuestionId,
    text: &str,
) -> Result<Reply, ArchonError> {
    let request = Request::Answer {
        question: question.clone(),
        text: text.into(),
    };
    handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await
}

async fn answers_in_feed(harness: &Harness) -> Vec<String> {
    let feed = harness.feed().await;
    feed.into_iter()
        .filter_map(|entry| match entry {
            FeedEntry::Answer { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

/// An answer given while Aether is unreachable is not recorded,
/// so answering again once the link is back delivers it.
#[tokio::test]
async fn an_undelivered_answer_leaves_the_question_open_for_another_try() {
    let harness = Harness::new().await;
    let aether = AetherQuestionRef {
        agent: agent_id(ORCH),
        seq: 9,
    };
    let asked = archon_daemon::feed::aether_question(&harness.ctx, aether, "Which?", None)
        .await
        .unwrap()
        .expect("a new question");

    let refused = answer(&harness, &asked.id, "the first").await.unwrap_err();
    assert!(matches!(refused, ArchonError::Unavailable(_)), "{refused}");
    let still = harness.ctx.store.lock().await.question(&asked.id).unwrap();
    assert!(still.is_open(), "an undelivered answer is not recorded");
    assert!(answers_in_feed(&harness).await.is_empty());

    harness.connect().await;
    harness.fake.script(
        "reply_to_question",
        Scripted::reply("message_sent", json!({})),
    );
    let Reply::Question(answered) = answer(&harness, &asked.id, "the second").await.unwrap() else {
        panic!("expected the answered question");
    };
    assert_eq!(answered.answer.as_deref(), Some("the second"));
    let replies = harness.fake.received_named("reply_to_question");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["content"], "the second");
    assert_eq!(answers_in_feed(&harness).await, ["the second"]);
}

#[tokio::test]
async fn an_answered_or_unknown_question_is_refused_without_delivery() {
    let mut harness = Harness::new().await;
    let asked: Question =
        archon_daemon::feed::question(&harness.ctx, "Merge?".into(), vec!["yes".into()], None)
            .await
            .unwrap();
    answer(&harness, &asked.id, "yes").await.unwrap();
    let again = answer(&harness, &asked.id, "no").await.unwrap_err();
    assert!(
        matches!(again, ArchonError::InvalidTransition(_)),
        "{again}"
    );
    let unknown = answer(&harness, &QuestionId::new(), "x").await.unwrap_err();
    assert!(matches!(unknown, ArchonError::NotFound(_)), "{unknown}");

    let delivered = TriggerEvent::OperatorAnswer {
        question: asked.id.clone(),
        text: "yes".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![delivered]);
    assert_eq!(answers_in_feed(&harness).await, ["yes"]);
}

async fn attach(harness: &Harness) -> Result<Reply, ArchonError> {
    handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        Request::OrchestratorAttach,
    )
    .await
}

#[tokio::test]
async fn tui_attachment_creates_once_then_reuses_without_messages_or_workers() {
    let harness = Harness::new().await;
    harness.connect().await;
    agent_table(&harness, &[(ORCH, "ready")]);
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH, "ready")),
    );
    for _ in 0..2 {
        let Reply::Orchestrator(info) = attach(&harness).await.unwrap() else {
            panic!("expected an orchestrator snapshot");
        };
        assert_eq!(info.agent, agent_id(ORCH));
        assert_eq!(info.status, "ready");
    }
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0]["name"], "Archon");
    assert_eq!(creates[0]["role"], supervisor::ORCHESTRATOR_ROLE);
    assert!(creates[0]["parent"].is_null());
    for cmd in ["send_message", "restart", "wake"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
    assert!(harness.feed().await.is_empty());
}

#[tokio::test]
async fn tui_attachment_selects_the_successor_without_restarting_it() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            Scripted::reply(
                "agent",
                if cmd["id"] == ORCH {
                    old.clone()
                } else {
                    agent_in(ORCH_2, "running")
                },
            )
        }),
    );
    let Reply::Orchestrator(info) = attach(&harness).await.unwrap() else {
        panic!("expected the successor");
    };
    assert_eq!(info.agent, agent_id(ORCH_2));
    assert_eq!(info.status, "running");
    for cmd in ["restart", "create_agent", "send_message"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
}

#[tokio::test]
async fn tui_attachment_refuses_mcp_standby_and_disconnection_before_creation() {
    let harness = Harness::new().await;
    let denied = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonMcp,
        Request::OrchestratorAttach,
    )
    .await
    .unwrap_err();
    assert!(matches!(denied, ArchonError::Forbidden(_)), "{denied}");
    let unavailable = attach(&harness).await.unwrap_err();
    assert!(
        matches!(unavailable, ArchonError::Unavailable(_)),
        "{unavailable}"
    );
    harness.ctx.standby.set("disk full".into()).unwrap();
    let standby = attach(&harness).await.unwrap_err();
    assert!(matches!(standby, ArchonError::DiskFull(_)), "{standby}");
    assert!(harness.fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn tui_attachment_preserves_revival_backoff() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let error = attach(&harness).await.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error}");
    assert!(error.to_string().contains("archon orchestrator restart"));
    assert_eq!(harness.fake.received_named("wake").len(), 1);
    assert!(harness.fake.received_named("restart").is_empty());
}

#[tokio::test]
async fn tui_attachment_does_not_return_an_unknown_or_stopped_snapshot() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    for snapshot in [
        Scripted::refuse("not_found", "vanished during attachment"),
        Scripted::reply("agent", agent_in(ORCH, "cancelled")),
    ] {
        harness.fake.script(
            "get_agent",
            Scripted::reply("agent", agent_in(ORCH, "ready")),
        );
        harness.fake.script("get_agent", snapshot);
        let error = attach(&harness).await.unwrap_err();
        assert!(
            matches!(
                error,
                ArchonError::NotFound(_) | ArchonError::Unavailable(_)
            ),
            "{error}"
        );
    }
    assert!(harness.fake.received_named("create_agent").is_empty());
}
