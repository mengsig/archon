use serde_json::json;

use super::*;

const ORCH: &str = "agt_01M6BFQZRXYJK8CKVK10H85F7C";
const CHILD: &str = "agt_01M6BFQZRXYJK8CKVK10H85F7D";

fn orch() -> AetherAgentId {
    AetherAgentId::parse(ORCH).unwrap()
}

fn at(minute: u32) -> Timestamp {
    format!("2026-11-15T19:{minute:02}:00Z").parse().unwrap()
}

fn event(seq: u64, minute: u32, agent: &str, kind: Value) -> Value {
    json!({"seq": seq, "at": at(minute).to_rfc3339(), "agent_id": agent, "kind": kind})
}

fn own(seq: u64, minute: u32, kind: Value) -> Value {
    event(seq, minute, ORCH, kind)
}

fn started(seq: u64, minute: u32) -> Value {
    own(seq, minute, json!({"type": "agent.started", "turn": 3}))
}

fn requested(seq: u64, minute: u32, id: &str, input: Value) -> Value {
    let call = json!({"id": id, "name": "Bash", "input": input});
    own(seq, minute, json!({"type": "tool.requested", "call": call}))
}

fn succeeded(seq: u64, minute: u32, id: &str) -> Value {
    own(seq, minute, json!({"type": "tool.succeeded", "call": id}))
}

fn consumed(seq: u64, minute: u32, up_to: u64) -> Value {
    own(
        seq,
        minute,
        json!({"type": "inbox.consumed", "up_to": up_to}),
    )
}

fn delivered(seq: u64, minute: u32, kind: Value) -> Value {
    let item = json!({"id": format!("ibx_{seq}"), "agent_id": ORCH, "seq": seq,
                      "delivered_at": at(minute).to_rfc3339(), "kind": kind});
    own(
        seq,
        minute,
        json!({"type": "inbox.delivered", "item": item}),
    )
}

fn operator_says(seq: u64, minute: u32, text: &str) -> Value {
    let from = json!({"actor": "user", "origin": {"transport": "local_socket"}});
    let kind = json!({"kind": "message", "message_id": format!("msg_{seq}"), "from": from,
                      "content": [{"type": "text", "text": text}]});
    delivered(seq, minute, kind)
}

fn archon_tells(seq: u64, minute: u32, text: &str) -> Value {
    let kind = json!({"kind": "external_trigger", "source": "archon",
                      "payload": {"archon": 1, "events": [{"type": "watchdog", "text": text}]}});
    delivered(seq, minute, kind)
}

/// The walk over one page that reaches the end of history.
fn turn_of(newest_first: &[Value]) -> Turn {
    let mut walk = Walk::default();
    walk.take_page(&orch(), newest_first, u64::MAX).unwrap();
    walk.finish()
}

fn open_turn(newest_first: &[Value]) -> OpenTurn {
    match turn_of(newest_first) {
        Turn::Open(turn) => turn,
        other => panic!("the turn is open, not {other:?}"),
    }
}

#[test]
fn every_open_call_is_found_not_only_the_newest() {
    let events = [
        requested(6, 14, "call_new", json!({"command": "ls"})),
        succeeded(5, 13, "call_done"),
        requested(4, 12, "call_done", json!({"command": "true"})),
        requested(
            3,
            11,
            "call_old",
            json!({"command": "make", "timeout": 600_000}),
        ),
        started(2, 10),
        consumed(1, 10, 7),
    ];
    let turn = open_turn(&events);
    let commands: Vec<_> = turn
        .open_calls
        .iter()
        .map(|call| call.command.as_deref().unwrap())
        .collect();
    assert_eq!(
        commands,
        ["ls", "make"],
        "newest first, the settled one out"
    );
    assert_eq!(
        turn.open_calls[1].timeout,
        Some(Duration::from_secs(600)),
        "the older call keeps its declared timeout"
    );
    assert_eq!(turn.open_calls[0].timeout, None);
    assert_eq!(turn.last_progress, at(14));
}

#[test]
fn a_tool_event_alone_is_progress() {
    let events = [
        succeeded(4, 20, "call_a"),
        requested(3, 12, "call_a", json!({"command": "ls"})),
        started(2, 10),
    ];
    let turn = open_turn(&events);
    assert_eq!(turn.last_progress, at(20), "Aether-only tool activity");
    assert!(turn.open_calls.is_empty());

    let running = [
        own(4, 13, json!({"type": "tool.started", "call": "call_a"})),
        requested(3, 12, "call_a", json!({"command": "ls"})),
        started(2, 10),
    ];
    let turn = open_turn(&running);
    assert_eq!(turn.last_progress, at(13), "a call that started moved");
    assert_eq!(turn.open_calls.len(), 1, "and is open until its outcome");
}

