//! Shows the result of a host command in the orchestrator's transcript, at
//! once and without a model turn: a feed line archond posts to the
//! conversation where aetherd keeps posts, else a note row.

use archon_aether::wire::{NOTE_LEVEL_PROTOCOL_VERSION, NOTES_FEATURE, NoteLevel, Welcome};
use archon_core::error::ArchonError;

use crate::context::DaemonContext;
use crate::executor::clip;
use crate::{feed, supervisor};

const MAX_NOTE_CHARS: usize = 500;

/// The one-line note for `label` (e.g. `/answer Q5`): coloured by `level` when
/// the aetherd takes one, else led by the word the colour would have said.
/// A plain answer (`None`) has neither.
pub(crate) fn note_line(
    label: &str,
    level: Option<NoteLevel>,
    text: &str,
    has_level: bool,
) -> String {
    assert!(!label.trim().is_empty(), "a result names its command");
    assert!(!text.trim().is_empty(), "a result says something");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = match (has_level, level) {
        (true, _) | (false, None) => format!("{label}: {text}"),
        (false, Some(NoteLevel::Applied)) => format!("Applied: {label}: {text}"),
        (false, Some(NoteLevel::Refused)) => format!("Refused: {label}: {text}"),
    };
    clip(&line, MAX_NOTE_CHARS)
}

/// One start-up line when aetherd cannot show command results, so their
/// absence from the TUI is explained.
pub(crate) fn warn_if_unsupported(welcome: &Welcome) {
    if welcome.features.iter().any(|f| f == NOTES_FEATURE) {
        return;
    }
    tracing::warn!(
        protocol = welcome.protocol_version,
        "Aether lacks notes; the TUI will not show host command results (they stay in Archon's feed)"
    );
}

/// Where a command's result line went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shown {
    /// Into the feed, which posts it to the conversation in order.
    InFeed,
    /// Straight into the transcript as a note, for an aetherd without posts.
    AsNote,
    /// Nowhere: no notes, no orchestrator, or the note failed (logged).
    Not,
}

/// Show an applied or refused command result; see [`show_level`].
pub(crate) async fn show(
    ctx: &DaemonContext,
    label: &str,
    applied: bool,
    text: &str,
) -> Result<Shown, ArchonError> {
    let level = match applied {
        true => NoteLevel::Applied,
        false => NoteLevel::Refused,
    };
    show_level(ctx, label, Some(level), text).await
}

/// The result line, at any level (`None` being a plain answer). Where
/// aetherd keeps posts it goes into the feed, so it reaches the conversation
/// in order and survives a restart; else it is a best-effort note.
pub(crate) async fn show_level(
    ctx: &DaemonContext,
    label: &str,
    level: Option<NoteLevel>,
    text: &str,
) -> Result<Shown, ArchonError> {
    let Ok(client) = ctx.aether.client() else {
        return Ok(Shown::Not);
    };
    if client.serves_conversation() {
        // A post carries no level, so its line leads with the word instead.
        let line = note_line(label, level, text, false);
        feed::system(ctx, line).await?;
        return Ok(Shown::InFeed);
    }
    let welcome = client.welcome();
    if !welcome.features.iter().any(|f| f == NOTES_FEATURE) {
        return Ok(Shown::Not);
    }
    let orchestrator = match supervisor::current_orchestrator(ctx).await {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(Shown::Not),
        Err(error) => {
            tracing::warn!(%error, "no orchestrator to show a command result to");
            return Ok(Shown::Not);
        }
    };
    let has_level = welcome.protocol_version >= NOTE_LEVEL_PROTOCOL_VERSION;
    let line = note_line(label, level, text, has_level);
    match client.append_note(&orchestrator, &line, level).await {
        Ok(()) => Ok(Shown::AsNote),
        Err(error) => {
            tracing::warn!(%error, command = label, "a command result was not shown in the TUI");
            Ok(Shown::Not)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn welcome(features: &[&str]) -> Welcome {
        serde_json::from_value(serde_json::json!({
            "protocol_version": 23, "daemon_version": "1.0.0", "instance_id": "i",
            "schema_version": 1, "pid": 1, "features": features,
        }))
        .expect("a valid welcome")
    }

    fn logged_by(welcome: &Welcome) -> String {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || warn_if_unsupported(welcome));
        let written = bytes.lock().unwrap().clone();
        String::from_utf8(written).unwrap()
    }

    #[test]
    fn an_aetherd_without_notes_logs_one_start_up_line() {
        let text = logged_by(&welcome(&[]));
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("Aether lacks notes"), "{text}");
    }

    #[test]
    fn an_aetherd_with_notes_logs_nothing() {
        let text = logged_by(&welcome(&["notes"]));
        assert!(text.is_empty(), "{text}");
    }

    #[test]
    fn a_plain_note_leads_with_the_word_a_level_would_colour() {
        let refused = note_line(
            "/answer Q5",
            Some(NoteLevel::Refused),
            "already answered",
            false,
        );
        assert_eq!(refused, "Refused: /answer Q5: already answered");
        let applied = note_line("/answer Q5", Some(NoteLevel::Applied), "answered", false);
        assert_eq!(applied, "Applied: /answer Q5: answered");
        let plain = note_line("/status", None, "all\nquiet", false);
        assert_eq!(plain, "/status: all quiet", "an answer has no word");
    }

    #[test]
    fn a_levelled_note_has_no_word_and_one_line() {
        let refused = Some(NoteLevel::Refused);
        let line = note_line("/merge", refused, "no such\nitem", true);
        assert_eq!(line, "/merge: no such item");
        let long = note_line("/merge", refused, &"x".repeat(5000), true);
        assert_eq!(long.chars().count(), MAX_NOTE_CHARS);
    }
}
