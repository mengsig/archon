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

const MAIL: i64 = MAIL_SILENCE.as_secs() as i64;
const GRACE: i64 = NUDGE_GRACE.as_secs() as i64;
const SPACING: i64 = INTERRUPT_SPACING.as_secs() as i64;

fn operator_mail(seq: u64) -> EventKind {
    EventKind::InboxDelivered {
        seq,
        from_operator: true,
        message: None,
    }
}

fn operator_text(seq: u64, text: &str) -> EventKind {
    use archon_core::agent_message::MessageSender;
    EventKind::InboxDelivered {
        seq,
        from_operator: true,
        message: Some(archon_aether::events::InboxMessage {
            channel: None,
            device: None,
            sender: MessageSender::Operator,
            text: text.into(),
        }),
    }
}

fn usage() -> EventKind {
    EventKind::AgentUsageUpdated {
        usage: Default::default(),
    }
}

/// A Bash call requested at `secs`, declaring `timeout` when given.
fn bash(secs: i64, timeout: Option<Duration>) -> OpenToolCall {
    OpenToolCall {
        id: "call_01".into(),
        name: "Bash".into(),
        command: Some("ls && head -40 fetch.sh".into()),
        timeout,
        requested_at: at(secs),
        last_event_at: at(secs),
    }
}

/// Whether `live` interrupts its silence at `now` with `call` open, as
/// `report_stall` decides it.
fn interrupts(
    live: &mut Liveness,
    now: Timestamp,
    call: Result<Option<OpenToolCall>, ArchonError>,
) -> Option<Hung> {
    let silence = live.candidate(&agent(A), now)?;
    if silence.mail && now < mail_deadline(silence.since, &call) {
        return None;
    }
    live.take_interrupt(&silence, now)
}

#[test]
fn operator_mail_to_a_turn_with_no_call_running_interrupts_it_after_three_minutes() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(!live.saw(&agent(A), &operator_mail(1), at(60)), "too soon");
    assert!(interrupts(&mut live, at(MAIL - 1), Ok(None)).is_none());
    assert_eq!(live.take_stall(at(MAIL)), None, "no nudge first");
    let hung = interrupts(&mut live, at(MAIL), Ok(None)).expect("the mail made it due");
    assert_eq!(
        hung,
        Hung {
            agent: agent(A),
            silent: MAIL_SILENCE,
            mail: true
        }
    );
    assert!(
        interrupts(&mut live, at(MAIL + SPACING), Ok(None)).is_none(),
        "once per silence"
    );
}

#[test]
fn operator_mail_arriving_after_the_bound_makes_the_check_due_at_once() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.saw(&agent(A), &operator_mail(1), at(MAIL)));
    assert!(interrupts(&mut live, at(MAIL), Ok(None)).is_some());
}

/// 2026-11-15: a trivial Bash at the default timeout sat 11 minutes with
/// three operator messages queued behind it.
#[test]
fn a_hung_call_with_mail_waiting_is_interrupted_within_its_bound() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &usage(), at(1));
    live.saw(&agent(A), &operator_mail(585), at(7 * 60));
    let call = || Ok(Some(bash(1, None)));
    let bound = 1 + UNDECLARED_CALL_BOUND.as_secs() as i64;
    assert!(interrupts(&mut live, at(bound - 1), call()).is_none());
    let hung = interrupts(&mut live, at(7 * 60), call()).expect("past the 4-minute bound");
    assert!(
        hung.mail && hung.silent < Duration::from_secs(7 * 60),
        "{hung:?}"
    );
}

#[test]
fn a_long_foreground_command_under_its_own_timeout_is_not_interrupted() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(1), at(30));
    let ten = Duration::from_secs(600);
    let call = || Ok(Some(bash(0, Some(ten))));
    for minute in [3, 5, 9, 10] {
        assert!(
            interrupts(&mut live, at(minute * 60), call()).is_none(),
            "minute {minute}"
        );
    }
    assert!(interrupts(&mut live, at(11 * 60 - 1), call()).is_none());
    assert!(
        interrupts(&mut live, at(11 * 60), call()).is_some(),
        "T + 1 min"
    );
}