#[test]
fn delivered_mail_is_not_progress_and_stays_unread_through_other_progress() {
    let said = json!({"type": "agent.message", "role": "assistant", "content": []});
    let events = [
        operator_says(7, 19, "And one more thing"),
        own(6, 15, said),
        operator_says(5, 14, "What is running?"),
        consumed(4, 11, 3),
        operator_says(3, 10, "An older one, read"),
        started(2, 10),
    ];
    let turn = open_turn(&events);
    assert_eq!(
        turn.last_progress,
        at(15),
        "mail arriving at 19 moved nothing"
    );
    assert_eq!(
        turn.oldest_unread_mail,
        Some(at(14)),
        "the message at 15 did not read the mail of 14; the mail of 10 was consumed"
    );
}

#[test]
fn operator_mail_is_told_by_its_sender_never_by_its_text() {
    let nudge = "Continue: your turn has made no progress for 3 minutes.";
    let from_archon = [archon_tells(3, 12, nudge), started(2, 10)];
    assert_eq!(open_turn(&from_archon).oldest_unread_mail, None);

    let from_operator = [operator_says(3, 12, nudge), started(2, 10)];
    assert_eq!(
        open_turn(&from_operator).oldest_unread_mail,
        Some(at(12)),
        "the operator may say what Archon's own note says"
    );

    let peer = json!({"kind": "message", "message_id": "msg_3",
                      "from": {"actor": "agent", "id": CHILD},
                      "content": [{"type": "text", "text": "done"}]});
    let from_agent = [delivered(3, 12, peer), started(2, 10)];
    assert_eq!(open_turn(&from_agent).oldest_unread_mail, None);
}

#[test]
fn a_turn_that_ended_or_never_began_is_closed() {
    let done = json!({"type": "agent.turn_completed", "turn": 3, "next": "waiting"});
    let ended = [
        own(4, 13, done),
        requested(3, 12, "call_a", json!({})),
        started(2, 10),
    ];
    assert_eq!(turn_of(&ended), Turn::Closed);
    let interrupted = [
        own(3, 13, json!({"type": "agent.interrupted", "turn": 3})),
        started(2, 10),
    ];
    assert_eq!(turn_of(&interrupted), Turn::Closed);
    assert_eq!(
        turn_of(&[operator_says(1, 10, "hello")]),
        Turn::Closed,
        "mail to an agent with no turn"
    );
}

#[test]
fn a_childs_events_are_not_the_agents() {
    let child_call = json!({"id": "call_c", "name": "Bash", "input": {"command": "sleep 9"}});
    let events = [
        event(
            4,
            40,
            CHILD,
            json!({"type": "tool.requested", "call": child_call}),
        ),
        started(2, 10),
    ];
    let turn = open_turn(&events);
    assert_eq!(turn.last_progress, at(10));
    assert!(turn.open_calls.is_empty());
}

#[test]
fn a_page_that_does_not_reach_the_turns_start_is_not_complete() {
    let mut walk = Walk::default();
    let page = [
        succeeded(9, 13, "call_b"),
        requested(8, 12, "call_b", json!({})),
    ];
    let next = walk.take_page(&orch(), &page, u64::MAX).unwrap();
    assert_eq!(next, 8, "the next page ends below the oldest event read");
    assert!(
        !walk.is_complete(),
        "an older call may be open before these"
    );

    walk.take_page(&orch(), &[started(2, 10), consumed(1, 10, 4)], next)
        .unwrap();
    assert!(walk.is_complete());
    let ascending = [started(3, 27), started(4, 28)];
    assert!(
        Walk::default()
            .take_page(&orch(), &ascending, u64::MAX)
            .is_err()
    );
}

#[test]
fn only_a_foreground_call_declares_a_timeout() {
    let foreground = json!({"command": "make", "timeout": 120_000});
    assert_eq!(
        foreground_timeout(Some(&foreground)),
        Some(Duration::from_secs(120))
    );
    let background = json!({"command": "make", "timeout": 120_000, "run_in_background": true});
    assert_eq!(foreground_timeout(Some(&background)), None);
    assert_eq!(foreground_timeout(Some(&json!({"command": "ls"}))), None);
    assert_eq!(foreground_timeout(None), None);
}
