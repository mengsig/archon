//! What the operator and the orchestrator read about a silent turn.

use archon_aether::turn::OpenCall;
use archon_core::error::ArchonError;

use super::{INTERRUPT_SPACING, NUDGE_GRACE, Silence, call_bound};

/// The longest command a note quotes.
const COMMAND_QUOTE: usize = 300;

/// What the interrupt of a hung turn did.
pub(super) enum Interrupted {
    Stopped,
    NoTurn,
    Failed(ArchonError),
}

impl Interrupted {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::NoTurn => "no turn",
            Self::Failed(_) => "failed",
        }
    }
}

fn minutes(silence: &Silence) -> u64 {
    let minutes = silence.length.as_secs() / 60;
    assert!(minutes > 0, "a silence acted on is minutes long");
    minutes
}

/// What the orchestrator reads as its nudge.
pub(super) fn nudge_text(silence: &Silence) -> String {
    let waiting = match silence.mail {
        true => " The operator's mail is waiting for you.",
        false => "",
    };
    format!(
        "Continue: your turn has made no progress for {} minutes.{waiting} If you ended it \
         with work still pending, such as watching a release build, pick that work up now.",
        minutes(silence)
    )
}

/// What the operator reads of a stall with no mail waiting, once nudged.
pub(super) fn stall_text(silence: &Silence) -> String {
    assert!(!silence.mail, "mail's nudge is not told; its interrupt is");
    format!(
        "The orchestrator has been in one turn for {} minutes without a message, tool result \
         or usage update; its model request or a tool call may be hung, and nothing will wake \
         it by itself. Archon sent it a nudge to continue. If it is still silent in {} \
         minutes, Archon interrupts the turn, as Esc in its terminal would, so that waiting \
         messages reach a fresh turn; if that fails, run `archon orchestrator restart` to \
         start a fresh one.",
        minutes(silence),
        NUDGE_GRACE.as_secs() / 60
    )
}

/// What the fresh turn reads: which calls were open, to check and rerun. Sent
/// before the interrupt, so it claims nothing a failed interrupt makes false.
pub(super) fn cut_off_note(silence: &Silence, open_calls: &[OpenCall]) -> String {
    let waited = match silence.mail {
        true => " while the operator's mail waited",
        false => "",
    };
    let calls = match open_calls {
        [] => "No tool call was open, so your model request was what hung; carry on from \
               where you were."
            .to_owned(),
        calls => format!(
            "These tool calls were open then: {}. Any of them without a result was cut off \
             and its outcome is unknown: check whether it took effect, and run it again if it \
             still matters.",
            named(calls)
        ),
    };
    format!(
        "Archon is interrupting your turn: it made no progress for {} minutes{waited}. \
         {calls} Messages that waited for you follow.",
        minutes(silence)
    )
}

/// Every one of `calls`, each with its command and start: the fresh turn
/// can only check and rerun a call it is told of.
fn named(calls: &[OpenCall]) -> String {
    assert!(!calls.is_empty(), "only open calls are named");
    let named: Vec<String> = calls
        .iter()
        .map(|call| {
            format!(
                "`{}`{}, started {}",
                call.name,
                quoted_command(call),
                call.requested_at.format("%H:%M:%SZ")
            )
        })
        .collect();
    named.join("; ")
}

fn quoted_command(call: &OpenCall) -> String {
    let Some(command) = call.command.as_deref() else {
        return String::new();
    };
    let quoted: String = command.chars().take(COMMAND_QUOTE).collect();
    let cut = match quoted.len() < command.len() {
        true => "…",
        false => "",
    };
    format!(" running `{quoted}{cut}`")
}

/// The open call that kept the mail waiting longest, as the operator reads it.
fn longest_call(open_calls: &[OpenCall]) -> String {
    let Some(longest) = open_calls
        .iter()
        .max_by_key(|call| call.requested_at + call_bound(call))
    else {
        return ", with no tool call running".to_owned();
    };
    let bound = call_bound(longest).as_secs();
    let (whole, seconds) = (bound / 60, bound % 60);
    let bound = match seconds {
        0 => format!("{whole} min"),
        _ => format!("{whole} min {seconds} s"),
    };
    format!(", in a `{}` call past its bound of {bound}", longest.name)
}

/// What the operator reads after archond tried to interrupt a hung turn.
pub(super) fn interrupt_text(
    silence: &Silence,
    open_calls: &[OpenCall],
    outcome: &Interrupted,
) -> String {
    let grace = NUDGE_GRACE.as_secs() / 60;
    let head = match silence.mail {
        true => format!(
            "Operator mail waited on the orchestrator's turn, silent for {} minutes{}, and \
             Archon's nudge brought no progress in {grace} minutes",
            minutes(silence),
            longest_call(open_calls)
        ),
        false => format!(
            "The orchestrator's turn was silent for {} minutes, with no progress in the \
             {grace} minutes after Archon's nudge",
            minutes(silence)
        ),
    };
    let retry = INTERRUPT_SPACING.as_secs() / 60;
    match outcome {
        Interrupted::Stopped => format!(
            "{head}, so Archon interrupted it, as Esc in its terminal would. Messages waiting \
             for it, yours included, are delivered in a fresh turn, which is told which calls \
             were cut off. If it stays silent, run `archon orchestrator restart` to start a \
             fresh orchestrator."
        ),
        Interrupted::NoTurn => format!(
            "{head}. Archon tried to interrupt it, but Aether reports no turn running, so \
             nothing was interrupted. If it stays silent, run `archon orchestrator restart` \
             to start a fresh one."
        ),
        Interrupted::Failed(error) => format!(
            "{head}. Archon could not interrupt it ({error}) and tries again in {retry} \
             minutes; to recover it now, run `archon orchestrator restart` to start a fresh \
             one."
        ),
    }
}
