use chrono::TimeDelta;

use super::notice::{Interrupted, cut_off_note, interrupt_text, nudge_text, stall_text};
use super::*;

fn orch() -> AetherAgentId {
    AetherAgentId::parse("agt_01M6BFQZRXYJK8CKVK10H85F7C").unwrap()
}

/// `seconds` after the turn's last progress.
fn at(seconds: i64) -> Timestamp {
    let start: Timestamp = "2026-11-15T19:12:00Z".parse().unwrap();
    start + TimeDelta::seconds(seconds)
}

fn call(started: i64, timeout_secs: Option<u64>) -> OpenCall {
    OpenCall {
        name: "Bash".into(),
        command: Some("ls && head -40 fetch.sh".into()),
        timeout: timeout_secs.map(Duration::from_secs),
        requested_at: at(started),
    }
}

/// A turn silent since `at(0)`, with operator mail unread since `mail`.
fn turn(mail: Option<i64>, open_calls: Vec<OpenCall>) -> OpenTurn {
    OpenTurn {
        last_progress: at(0),
        open_calls,
        oldest_unread_mail: mail.map(at),
    }
}

fn marks(nudged: Option<i64>, interrupted: Option<i64>) -> Marks {
    Marks {
        agent: orch(),
        nudged_at: nudged.map(at),
        interrupted_at: interrupted.map(at),
    }
}

fn is_nudge(due: &Due, mail: bool) -> bool {
    matches!(due, Due::Nudge(silence) if silence.mail == mail && silence.since == at(0))
}

fn is_interrupt(due: &Due, mail: bool) -> bool {
    matches!(due, Due::Interrupt(silence) if silence.mail == mail && silence.since == at(0))
}

#[test]
fn mail_and_silence_must_both_be_three_minutes_old_before_the_nudge() {
    let none = marks(None, None);
    let old_mail = turn(Some(0), vec![]);
    assert_eq!(due(&old_mail, &none, at(179)), Due::Nothing);
    assert!(is_nudge(&due(&old_mail, &none, at(180)), true));

    let fresh_mail = turn(Some(600), vec![]);
    assert_eq!(
        due(&fresh_mail, &none, at(779)),
        Due::Nothing,
        "a long silence, but the mail is under three minutes old"
    );
    assert!(is_nudge(&due(&fresh_mail, &none, at(780)), true));
}

#[test]
fn the_interrupt_follows_two_more_silent_minutes_after_the_nudge() {
    let hung = turn(Some(0), vec![]);
    let nudged = marks(Some(180), None);
    assert_eq!(
        due(&hung, &nudged, at(299)),
        Due::Nothing,
        "in the grace: no second nudge, no interrupt yet"
    );
    assert!(is_interrupt(&due(&hung, &nudged, at(300)), true));
}

#[test]
fn every_interrupt_follows_a_nudge_of_its_own_silence_however_many_came_before() {
    let hung = turn(Some(0), vec![]);
    for earlier_stalls in 0..6 {
        // What an earlier silence left: its nudge, and its interrupt long ago.
        let before = -3600 * (earlier_stalls + 1);
        let stale = marks(Some(before), Some(before + 120));
        assert!(
            is_nudge(&due(&hung, &stale, at(900)), true),
            "an old nudge is not this silence's: {earlier_stalls} earlier stalls"
        );
        let own = marks(Some(900), Some(before + 120));
        assert!(is_interrupt(&due(&hung, &own, at(1020)), true));
    }
}

#[test]
fn interrupts_are_spaced_five_minutes_apart_with_no_lifetime_cap() {
    let hung = turn(Some(0), vec![]);
    let failed = marks(Some(180), Some(300));
    assert_eq!(
        due(&hung, &failed, at(599)),
        Due::Nothing,
        "the last interrupt was under five minutes ago"
    );
    assert!(
        is_interrupt(&due(&hung, &failed, at(600)), true),
        "tried again, under the nudge already sent in this silence"
    );
    for tried in 1..20 {
        let again = marks(Some(180), Some(300 * tried));
        let retry = at(300 * (tried + 1));
        assert!(is_interrupt(&due(&hung, &again, retry), true), "{tried}");
    }
}

#[test]
fn a_foreground_call_within_its_declared_timeout_keeps_mail_waiting() {
    let none = marks(None, None);
    // 2026-11-15: a Bash call declaring two minutes that never came back.
    let trivial = turn(Some(0), vec![call(0, Some(120))]);
    assert_eq!(due(&trivial, &none, at(179)), Due::Nothing);
    assert!(is_nudge(&due(&trivial, &none, at(180)), true));

    let build = turn(Some(0), vec![call(0, Some(600))]);
    for minute in [3, 5, 10] {
        assert_eq!(
            due(&build, &none, at(minute * 60)),
            Due::Nothing,
            "{minute} min"
        );
    }
    assert!(is_nudge(&due(&build, &none, at(660)), true));

    let undeclared = turn(Some(0), vec![call(0, None)]);
    assert_eq!(due(&undeclared, &none, at(239)), Due::Nothing);
    assert!(is_nudge(&due(&undeclared, &none, at(240)), true));
}

