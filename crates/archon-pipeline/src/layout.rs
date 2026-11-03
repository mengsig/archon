//! The order of a pull request body Archon delivers, top to bottom: the risk
//! level and what changed at a high level, the evidence, the body's own text
//! (the technical detail), then the risk section's reasons and evidence.
//! Archon owns only what lies between its markers; a later delivery replaces
//! each of its sections and leaves the rest of the body as it is.

use archon_core::risk::RiskLevel;

mod budget;
pub use budget::{MAX_BODY_CHARS, compose_pr};

pub const SUMMARY_START: &str = "<!-- archon-summary -->";
pub const SUMMARY_END: &str = "<!-- /archon-summary -->";
pub const EVIDENCE_START: &str = "<!-- archon-evidence -->";
pub const EVIDENCE_END: &str = "<!-- /archon-evidence -->";
pub const RISK_START: &str = "<!-- archon-risk -->";
pub const RISK_END: &str = "<!-- /archon-risk -->";
/// Every section Archon owns, by its markers.
const OWNED: [(&str, &str); 3] = [
    (SUMMARY_START, SUMMARY_END),
    (EVIDENCE_START, EVIDENCE_END),
    (RISK_START, RISK_END),
];

/// `text`, an agent's, with no HTML comment opener left to pass for a marker.
pub fn defused(text: &str) -> String {
    text.replace("<!--", "<! --")
}

/// The section `inner` between the markers `start` and `end`, each on a line
/// of its own; nothing inside can pass for a marker.
pub fn wrap(start: &str, end: &str, inner: &str) -> String {
    assert!(
        OWNED.contains(&(start, end)),
        "a pair of Archon's markers: {start}"
    );
    let text = format!("{start}\n{}\n{end}", defused(inner.trim()));
    debug_assert_eq!(text.matches("<!--").count(), 2, "only its own markers");
    text
}

/// The summary section: the risk level, then what changed at a high level
/// as stage `by` said in `summary`, or that it said nothing.
pub fn summary_section(level: RiskLevel, by: &str, summary: Option<&str>) -> String {
    assert!(!by.is_empty(), "a stage wrote the change");
    let summary = summary.map(str::trim).filter(|s| !s.is_empty());
    let what = match summary {
        Some(summary) => summary.to_owned(),
        None => format!("_No high-level summary: `{by}` gave no `pr.summary`._"),
    };
    let inner = format!("Risk level: {level}\n\n## What changed\n\n{what}");
    wrap(SUMMARY_START, SUMMARY_END, &inner)
}

/// `body` with `top` sections above its own text and `bottom` ones below,
/// each replacing the one Archon wrote before; a start marker with no end
/// is left in place, as the body's own text.
pub fn compose(body: &str, top: &[&str], bottom: &[&str]) -> String {
    for section in top.iter().chain(bottom) {
        assert!(
            OWNED
                .iter()
                .any(|(start, end)| section.starts_with(start) && section.ends_with(end)),
            "an Archon section: {section:?}"
        );
    }
    let own = without_owned(body);
    let parts: Vec<&str> = top
        .iter()
        .copied()
        .chain([own.trim()])
        .chain(bottom.iter().copied())
        .filter(|part| !part.trim().is_empty())
        .collect();
    let composed = format!("{}\n", parts.join("\n\n"));
    debug_assert!(
        top.first().is_none_or(|first| composed.starts_with(first)),
        "the top leads"
    );
    composed
}

/// `body` without any section Archon wrote, the rest as it was.
fn without_owned(body: &str) -> String {
    let mut rest = body.to_owned();
    for (start, end) in OWNED {
        let mut removed = 0;
        while let Some((from, to)) = owned_span(&rest, start, end) {
            assert!(from < to && to <= rest.len(), "a span of the body");
            rest = format!("{}\n\n{}", rest[..from].trim_end(), rest[to..].trim_start());
            removed += 1;
            assert!(removed <= body.len(), "each pass removes a start marker");
        }
    }
    rest
}

/// The byte span of the first section between `start` and `end` in `body`:
/// the first end-marker line with a start-marker line before it, and the
/// nearest such start, so a stray marker never takes the text around it.
fn owned_span(body: &str, start: &str, end: &str) -> Option<(usize, usize)> {
    let starts = marker_lines(body, start);
    marker_lines(body, end)
        .into_iter()
        .find_map(|(to, to_end)| {
            let from = starts.iter().rev().find(|(from, _)| *from < to)?;
            Some((from.0, to_end))
        })
}

