use super::*;

fn at(secs: i64) -> Timestamp {
    chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
}

fn agent(id: &str) -> AetherAgentId {
    AetherAgentId::parse(id).unwrap()
}

const A: &str = "agt_01M5H1GBDP193XE44D522J0DN6";
const B: &str = "agt_01M5H1SFJV2J2RNXYAH7HB6M6B";
const STALL: i64 = STALL_AFTER.as_secs() as i64;

fn started() -> EventKind {
    EventKind::AgentStarted { turn: 241 }
}

fn ended() -> EventKind {
    EventKind::AgentTurnCompleted {
        turn: 241,
        next: Some("ready".into()),
    }
}

#[test]
fn a_turn_that_goes_silent_is_reported_once() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert_eq!(live.take_stall(at(STALL - 1)), None);
    assert_eq!(live.take_stall(at(STALL)).unwrap().silent, STALL_AFTER);
    assert_eq!(live.take_stall(at(STALL + 60)), None, "told once");
}

#[test]
fn any_event_rearms_the_report() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.take_stall(at(STALL)).is_some());
    let message = EventKind::AgentMessage {
        assistant: true,
        text: "still here".into(),
    };
    live.saw(&agent(A), &message, at(STALL + 10));
    assert_eq!(live.take_stall(at(STALL + 20)), None);
    assert!(live.take_stall(at(2 * STALL + 10)).is_some());
}

#[test]
fn a_turn_seen_only_by_its_messages_after_a_restart_is_watched() {
    let mut live = Liveness::default();
    let message = EventKind::AgentMessage {
        assistant: true,
        text: "pushing the tag".into(),
    };
    live.saw(&agent(A), &message, at(0));
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn an_idle_orchestrator_is_not_stalled() {
    let mut live = Liveness::default();
    assert_eq!(live.take_stall(at(10 * STALL)), None, "nothing seen yet");
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &ended(), at(5));
    assert_eq!(live.take_stall(at(10 * STALL)), None, "the turn ended");
}

#[test]
fn a_replaced_orchestrator_starts_clean() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(B), &EventKind::AgentCompleted, at(10));
    assert_eq!(live.take_stall(at(10 * STALL)), None);
}

#[test]
fn mail_delivered_to_a_hung_turn_does_not_rearm_the_report() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let mail = EventKind::InboxDelivered {
        seq: 7,
        from_operator: false,
        message: None,
    };
    live.saw(&agent(A), &mail, at(STALL - 10));
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn nudges_are_bounded_per_agent() {
    let mut live = Liveness::default();
    let mut nudged = 0;
    for round in 0..10 {
        let base = round * (STALL + 100);
        live.saw(&agent(A), &started(), at(base));
        let stall = live.take_stall(at(base + STALL)).expect("stalls again");
        nudged += i32::from(stall.nudge);
    }
    assert_eq!(nudged, i32::from(MAX_NUDGES));
}

#[test]
fn a_turn_found_open_after_a_restart_is_watched_from_the_probe() {
    let mut live = Liveness::default();
    assert!(live.needs_probe(&agent(A)));
    live.probed(&agent(A), true, at(0));
    assert!(!live.needs_probe(&agent(A)));
    assert!(live.take_stall(at(STALL - 1)).is_none());
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn a_progress_event_before_the_probe_does_not_skip_it() {
    let mut live = Liveness::default();
    let usage = EventKind::Other {
        type_name: "tool.result".into(),
    };
    live.saw(&agent(A), &usage, at(0));
    assert!(live.needs_probe(&agent(A)));
    live.probed(&agent(A), true, at(5));
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn an_idle_orchestrator_found_by_the_probe_is_not_watched() {
    let mut live = Liveness::default();
    live.probed(&agent(A), false, at(0));
    assert!(live.take_stall(at(10 * STALL)).is_none());
}

#[test]
fn the_notice_says_what_happened_and_what_to_do() {
    let hours = Duration::from_secs(3 * 3600);
    let text = stall_text(hours, Nudge::Sent, true);
    assert!(text.contains("180 minutes"), "{text}");
    assert!(text.contains("archon orchestrator restart"), "{text}");
    assert!(text.contains("sent it a nudge"), "{text}");
    assert!(text.contains("in 5 minutes, Archon interrupts the turn"), "{text}");
    let spent = stall_text(hours, Nudge::Spent, false);
    assert!(spent.contains("sends no more"), "{spent}");
    assert!(spent.contains("used its 3 interrupts"), "{spent}");
    let failed = stall_text(
        hours,
        Nudge::Failed(ArchonError::Invalid("no".into())),
        true,
    );
    assert!(failed.contains("could not nudge"), "{failed}");
}

#[test]
fn the_interrupt_notice_says_what_was_silent_what_archon_did_and_the_fallback() {
    let silent = Duration::from_secs(35 * 60);
    let stopped = interrupt_text(silent, &Interrupted::Stopped);
    assert!(stopped.contains("silent for 35 minutes"), "{stopped}");
    assert!(stopped.contains("Archon interrupted it"), "{stopped}");
    assert!(stopped.contains("archon orchestrator restart"), "{stopped}");
    let none = interrupt_text(silent, &Interrupted::NoTurn);
    assert!(none.contains("no turn running"), "{none}");
    assert!(none.contains("archon orchestrator restart"), "{none}");
    let failed = interrupt_text(
        silent,
        &Interrupted::Failed(ArchonError::Unavailable("aetherd gone".into())),
    );
    assert!(failed.contains("could not interrupt it"), "{failed}");
    assert!(failed.contains("aetherd gone"), "{failed}");
    assert!(failed.contains("archon orchestrator restart"), "{failed}");
}

const GRACE: i64 = INTERRUPT_GRACE.as_secs() as i64;
const MAIL: i64 = MAIL_STALL_AFTER.as_secs() as i64;

fn operator_mail(seq: u64) -> EventKind {
    EventKind::InboxDelivered {
        seq,
        from_operator: true,
        message: None,
    }
}

fn usage() -> EventKind {
    EventKind::AgentUsageUpdated {
        usage: Default::default(),
    }
}

#[test]
fn a_reported_stall_still_silent_after_the_grace_is_interrupted_once() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let stall = live.take_stall(at(STALL)).expect("stalled");
    assert!(stall.nudge && stall.interrupt, "{stall:?}");
    assert_eq!(live.take_interrupt(&agent(A), at(STALL + GRACE - 1)), None);
    let hung = live.take_interrupt(&agent(A), at(STALL + GRACE)).unwrap();
    assert_eq!(hung.agent, agent(A));
    assert_eq!(hung.silent, STALL_AFTER + INTERRUPT_GRACE);
    assert_eq!(
        live.take_interrupt(&agent(A), at(STALL + 10 * GRACE)),
        None,
        "one interrupt per silence"
    );
    assert_eq!(live.take_stall(at(3 * STALL)), None, "told once");
}

#[test]
fn only_the_current_orchestrator_is_interrupted() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.take_stall(at(STALL)).is_some());
    assert_eq!(live.take_interrupt(&agent(B), at(STALL + GRACE)), None);
    assert!(live.take_interrupt(&agent(A), at(STALL + GRACE)).is_some());
}

