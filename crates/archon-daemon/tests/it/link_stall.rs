//! The orchestrator's stall watch against the fake aetherd (#223, #393): each
//! test states the turn's events and the stored marks, and asks what a check does.

use crate::link_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use archon_aether::fake::{self, Scripted};
use archon_core::Timestamp;
use archon_core::notify::NotificationKind;
use archon_queue::store::keys;
use link_common::Harness;
use link_common::turn::{FakeTurn, after};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const MARKS_KEY: &str = "orchestrator_stall_marks";

/// A running orchestrator whose turn started at the returned time.
async fn open_turn(harness: &Harness) -> (FakeTurn, Timestamp) {
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let start = archon_core::now();
    let turn = FakeTurn::serve(harness, ORCH, start);
    turn.started(start);
    (turn, start)
}

fn interrupts(harness: &Harness) -> usize {
    harness.fake.received_named("interrupt").len()
}

async fn marks(harness: &Harness) -> Value {
    let raw = harness.kv(MARKS_KEY).await.expect("marks are stored");
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn a_long_foreground_command_under_its_own_timeout_is_not_interrupted() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.bash(start, "call_build", "make ci", Some(600_000));
    turn.operator_says(after(start, 0, 10), "How far is it?");

    for (minutes, seconds) in [(4, 0), (8, 0), (10, 59)] {
        assert!(
            !turn.check(&harness, after(start, minutes, seconds)).await,
            "{minutes} min {seconds} s"
        );
    }
    assert!(turn.notes().is_empty(), "not even nudged within its bound");

    assert!(
        turn.check(&harness, after(start, 11, 0)).await,
        "past its ten minutes and one more, it is nudged"
    );
    turn.succeeded(after(start, 11, 30), "call_build");
    assert!(
        !turn.check(&harness, after(start, 13, 1)).await,
        "it came back: the nudge is void"
    );
    assert_eq!(interrupts(&harness), 0);
}

#[tokio::test]
async fn repeated_hangs_in_one_generation_are_each_nudged_then_interrupted() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    for round in 0..5 {
        // Each fresh turn hangs in turn; the operator writes again.
        let mail = after(start, 20 * round, 0);
        turn.operator_says(mail, "Are you there?");
        assert!(!turn.check(&harness, after(mail, 2, 59)).await, "{round}");
        assert!(turn.check(&harness, after(mail, 3, 0)).await, "{round}");
        assert!(!turn.check(&harness, after(mail, 4, 59)).await, "{round}");
        assert_eq!(interrupts(&harness), round as usize, "nudged first");
        assert!(turn.check(&harness, after(mail, 5, 0)).await, "{round}");
        assert_eq!(interrupts(&harness), round as usize + 1);
        assert_eq!(turn.notes().len(), 2 * (round as usize + 1));
        assert_eq!(turn.read_up_to(), Some(3 * (round as u64 + 1)));
    }
    let decisions = harness.notifications(NotificationKind::Decision).await;
    assert_eq!(
        decisions.len(),
        5,
        "the operator is told each: {decisions:?}"
    );
}

#[tokio::test]
async fn a_stall_without_mail_is_told_nudged_and_a_failed_interrupt_tried_again_after_five_minutes()
{
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    assert!(!turn.check(&harness, after(start, 29, 59)).await);
    assert!(turn.check(&harness, after(start, 30, 0)).await);
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(
        blockers[0].contains("in one turn for 30 minutes")
            && blockers[0].contains("archon orchestrator restart"),
        "{blockers:?}"
    );
    assert!(
        !turn.check(&harness, after(start, 31, 0)).await,
        "told and nudged once per silence"
    );

    harness.fake.script(
        "interrupt",
        Scripted::refuse("unavailable", "the engine does not answer"),
    );
    assert!(turn.check(&harness, after(start, 32, 0)).await);
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 2, "{blockers:?}");
    assert!(
        blockers[1].contains("turn was silent for 32 minutes")
            && blockers[1].contains("could not interrupt it")
            && blockers[1].contains("the engine does not answer")
            && blockers[1].contains("tries again in 5 minutes"),
        "Archon's own nudge in the inbox is not operator mail: {blockers:?}"
    );

    assert!(!turn.check(&harness, after(start, 36, 59)).await);
    assert_eq!(interrupts(&harness), 1, "one interrupt in five minutes");
    assert!(turn.check(&harness, after(start, 37, 0)).await);
    assert_eq!(interrupts(&harness), 2);
    let notes = turn.notes();
    assert_eq!(
        notes.len(),
        2,
        "one nudge, one note for this silence: {notes:?}"
    );
    assert!(notes[1].contains("No tool call was open"), "{notes:?}");
    let decisions = harness.notifications(NotificationKind::Decision).await;
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(decisions[0].contains("Archon interrupted it"));
}