/// The byte spans of the lines of `text` that hold `marker` alone, give or
/// take surrounding whitespace (GitHub may hand back `\r\n` line ends).
fn marker_lines(text: &str, marker: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if content.trim() == marker {
            spans.push((offset, offset + content.len()));
        }
        offset += line.len();
    }
    debug_assert_eq!(offset, text.len(), "every line counted");
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(text: &str) -> String {
        format!("{RISK_START}\n{text}\n{RISK_END}")
    }

    #[test]
    fn sections_replace_their_old_selves_and_keep_the_owner_s_text() {
        let summary = summary_section(RiskLevel::Low, "implement", Some("made it fast"));
        let body = compose("The owner's text.", &[&summary], &[&risk("one")]);
        assert_eq!(
            body,
            format!("{summary}\n\nThe owner's text.\n\n{}\n", risk("one"))
        );
        let again = compose(&body, &[&summary], &[&risk("two")]);
        assert_eq!(
            again,
            format!("{summary}\n\nThe owner's text.\n\n{}\n", risk("two"))
        );
        assert_eq!(
            compose(&again, &[&summary], &[&risk("two")]),
            again,
            "idempotent"
        );
        let old = format!("{}\n\nWhat and why.", risk("old, on top"));
        let moved = compose(&old, &[&summary], &[&risk("new")]);
        assert_eq!(
            moved,
            format!("{summary}\n\nWhat and why.\n\n{}\n", risk("new"))
        );
        assert_eq!(compose("", &[], &[&risk("x")]), format!("{}\n", risk("x")));
    }

    #[test]
    fn a_start_marker_with_no_end_stays_as_the_owner_s_text() {
        let broken = format!("{RISK_START} what the owner wrote");
        let body = compose(&broken, &[], &[&risk("x")]);
        assert_eq!(body, format!("{broken}\n\n{}\n", risk("x")));
        let alone = format!("{RISK_START}\nThe owner's text.");
        let body = compose(&alone, &[], &[&risk("x")]);
        assert_eq!(body, format!("{alone}\n\n{}\n", risk("x")));
        let again = compose(&body, &[], &[&risk("y")]);
        assert_eq!(again, format!("{alone}\n\n{}\n", risk("y")), "kept");
    }

    #[test]
    fn markers_count_only_on_lines_of_their_own() {
        let inline = format!("Mentions {RISK_END} and {RISK_START} in a line.");
        let body = compose(&inline, &[], &[&risk("x")]);
        assert_eq!(body, format!("{inline}\n\n{}\n", risk("x")));
        let stray_end = format!("{RISK_END}\nThe owner's text.");
        let body = compose(&stray_end, &[], &[&risk("x")]);
        assert_eq!(
            compose(&body, &[], &[&risk("y")]),
            format!("{stray_end}\n\n{}\n", risk("y"))
        );
        let crlf = format!("{RISK_START}\r\nold\r\n{RISK_END}\r\nText.");
        assert_eq!(
            compose(&crlf, &[], &[&risk("z")]),
            format!("Text.\n\n{}\n", risk("z"))
        );
    }

    #[test]
    fn wrapped_text_cannot_end_or_forge_a_section() {
        let forged = format!("a `{RISK_END}` check\n{RISK_END}\n{RISK_START}\nmore");
        let section = wrap(RISK_START, RISK_END, &forged);
        assert_eq!(section.matches("<!--").count(), 2, "{section}");
        let body = compose("Text.", &[], &[&section]);
        let twice = compose(&compose(&body, &[], &[&section]), &[], &[&section]);
        assert_eq!(twice, body, "replaced in place, never piled up");
        assert_eq!(twice.matches(RISK_START).count(), 1);
    }

    #[test]
    fn the_summary_leads_with_the_level_and_says_when_none_was_given() {
        let given = summary_section(
            RiskLevel::Medium,
            "build",
            Some(" before: slow. now: fast <!-- /archon-summary --> "),
        );
        assert!(given.starts_with(&format!(
            "{SUMMARY_START}\nRisk level: medium\n\n## What changed\n\nbefore: slow"
        )));
        assert_eq!(
            given.matches("<!--").count(),
            2,
            "an agent's marker is defused: {given}"
        );
        let missing = summary_section(RiskLevel::High, "build", Some("  "));
        assert!(
            missing.contains("_No high-level summary: `build` gave no `pr.summary`._"),
            "{missing}"
        );
    }
}
