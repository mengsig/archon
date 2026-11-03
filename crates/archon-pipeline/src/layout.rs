//! The order of a pull request body Archon delivers, top to bottom: the risk
//! level and what changed at a high level, the evidence, the body's own text
//! (the technical detail), then the risk section's reasons and evidence.
//! Archon owns only what lies between its markers; a later delivery replaces
//! each of its sections and leaves the rest of the body as it is.

use archon_core::risk::RiskLevel;

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

/// The summary section: the risk level, then what changed at a high level
/// as stage `by` said in `summary`, or that it said nothing.
pub fn summary_section(level: RiskLevel, by: &str, summary: Option<&str>) -> String {
    assert!(!by.is_empty(), "a stage wrote the change");
    let summary = summary.map(str::trim).filter(|s| !s.is_empty());
    let what = match summary {
        Some(summary) => defused(summary),
        None => format!("_No high-level summary: `{by}` gave no `pr.summary`._"),
    };
    let text =
        format!("{SUMMARY_START}\nRisk level: {level}\n\n## What changed\n\n{what}\n{SUMMARY_END}");
    debug_assert_eq!(text.matches(SUMMARY_START).count(), 1, "one section");
    text
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
        while let Some(from) = rest.find(start) {
            let Some(to) = rest[from..].find(end) else {
                break;
            };
            let to = from + to + end.len();
            rest = format!("{}\n\n{}", rest[..from].trim_end(), rest[to..].trim_start());
        }
    }
    rest
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