#[test]
fn a_declared_timeout_is_capped_at_the_foreground_cap_and_an_unreadable_call_gets_the_cap() {
    let two_hours = bash(0, Some(Duration::from_secs(7200)));
    assert_eq!(call_bound(&two_hours), FOREGROUND_CAP + PAST_TIMEOUT);
    assert_eq!(
        call_bound(&bash(0, Some(Duration::from_secs(120)))),
        Duration::from_secs(180)
    );
    assert_eq!(call_bound(&bash(0, None)), UNDECLARED_CALL_BOUND);
    let short = bash(0, Some(Duration::from_secs(10)));
    assert_eq!(
        mail_deadline(at(0), &Ok(Some(short))),
        at(MAIL),
        "never under the silence"
    );
    let unreadable = Err(ArchonError::Unavailable("aetherd gone".into()));
    assert_eq!(
        mail_deadline(at(0), &unreadable),
        at(UNKNOWN_CALL_BOUND.as_secs() as i64),
        "a call that cannot be read may be a healthy foreground one"
    );
    assert_eq!(UNKNOWN_CALL_BOUND, FOREGROUND_CAP + PAST_TIMEOUT);
}

#[test]
fn repeated_hangs_in_one_generation_are_each_interrupted_at_most_once_per_spacing() {
    let mut live = Liveness::default();
    let mut interrupted = 0;
    for round in 0..10 {
        let base = round * (SPACING + 60);
        live.saw(&agent(A), &started(), at(base));
        live.saw(&agent(A), &operator_mail(round as u64), at(base + 10));
        interrupted += i32::from(interrupts(&mut live, at(base + MAIL), Ok(None)).is_some());
    }
    assert_eq!(interrupted, 10, "no lifetime cap");

    let base = 10 * (SPACING + 60);
    live.saw(&agent(A), &started(), at(base));
    live.saw(&agent(A), &operator_mail(99), at(base));
    assert!(interrupts(&mut live, at(base + MAIL), Ok(None)).is_some());
    live.saw(&agent(A), &started(), at(base + MAIL + 1));
    live.saw(&agent(A), &operator_mail(100), at(base + MAIL + 1));
    let next = base + 2 * MAIL + 1;
    assert!(
        interrupts(&mut live, at(next), Ok(None)).is_none(),
        "spaced"
    );
    assert!(interrupts(&mut live, at(base + MAIL + SPACING), Ok(None)).is_some());
}

#[test]
fn a_nudged_stall_is_interrupted_when_the_nudge_brings_no_progress_in_two_minutes() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let stall = live.take_stall(at(STALL)).expect("stalled");
    assert!(stall.nudge);
    assert!(interrupts(&mut live, at(STALL + GRACE - 1), Ok(None)).is_none());
    let hung = interrupts(&mut live, at(STALL + GRACE), Ok(None)).unwrap();
    assert_eq!(hung.silent, STALL_AFTER + NUDGE_GRACE);
    assert!(!hung.mail);
    assert_eq!(live.take_stall(at(3 * STALL)), None, "told once");
}

#[test]
fn a_turn_that_progresses_after_the_nudge_is_not_interrupted() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.take_stall(at(STALL)).is_some());
    live.saw(&agent(A), &usage(), at(STALL + GRACE - 1));
    assert!(interrupts(&mut live, at(STALL + GRACE), Ok(None)).is_none());
    assert!(interrupts(&mut live, at(STALL + 5 * GRACE), Ok(None)).is_none());
}

#[test]
fn only_the_current_orchestrator_is_interrupted() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(live.take_stall(at(STALL)).is_some());
    assert_eq!(live.candidate(&agent(B), at(STALL + GRACE)), None);
    assert!(live.candidate(&agent(A), at(STALL + GRACE)).is_some());
}

#[test]
fn progress_between_finding_and_claiming_a_silence_cancels_the_interrupt() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(1), at(10));
    let silence = live.candidate(&agent(A), at(MAIL)).unwrap();
    live.saw(&agent(A), &usage(), at(MAIL));
    assert_eq!(live.take_interrupt(&silence, at(MAIL)), None);
}

#[test]
fn a_failed_interrupt_is_tried_again_after_the_spacing() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(1), at(10));
    assert!(interrupts(&mut live, at(MAIL), Ok(None)).is_some());
    live.interrupt_failed(&agent(A));
    assert!(interrupts(&mut live, at(MAIL + SPACING - 1), Ok(None)).is_none());
    assert!(interrupts(&mut live, at(MAIL + SPACING), Ok(None)).is_some());
}

#[test]
fn archons_own_notes_are_not_operator_mail_but_a_lookalike_is() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    let nudge = notice::nudge_text(STALL_AFTER);
    live.sending_note(&nudge);
    assert!(!live.saw(&agent(A), &operator_text(1, &nudge), at(STALL)));
    assert!(
        live.take_stall(at(STALL)).is_some(),
        "still the no-mail rule"
    );
    assert!(
        interrupts(&mut live, at(STALL + 1), Ok(None)).is_none(),
        "the nudge gets its grace"
    );
    // The operator may well write what Archon once did; only a pending note is Archon's.
    live.saw(&agent(A), &operator_text(2, &nudge), at(STALL + 1));
    assert!(
        interrupts(&mut live, at(STALL + 1), Ok(None)).is_some(),
        "real mail is due at once"
    );
}

