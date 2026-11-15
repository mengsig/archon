//! What the operator and the orchestrator read about a silent turn.

use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::ports::OpenToolCall;

use super::{Hung, INTERRUPT_SPACING, MAX_NUDGES, NUDGE_GRACE, call_bound};

/// How Archon's nudge to a silent turn begins.
const NUDGE_PREFIX: &str = "Continue: your turn has made no progress";
/// How Archon's note to an interrupted orchestrator begins.
const CUT_OFF_PREFIX: &str = "Archon interrupted your turn";
/// The longest command a note quotes.
const COMMAND_QUOTE: usize = 300;

pub(super) enum Nudge {
    Sent,
    Spent,
    Failed(ArchonError),
}

/// What the interrupt of a hung turn did.
pub(super) enum Interrupted {
    /// `told` is whether the orchestrator was told which call was cut off.
    Stopped {
        told: Result<(), ArchonError>,
    },
    NoTurn,
    Failed(ArchonError),
}

pub(super) fn nudge_text(silent: Duration) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "a nudge follows minutes of silence");
    format!(
        "{NUDGE_PREFIX} for {minutes} minutes. If you ended it with work still pending, such \
         as watching a release build, pick that work up now."
    )
}

/// What the operator reads of a stall; `nudge` says how the nudge went.
pub(super) fn stall_text(silent: Duration, nudge: Nudge) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "a stall is minutes long");
    let nudge = match nudge {
        Nudge::Sent => "Archon sent it a nudge to continue.".to_owned(),
        Nudge::Spent => {
            format!("Archon has used its {MAX_NUDGES} nudge attempts on it and sends no more.")
        }
        Nudge::Failed(error) => format!("Archon could not nudge it ({error})."),
    };
    let grace = NUDGE_GRACE.as_secs() / 60;
    format!(
        "The orchestrator has been in one turn for {minutes} minutes without a message, tool \
         result or usage update; its model request or a tool call may be hung, and nothing \
         will wake it by itself. {nudge} If it is still silent in {grace} minutes, Archon \
         interrupts the turn, as Esc in its terminal would, so that waiting messages reach a \
         fresh turn; if that fails, run `archon orchestrator restart` to start a fresh one."
    )
}

/// What the orchestrator reads in the fresh turn after an interrupt: which
/// call was cut off, so it can check and rerun it.
pub(super) fn cut_off_note(
    hung: &Hung,
    call: &Result<Option<OpenToolCall>, ArchonError>,
) -> String {
    let minutes = hung.silent.as_secs() / 60;
    assert!(minutes > 0, "an interrupt follows minutes of silence");
    let waited = if hung.mail {
        ", while the operator's mail waited"
    } else {
        ""
    };
    let call = match call {
        Ok(Some(call)) => format!(
            "The tool call cut off was `{}`{}, started {}; its outcome is unknown, so check \
             whether it took effect, and run it again if it still matters.",
            call.name,
            quoted_command(call),
            call.requested_at.format("%H:%M:%SZ"),
        ),
        Ok(None) => "No tool call was open, so your model request was what hung; carry on \
                     from where you were."
            .to_owned(),
        Err(error) => format!(
            "Archon could not read which tool call was open ({error}); if you were running \
             one, check whether it took effect, and run it again if it still matters."
        ),
    };
    let note = format!(
        "{CUT_OFF_PREFIX}: it was silent for {minutes} minutes{waited}. {call} Messages that \
         waited for you are in your inbox."
    );
    assert!(
        note.starts_with(CUT_OFF_PREFIX),
        "the note says who interrupted"
    );
    note
}

fn quoted_command(call: &OpenToolCall) -> String {
    let Some(command) = call.command.as_deref() else {
        return String::new();
    };
    let quoted: String = command.chars().take(COMMAND_QUOTE).collect();
    let cut = if quoted.len() < command.len() {
        "…"
    } else {
        ""
    };
    format!(" running `{quoted}{cut}`")
}

/// `span` in minutes, and seconds where it has any.
fn span_text(span: Duration) -> String {
    let (minutes, seconds) = (span.as_secs() / 60, span.as_secs() % 60);
    match seconds {
        0 => format!("{minutes} min"),
        _ => format!("{minutes} min {seconds} s"),
    }
}

/// What the operator reads after archond tried to interrupt a hung turn.
pub(super) fn interrupt_text(
    hung: &Hung,
    call: &Result<Option<OpenToolCall>, ArchonError>,
    outcome: &Interrupted,
) -> String {
    let minutes = hung.silent.as_secs() / 60;
    assert!(minutes > 0, "an interrupt follows minutes of silence");
    let running = match call {
        Ok(Some(call)) => format!(
            ", in a `{}` call past its bound of {}",
            call.name,
            span_text(call_bound(call))
        ),
        Ok(None) => ", with no tool call running".to_owned(),
        Err(_) => String::new(),
    };
    let head = match hung.mail {
        true => format!(
            "Operator mail waited on the orchestrator's turn, silent for {minutes} \
             minutes{running}"
        ),
        false => format!(
            "The orchestrator's turn was silent for {minutes} minutes{running}, with no \
             progress in the {} minutes after Archon's nudge",
            NUDGE_GRACE.as_secs() / 60
        ),
    };
    let retry = INTERRUPT_SPACING.as_secs() / 60;
    let text = match outcome {
        Interrupted::Stopped { .. } => format!(
            "{head}, so Archon interrupted it, as Esc in its terminal would. Messages waiting \
             for it, yours included, are delivered in a fresh turn, which is told which call \
             was cut off. If it stays silent, run `archon orchestrator restart` to start a \
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
    };
    match outcome {
        Interrupted::Stopped { told: Err(error) } => {
            format!("{text} Archon could not tell it which call was cut off ({error}).")
        }
        _ => text,
    }
}

#[cfg(test)]
mod tests;