#[test]
fn a_turn_that_progresses_in_the_grace_is_not_interrupted() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.take_stall(at(STALL)).is_some());
    live.saw(&agent(A), &usage(), at(STALL + GRACE - 1));
    assert_eq!(live.take_interrupt(&agent(A), at(STALL + GRACE)), None);
    assert_eq!(live.take_interrupt(&agent(A), at(STALL + 5 * GRACE)), None);
}

#[test]
fn a_busy_turn_that_keeps_emitting_events_is_left_alone() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let result = EventKind::Other {
        type_name: "tool.result".into(),
    };
    for minute in (0..=240).step_by(9) {
        let now = at(minute * 60);
        live.saw(&agent(A), &operator_mail(minute as u64), now);
        let progress = if minute % 2 == 0 { usage() } else { result.clone() };
        live.saw(&agent(A), &progress, now);
        let later = now + chrono::TimeDelta::minutes(8);
        assert_eq!(live.take_stall(later), None);
        assert_eq!(live.take_interrupt(&agent(A), later), None);
    }
}

#[test]
fn interrupts_are_bounded_per_agent() {
    let mut live = Liveness::default();
    let mut interrupted = 0;
    for round in 0..10 {
        let base = round * (STALL + GRACE + 100);
        live.saw(&agent(A), &started(), at(base));
        let stall = live.take_stall(at(base + STALL)).expect("stalls again");
        assert_eq!(stall.interrupt, round < i64::from(MAX_INTERRUPTS));
        let hung = live.take_interrupt(&agent(A), at(base + STALL + GRACE));
        interrupted += i32::from(hung.is_some());
    }
    assert_eq!(interrupted, i32::from(MAX_INTERRUPTS));
    live.saw(&agent(B), &started(), at(0));
    assert!(live.take_stall(at(STALL)).unwrap().interrupt, "a new generation");
}

#[test]
fn operator_mail_to_a_turn_silent_past_the_shorter_limit_is_due_at_once() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(!live.saw(&agent(A), &operator_mail(1), at(MAIL - 1)), "too soon");
    assert!(live.take_stall(at(MAIL - 1)).is_none());
    assert!(live.saw(&agent(A), &operator_mail(2), at(MAIL)));
    let stall = live.take_stall(at(MAIL)).expect("the mail made it due");
    assert_eq!(stall.silent, MAIL_STALL_AFTER);
    assert!(stall.nudge);
    assert!(!live.saw(&agent(A), &operator_mail(3), at(MAIL + 1)), "reported");
    assert!(live.take_interrupt(&agent(A), at(MAIL + GRACE)).is_some());
}

#[test]
fn operator_mail_earlier_in_the_silence_shortens_the_limit() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(!live.saw(&agent(A), &operator_mail(1), at(60)));
    assert!(live.take_stall(at(MAIL - 1)).is_none());
    assert_eq!(live.take_stall(at(MAIL)).unwrap().silent, MAIL_STALL_AFTER);
}

#[test]
fn mail_that_is_not_the_operators_keeps_the_full_limit() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let trigger = EventKind::InboxDelivered {
        seq: 1,
        from_operator: false,
        message: None,
    };
    assert!(!live.saw(&agent(A), &trigger, at(MAIL)));
    assert!(live.take_stall(at(STALL - 1)).is_none());
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn operator_mail_to_an_idle_orchestrator_is_no_stall() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &ended(), at(5));
    assert!(!live.saw(&agent(A), &operator_mail(1), at(STALL)));
    assert!(live.take_stall(at(2 * STALL)).is_none());
}

#[test]
fn progress_forgets_the_operator_mail() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(1), at(60));
    live.saw(&agent(A), &EventKind::InboxConsumed { up_to: 1 }, at(120));
    assert!(live.take_stall(at(120 + MAIL)).is_none());
    assert!(live.take_stall(at(120 + STALL)).is_some());
}
