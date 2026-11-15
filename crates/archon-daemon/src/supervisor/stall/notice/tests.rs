use archon_core::aether_ids::AetherAgentId;

use super::*;

fn hung(minutes: u64, mail: bool) -> Hung {
    Hung {
        agent: AetherAgentId::parse("agt_01M5H1GBDP193XE44D522J0DN6").unwrap(),
        silent: Duration::from_secs(minutes * 60),
        mail,
    }
}

fn bash(command: &str) -> OpenToolCall {
    let at = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    OpenToolCall {
        id: "call_01".into(),
        name: "Bash".into(),
        command: Some(command.into()),
        timeout: Some(Duration::from_secs(90)),
        requested_at: at,
        last_event_at: at,
    }
}

#[test]
fn the_stall_notice_says_what_happened_and_what_follows() {
    let hours = Duration::from_secs(3 * 3600);
    let text = stall_text(hours, Nudge::Sent);
    assert!(text.contains("180 minutes"), "{text}");
    assert!(text.contains("sent it a nudge"), "{text}");
    assert!(
        text.contains("in 2 minutes, Archon interrupts the turn"),
        "{text}"
    );
    assert!(text.contains("archon orchestrator restart"), "{text}");
    assert!(stall_text(hours, Nudge::Spent).contains("sends no more"));
    let failed = stall_text(hours, Nudge::Failed(ArchonError::Invalid("no".into())));
    assert!(failed.contains("could not nudge"), "{failed}");
}

#[test]
fn the_orchestrator_is_told_which_call_was_cut_off() {
    let note = cut_off_note(&hung(7, true), &Ok(Some(bash("ls && head -40 fetch.sh"))));
    assert!(is_own_note(&note), "{note}");
    assert!(
        note.contains("silent for 7 minutes, while the operator's mail waited"),
        "{note}"
    );
    assert!(
        note.contains("`Bash` running `ls && head -40 fetch.sh`"),
        "{note}"
    );
    assert!(note.contains("run it again"), "{note}");
    let long = cut_off_note(&hung(7, false), &Ok(Some(bash(&"é".repeat(400)))));
    assert!(
        long.contains("…`") && !long.contains(&"é".repeat(301)),
        "{long}"
    );
    let none = cut_off_note(&hung(3, true), &Ok(None));
    assert!(none.contains("No tool call was open"), "{none}");
    let unread = cut_off_note(
        &hung(3, true),
        &Err(ArchonError::Unavailable("gone".into())),
    );
    assert!(unread.contains("could not read which tool call") && unread.contains("gone"));
    assert!(is_own_note(&nudge_text(Duration::from_secs(1800))));
    assert!(!is_own_note("Continue with the release, please"));
}

#[test]
fn the_interrupt_notice_says_why_what_archon_did_and_the_fallback() {
    let call = Ok(Some(bash("make ci")));
    let told = Ok(());
    let stopped = interrupt_text(&hung(4, true), &call, &Interrupted::Stopped, &told);
    assert!(stopped.contains("Operator mail waited"), "{stopped}");
    assert!(
        stopped.contains("past its bound of 2 min 30 s"),
        "{stopped}"
    );
    assert!(stopped.contains("Archon interrupted it"), "{stopped}");
    assert!(stopped.contains("told which call was cut off"), "{stopped}");
    let nudged = interrupt_text(&hung(32, false), &Ok(None), &Interrupted::Stopped, &told);
    assert!(
        nudged.contains("2 minutes after Archon's nudge"),
        "{nudged}"
    );
    assert!(nudged.contains("no tool call running"), "{nudged}");
    let none = interrupt_text(&hung(4, true), &call, &Interrupted::NoTurn, &told);
    assert!(none.contains("no turn running") && none.contains("archon orchestrator restart"));
    let failed = Interrupted::Failed(ArchonError::Unavailable("aetherd gone".into()));
    let untold = Err(ArchonError::Unavailable("inbox closed".into()));
    let text = interrupt_text(&hung(4, true), &call, &failed, &untold);
    assert!(
        text.contains("could not interrupt it (") && text.contains("aetherd gone"),
        "{text}"
    );
    assert!(text.contains("tries again in 5 minutes"), "{text}");
    assert!(text.contains("could not tell it which call") && text.contains("inbox closed"));
}