#[test]
fn the_longest_bound_among_parallel_calls_holds() {
    let none = marks(None, None);
    // The newer call declares nothing; the older one may run ten minutes.
    let parallel = turn(Some(0), vec![call(0, None), call(-30, Some(600))]);
    assert_eq!(
        due(&parallel, &none, at(240)),
        Due::Nothing,
        "the newer call's four minutes are over, the older call's bound is not"
    );
    assert_eq!(due(&parallel, &none, at(629)), Due::Nothing);
    assert!(is_nudge(&due(&parallel, &none, at(630)), true));
}

#[test]
fn unread_mail_keeps_its_bound_when_the_turn_moved_without_reading_it() {
    // Mail came, the turn emitted a message at `at(0)` and then hung.
    let moved_on = turn(Some(-600), vec![]);
    assert!(
        is_nudge(&due(&moved_on, &marks(None, None), at(180)), true),
        "three minutes of silence, not thirty"
    );
}

#[test]
fn without_mail_a_stall_is_half_an_hour_then_nudged_then_interrupted() {
    let silent = turn(None, vec![call(0, Some(120))]);
    let none = marks(None, None);
    assert_eq!(due(&silent, &none, at(29 * 60)), Due::Nothing);
    assert!(is_nudge(&due(&silent, &none, at(30 * 60)), false));
    let nudged = marks(Some(30 * 60), None);
    assert_eq!(due(&silent, &nudged, at(31 * 60)), Due::Nothing);
    assert!(is_interrupt(&due(&silent, &nudged, at(32 * 60)), false));
}

#[test]
fn a_call_is_bound_by_its_timeout_capped_as_aether_caps_it() {
    assert_eq!(
        call_bound(&call(0, Some(120))),
        Duration::from_secs(180),
        "its timeout and a minute"
    );
    assert_eq!(
        call_bound(&call(0, Some(3600))),
        FOREGROUND_CAP + PAST_TIMEOUT
    );
    assert_eq!(call_bound(&call(0, None)), UNDECLARED_CALL_BOUND);
}

#[test]
fn marks_survive_the_store() {
    let saved = marks(Some(180), Some(300));
    let raw = serde_json::to_string(&saved).unwrap();
    assert_eq!(serde_json::from_str::<Marks>(&raw).unwrap(), saved);
    assert_eq!(Marks::none(&orch()), marks(None, None));
}

fn silence(seconds: u64, mail: bool) -> Silence {
    Silence {
        since: at(0),
        length: Duration::from_secs(seconds),
        mail,
    }
}

#[test]
fn the_cut_off_note_names_every_open_call_and_claims_no_more_than_it_knows() {
    let mut other = call(-30, Some(600));
    other.command = Some("make ci".into());
    let note = cut_off_note(&silence(300, true), &[call(0, None), other]);
    assert!(
        note.starts_with("Archon is interrupting your turn: it made no progress for 5 minutes")
            && note.contains("while the operator's mail waited")
            && note.contains("`Bash` running `ls && head -40 fetch.sh`, started 19:12:00Z")
            && note.contains("`Bash` running `make ci`, started 19:11:30Z")
            && note.contains("Any of them without a result was cut off")
            && note.ends_with("Messages that waited for you follow."),
        "{note}"
    );
    let none_open = cut_off_note(&silence(1920, false), &[]);
    assert!(
        none_open.contains("No tool call was open") && !none_open.contains("mail"),
        "{none_open}"
    );
    let many: Vec<OpenCall> = (0..8).map(|_| call(0, None)).collect();
    assert!(cut_off_note(&silence(300, true), &many).contains("; 3 more."));
}

#[test]
fn the_operator_is_told_what_was_done_and_what_to_do_when_it_fails() {
    let mail = silence(300, true);
    let stopped = interrupt_text(&mail, &[call(0, Some(120))], &Interrupted::Stopped);
    assert!(
        stopped.contains("Operator mail waited")
            && stopped.contains("silent for 5 minutes, in a `Bash` call past its bound of 3 min")
            && stopped.contains("Archon interrupted it"),
        "{stopped}"
    );
    let stall = silence(1920, false);
    let failed = Interrupted::Failed(ArchonError::Unavailable("no answer".into()));
    let failed = interrupt_text(&stall, &[], &failed);
    assert!(
        failed.contains("silent for 32 minutes")
            && failed.contains("could not interrupt it")
            && failed.contains("tries again in 5 minutes")
            && failed.contains("archon orchestrator restart"),
        "{failed}"
    );
    let idle = interrupt_text(&stall, &[], &Interrupted::NoTurn);
    assert!(idle.contains("no turn running"), "{idle}");
    assert!(nudge_text(&mail).contains("operator's mail is waiting"));
    assert!(stall_text(&stall).contains("in one turn for 32 minutes"));
}