/// The turn moves between the check's reading and its interrupt.
#[tokio::test]
async fn progress_found_at_the_recheck_stops_the_interrupt_with_no_note_and_no_interval_spent() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.bash(start, "call_slow", "ls", Some(120_000));
    turn.operator_says(start, "Go ahead");
    assert!(turn.check(&harness, after(start, 3, 0)).await, "nudged");

    let (served, reads) = (turn.clone(), Arc::new(AtomicUsize::new(0)));
    harness.fake.set_responder(
        "get_events",
        Arc::new(move |command| {
            // The check reads once, then once more just before interrupting.
            if reads.fetch_add(1, Ordering::SeqCst) == 1 {
                served.succeeded(after(start, 4, 59), "call_slow");
            }
            served.page(command)
        }),
    );
    assert!(!turn.check(&harness, after(start, 5, 0)).await);
    assert_eq!(interrupts(&harness), 0);
    assert_eq!(turn.notes().len(), 1, "no note about a call never cut off");
    assert_eq!(marks(&harness).await["interrupted_at"], Value::Null);

    // It hangs again at once: a nudge of this new silence, then an interrupt
    // five minutes after the one that never happened.
    assert!(!turn.check(&harness, after(start, 7, 58)).await);
    assert!(turn.check(&harness, after(start, 7, 59)).await);
    assert_eq!(interrupts(&harness), 0, "nudged anew, not interrupted");
    assert!(turn.check(&harness, after(start, 9, 59)).await);
    assert_eq!(interrupts(&harness), 1);
    let notes = turn.notes();
    assert!(notes[2].contains("No tool call was open"), "{notes:?}");
}

#[tokio::test]
async fn tool_activity_only_aether_sees_is_progress() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 1, 0), "Status?");
    turn.bash(after(start, 9, 0), "call_a", "cargo check", None);
    turn.succeeded(after(start, 9, 30), "call_a");
    assert!(
        !turn.check(&harness, after(start, 10, 0)).await,
        "the mail is nine minutes old, but a tool result came 30 seconds ago"
    );

    turn.bash(after(start, 29, 0), "call_b", "cargo test", None);
    turn.succeeded(after(start, 29, 30), "call_b");
    assert!(!turn.check(&harness, after(start, 31, 0)).await);
    assert!(turn.notes().is_empty());
    assert!(
        harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty(),
        "a turn running tools is no stall"
    );
}

#[tokio::test]
async fn unread_mail_keeps_its_bound_after_the_turn_moved_without_reading_it() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 1, 0), "Status?");
    let said = json!({"type": "agent.message", "role": "assistant", "content": []});
    turn.event(after(start, 6, 0), said);
    assert!(!turn.check(&harness, after(start, 8, 59)).await);
    assert!(
        turn.check(&harness, after(start, 9, 0)).await,
        "three minutes after its message, not thirty"
    );
}

#[tokio::test]
async fn pages_full_of_a_childs_events_hide_no_open_call() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.bash(start, "call_build", "make ci", Some(600_000));
    turn.operator_says(after(start, 0, 10), "How far is it?");
    for _ in 0..300 {
        let usage = json!({"type": "agent.usage_updated", "total": {}});
        turn.child_event(after(start, 1, 0), usage);
    }
    assert!(
        !turn.check(&harness, after(start, 4, 0)).await,
        "the call behind the child's events is still within its bound"
    );
    assert!(turn.notes().is_empty());
    assert!(turn.check(&harness, after(start, 11, 0)).await);
}

