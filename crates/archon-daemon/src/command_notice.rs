//! Shows the result of a host command as a note row in the orchestrator's
//! transcript, at once and without a model turn.

use archon_aether::wire::{NOTE_LEVEL_PROTOCOL_VERSION, NOTES_FEATURE, NoteLevel};

use crate::context::DaemonContext;
use crate::executor::clip;
use crate::supervisor;

const MAX_NOTE_CHARS: usize = 500;

/// The one-line note for `label` (e.g. `/answer Q5`): coloured by `level` when
/// the aetherd takes one, else led by the word the colour would have said.
pub(crate) fn note_line(label: &str, applied: bool, text: &str, has_level: bool) -> String {
    assert!(!label.trim().is_empty(), "a result names its command");
    assert!(!text.trim().is_empty(), "a result says something");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = match (has_level, applied) {
        (true, _) => format!("{label}: {text}"),
        (false, true) => format!("Applied: {label}: {text}"),
        (false, false) => format!("Refused: {label}: {text}"),
    };
    clip(&line, MAX_NOTE_CHARS)
}

/// Best effort: the outcome is already in Archon's feed, so a note that cannot
/// be added is logged and never fails the command. Nothing is shown when
/// aetherd lacks notes or no orchestrator exists yet.
pub(crate) async fn show(ctx: &DaemonContext, label: &str, applied: bool, text: &str) {
    let Ok(client) = ctx.aether.client() else {
        return;
    };
    let welcome = client.welcome();
    if !welcome.features.iter().any(|f| f == NOTES_FEATURE) {
        return;
    }
    let orchestrator = match supervisor::current_orchestrator(ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "no orchestrator to show a command result to");
            return;
        }
    };
    let has_level = welcome.protocol_version >= NOTE_LEVEL_PROTOCOL_VERSION;
    let level = match applied {
        true => NoteLevel::Applied,
        false => NoteLevel::Refused,
    };
    let line = note_line(label, applied, text, has_level);
    if let Err(error) = client.append_note(&orchestrator, &line, Some(level)).await {
        tracing::warn!(%error, command = label, "a command result was not shown in the TUI");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_note_leads_with_the_word_a_level_would_colour() {
        let refused = note_line("/answer Q5", false, "already answered", false);
        assert_eq!(refused, "Refused: /answer Q5: already answered");
        let applied = note_line("/answer Q5", true, "answered", false);
        assert_eq!(applied, "Applied: /answer Q5: answered");
    }

    #[test]
    fn a_levelled_note_has_no_word_and_one_line() {
        let line = note_line("/merge", false, "no such\nitem", true);
        assert_eq!(line, "/merge: no such item");
        let long = note_line("/merge", false, &"x".repeat(5000), true);
        assert_eq!(long.chars().count(), MAX_NOTE_CHARS);
    }
}
