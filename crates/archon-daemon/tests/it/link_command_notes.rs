//! A host command's result reaches the orchestrator's transcript as a note
//! the moment it is applied or refused, with no orchestrator turn.

use crate::link_common;

use archon_aether::AetherEvent;
use archon_aether::events::parse_event;
use archon_aether::fake::Scripted;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::pump;
use archon_queue::store::keys;
use link_common::Harness;
use serde_json::{Value, json};

const ORCH: &str = "agt_orch";

fn operator_says(seq: u64, text: &str) -> AetherEvent {
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

async fn harness(features: &[&str]) -> Harness {
    let harness = Harness::new().await;
    harness
        .fake
        .set_features(features.iter().map(|f| (*f).to_owned()).collect());
    harness.fake.script(
        "append_note",
        Scripted::reply("note_appended", json!({"seq": 1})),
    );
    harness.fake.script(
        "append_note",
        Scripted::reply("note_appended", json!({"seq": 2})),
    );
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness
}

async fn say(harness: &Harness, seq: u64, text: &str) {
    pump::handle_orchestrator_event(&harness.ctx, &operator_says(seq, text))
        .await
        .expect("the pump handles the event");
}

fn note_texts(harness: &Harness) -> Vec<String> {
    let notes = harness.fake.received_named("append_note");
    let texts = notes.iter().map(|n| n["text"].as_str().unwrap().to_owned());
    texts.collect()
}

#[tokio::test]
async fn an_applied_and_a_refused_answer_each_add_a_note_at_once() {
    let harness = harness(&["notes"]).await;
    let ask = Request::AskOperator {
        text: "Ship to prod?".into(),
        item: None,
        options: vec![],
        caller: None,
    };
    let reply = archon_daemon::handlers::handle(&harness.ctx, ClientKind::ArchonCli, ask).await;
    let Ok(Reply::Question(asked)) = reply else {
        panic!("asked: {reply:?}")
    };

    say(&harness, 1, &format!("/answer {} yes", asked.number)).await;
    let applied = note_texts(&harness);
    assert_eq!(applied.len(), 1, "one note for the applied answer");
    assert!(applied[0].starts_with("/answer"), "{applied:?}");
    let levels = |h: &Harness| -> Vec<Value> {
        let notes = h.fake.received_named("append_note");
        notes.iter().map(|n| n["level"].clone()).collect()
    };
    assert_eq!(levels(&harness), [json!("applied")]);

    say(&harness, 2, "/answer 99 hi").await;
    let all = note_texts(&harness);
    assert_eq!(all.len(), 2, "one more note for the refusal: {all:?}");
    assert!(all[1].starts_with("/answer:"), "{all:?}");
    assert_eq!(levels(&harness), [json!("applied"), json!("refused")]);
    let sent: Vec<Value> = harness.fake.received_named("send_message");
    assert!(sent.is_empty(), "no message woke the orchestrator");
    assert_eq!(harness.fake.received_named("append_note")[0]["agent"], ORCH);
}

#[tokio::test]
async fn an_aetherd_without_notes_gets_none_and_the_command_still_applies() {
    let harness = harness(&[]).await;
    say(&harness, 1, "/answer 99 hi").await;
    assert!(
        note_texts(&harness).is_empty(),
        "no note without the feature"
    );
    let held = archon_daemon::triggers::held(&harness.ctx).await.unwrap();
    assert_eq!(held.len(), 1, "the refusal still reaches the orchestrator");
}

#[tokio::test]
async fn a_paused_queue_still_shows_the_note_at_once() {
    let harness = harness(&["notes"]).await;
    let pause = Request::Pause(archon_core::protocol::PauseTarget::Global { hard: false });
    archon_daemon::handlers::handle(&harness.ctx, ClientKind::ArchonCli, pause)
        .await
        .expect("the queue pauses");

    say(&harness, 1, "/answer 99 hi").await;
    let notes = note_texts(&harness);
    assert_eq!(
        notes.len(),
        1,
        "the refusal is shown while paused: {notes:?}"
    );
    assert!(notes[0].starts_with("/answer:"), "{notes:?}");
}

#[tokio::test]
async fn a_second_command_mid_turn_is_shown_without_waiting_for_the_turn() {
    let harness = harness(&["notes"]).await;
    say(&harness, 1, "/answer 98 hi").await;
    say(&harness, 2, "/answer 99 hi").await;
    let notes = note_texts(&harness);
    assert_eq!(notes.len(), 2, "each command shows its own note: {notes:?}");
    assert!(harness.fake.received_named("send_message").is_empty());
}

#[tokio::test]
async fn a_protocol_24_aetherd_gets_the_level_and_no_word_prefix() {
    let harness = harness(&["notes"]).await;
    assert_eq!(
        harness
            .ctx
            .aether
            .client()
            .unwrap()
            .welcome()
            .protocol_version,
        24
    );

    say(&harness, 1, "/answer 99 hi").await;
    let sent = &harness.fake.received_named("append_note")[0];
    assert_eq!(sent["level"], "refused", "{sent}");
    assert!(
        !sent["text"].as_str().unwrap().starts_with("Refused:"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_read_command_is_answered_in_a_note_without_an_orchestrator_turn() {
    let harness = harness(&["notes"]).await;
    let ask = Request::AskOperator {
        text: "Ship to prod?".into(),
        item: None,
        options: vec![],
        caller: None,
    };
    archon_daemon::handlers::handle(&harness.ctx, ClientKind::ArchonCli, ask)
        .await
        .expect("asked");

    say(&harness, 1, "/status").await;
    say(&harness, 2, "/questions").await;
    let notes = note_texts(&harness);
    assert_eq!(notes.len(), 2, "one note per read: {notes:?}");
    assert!(notes[0].starts_with("/status: archond "), "{notes:?}");
    assert!(notes[0].contains("1 open question(s)"), "{notes:?}");
    assert!(notes[1].contains("Q1: Ship to prod?"), "{notes:?}");
    let levels: Vec<Value> = harness
        .fake
        .received_named("append_note")
        .iter()
        .map(|n| n["level"].clone())
        .collect();
    assert_eq!(
        levels,
        [Value::Null, Value::Null],
        "an answer is a plain note"
    );
    assert!(harness.fake.received_named("send_message").is_empty());
    let held = archon_daemon::triggers::held(&harness.ctx).await.unwrap();
    assert!(
        held.is_empty(),
        "nothing waits for the orchestrator: {held:?}"
    );
}

#[tokio::test]
async fn a_bad_read_is_refused_in_its_note_and_without_notes_reaches_the_orchestrator() {
    let with_notes = harness(&["notes"]).await;
    say(&with_notes, 1, "/queue nonsense").await;
    let notes = note_texts(&with_notes);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("unknown queue state"), "{notes:?}");
    assert_eq!(
        with_notes.fake.received_named("append_note")[0]["level"],
        "refused"
    );

    let without_notes = harness(&[]).await;
    say(&without_notes, 1, "/repos").await;
    assert!(note_texts(&without_notes).is_empty());
    let held = archon_daemon::triggers::held(&without_notes.ctx)
        .await
        .unwrap();
    assert_eq!(held.len(), 1, "the orchestrator restates the answer");
}