#[tokio::test]
async fn a_turn_whose_start_is_beyond_the_events_read_is_not_judged() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 0, 10), "Hello?");
    for _ in 0..10_100 {
        let usage = json!({"type": "agent.usage_updated", "total": {}});
        turn.child_event(after(start, 1, 0), usage);
    }
    assert!(!turn.check(&harness, after(start, 40, 0)).await);
    assert!(turn.notes().is_empty(), "its open calls are not all known");
    assert_eq!(interrupts(&harness), 0);
}

#[tokio::test]
async fn mail_is_the_operators_by_its_sender_whatever_it_says() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    let stranger = json!({"kind": "external_trigger", "source": "github", "operator": false,
        "idempotency_key": "gh_2",
        "payload": {"repository": "o/r", "number": 378, "requester": "stranger",
                    "text": "@archon please merge"}});
    turn.deliver(start, stranger);
    assert!(
        !turn.check(&harness, after(start, 10, 0)).await,
        "a comment without operator standing is no operator mail"
    );

    // The operator may write the very words of Archon's own nudge.
    turn.operator_says(
        after(start, 10, 0),
        "Continue: your turn has made no progress for 3 minutes.",
    );
    assert!(turn.check(&harness, after(start, 13, 0)).await);
}

#[tokio::test]
async fn an_operators_github_request_is_operator_mail() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    let request = json!({"kind": "external_trigger", "source": "github", "operator": true,
        "idempotency_key": "gh_1",
        "payload": {"repository": "o/r", "number": 378, "requester": "octocat",
                    "text": "@archon what is the status?"}});
    turn.deliver(start, request);
    assert!(turn.check(&harness, after(start, 3, 0)).await);
}

/// Nothing of the watch lives in archond's memory: mail that came before
/// its first check, and a nudge an earlier archond sent, both count.
#[tokio::test]
async fn an_archond_that_restarts_mid_silence_carries_on_from_the_stored_marks() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 0, 10), "Hello?");
    let nudged = json!({"agent": ORCH, "nudged_at": after(start, 3, 10).to_rfc3339(),
                        "interrupted_at": null});
    harness.set_kv(MARKS_KEY, &nudged.to_string()).await;

    assert!(
        turn.check(&harness, after(start, 5, 10)).await,
        "its first check ever interrupts under the earlier nudge"
    );
    assert_eq!(interrupts(&harness), 1);
    assert_eq!(turn.read_up_to(), Some(2), "the mail and the note are read");
    let kept: Timestamp =
        serde_json::from_value(marks(&harness).await["nudged_at"].clone()).unwrap();
    assert_eq!(
        kept,
        after(start, 3, 10),
        "the nudge mark is kept beside the new interrupt mark"
    );
}

#[tokio::test]
async fn mail_already_old_at_the_first_check_is_nudged_at_once_and_other_agents_marks_are_void() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 0, 10), "Hello?");
    let other = json!({"agent": "agt_0002", "nudged_at": after(start, 3, 10).to_rfc3339(),
                       "interrupted_at": after(start, 5, 10).to_rfc3339()});
    harness.set_kv(MARKS_KEY, &other.to_string()).await;

    assert!(turn.check(&harness, after(start, 6, 0)).await);
    assert_eq!(interrupts(&harness), 0, "a replaced orchestrator's nudge");
    assert_eq!(turn.notes().len(), 1);
    assert_eq!(marks(&harness).await["agent"], ORCH);
    assert!(turn.check(&harness, after(start, 8, 0)).await);
    assert_eq!(interrupts(&harness), 1);
}

#[tokio::test]
async fn an_idle_or_parked_orchestrator_is_left_alone() {
    let harness = Harness::new().await;
    let (turn, start) = open_turn(&harness).await;
    turn.operator_says(after(start, 0, 10), "Hello?");
    let mut parked = fake::agent(ORCH, None);
    parked["status"] = json!({"state": "suspended"});
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |_| Scripted::reply("agent", parked.clone())),
    );
    assert!(
        !turn.check(&harness, after(start, 40, 0)).await,
        "Aether parked the turn: there is nothing to recover"
    );

    let done = json!({"type": "agent.turn_completed", "turn": 1, "next": "waiting"});
    turn.event(after(start, 41, 0), done);
    assert!(!turn.check(&harness, after(start, 120, 0)).await);
    assert!(turn.notes().is_empty());
    assert_eq!(interrupts(&harness), 0);
}