#[test]
fn pending_notes_stay_bounded() {
    let mut live = Liveness::default();
    for n in 0..3 * PENDING_NOTES {
        live.sending_note(&format!("note {n}"));
    }
    assert_eq!(live.pending_notes.len(), PENDING_NOTES);
    assert!(!live.take_note("note 0"), "the oldest is forgotten");
    assert!(live.take_note(&format!("note {}", 3 * PENDING_NOTES - 1)));
}

#[test]
fn a_busy_turn_that_keeps_emitting_events_is_left_alone() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    for minute in (0..=240).step_by(2) {
        let now = at(minute * 60);
        live.saw(&agent(A), &operator_mail(minute as u64), now);
        live.saw(&agent(A), &usage(), now);
        let later = now + chrono::TimeDelta::seconds(MAIL - 1);
        assert_eq!(live.take_stall(later), None);
        assert_eq!(live.candidate(&agent(A), later), None);
    }
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
    assert!(live.candidate(&agent(A), at(STALL - 1)).is_none());
    assert!(live.take_stall(at(STALL)).is_some());
}

#[test]
fn operator_mail_to_an_idle_orchestrator_is_no_stall() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &ended(), at(5));
    assert!(!live.saw(&agent(A), &operator_mail(1), at(STALL)));
    assert!(live.take_stall(at(2 * STALL)).is_none());
    assert!(live.candidate(&agent(A), at(2 * STALL)).is_none());
}

#[test]
fn only_consuming_the_inbox_forgets_the_operator_mail() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(4), at(60));
    live.saw(&agent(A), &usage(), at(120));
    assert!(
        interrupts(&mut live, at(120 + MAIL), Ok(None)).is_some(),
        "progress is not reading the mail"
    );
    live.saw(&agent(A), &started(), at(400));
    live.saw(&agent(A), &EventKind::InboxConsumed { up_to: 3 }, at(401));
    assert!(
        live.candidate(&agent(A), at(401 + MAIL)).is_some(),
        "seq 4 is unread"
    );
    live.saw(&agent(A), &EventKind::InboxConsumed { up_to: 4 }, at(402));
    assert!(live.candidate(&agent(A), at(402 + MAIL)).is_none());
    assert!(live.take_stall(at(402 + STALL)).is_some());
}

/// Archon's stream carries no tool events: a call that just ended, seen
/// only in Aether, restarts the silence of either kind.
#[test]
fn a_tool_event_only_aether_saw_restarts_the_silence() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    live.saw(&agent(A), &operator_mail(1), at(10));
    assert!(live.check_due(&agent(A), at(MAIL)));
    live.saw_activity(&agent(A), at(MAIL - 5));
    assert!(!live.check_due(&agent(A), at(MAIL)));
    assert!(interrupts(&mut live, at(2 * MAIL - 6), Ok(None)).is_none());
    live.saw_activity(&agent(A), at(1));
    assert!(
        interrupts(&mut live, at(2 * MAIL - 5), Ok(None)).is_some(),
        "older activity is no news"
    );

    let mut nudged = Liveness::default();
    nudged.saw(&agent(A), &started(), at(0));
    assert!(nudged.check_due(&agent(A), at(STALL)));
    nudged.saw_activity(&agent(A), at(STALL - 60));
    assert!(
        nudged.take_stall(at(STALL)).is_none(),
        "the tool kept the turn busy"
    );
    assert!(nudged.take_stall(at(2 * STALL)).is_some());
    nudged.saw_activity(&agent(A), at(2 * STALL + 1));
    assert!(
        nudged.candidate(&agent(A), at(2 * STALL + GRACE)).is_none(),
        "progress after the nudge"
    );
    assert!(
        nudged.take_stall(at(3 * STALL + 1)).is_some(),
        "a later stall gets its own nudge"
    );
}

#[test]
fn nothing_is_due_for_another_agent_or_a_closed_turn() {
    let mut live = Liveness::default();
    live.saw(&agent(A), &started(), at(0));
    assert!(!live.check_due(&agent(B), at(STALL)));
    live.saw(&agent(A), &ended(), at(1));
    assert!(!live.check_due(&agent(A), at(10 * STALL)));
    live.saw_activity(&agent(A), at(2));
    assert!(live.take_stall(at(10 * STALL)).is_none());
}
