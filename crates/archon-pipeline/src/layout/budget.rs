//! Fit the delivered body by shortening technical detail before risk details.

use super::{RISK_END, RISK_START, compose, without_owned};

pub use crate::ship::MAX_BODY_CHARS;

/// Compose a PR, preserving its top sections and the risk section's wrapper.
/// If the protected text cannot fit, return the original for the write guard.
pub fn compose_pr(body: &str, top: &[&str], risk: &str) -> String {
    assert!(risk.starts_with(RISK_START) && risk.ends_with(RISK_END));
    let original = compose(body, top, &[risk]);
    let excess = original.chars().count().saturating_sub(MAX_BODY_CHARS);
    if excess == 0 {
        return original;
    }
    let own = without_owned(body);
    let own = own.trim();
    let (heading, detail) = own
        .strip_prefix("## Technical detail\n\n")
        .map_or(("", own), |detail| ("## Technical detail\n\n", detail));
    let shortened = shorten(detail, detail.chars().count().saturating_sub(excess));
    let technical = format!("{heading}{shortened}");
    let text = compose(&technical, top, &[risk]);
    let excess = text.chars().count().saturating_sub(MAX_BODY_CHARS);
    if excess == 0 {
        return text;
    }
    let Some((prefix, details)) = risk.split_once("</summary>\n") else {
        return original;
    };
    let Some((details, suffix)) = details.rsplit_once("\n</details>") else {
        return original;
    };
    let shortened = shorten(details, details.chars().count().saturating_sub(excess));
    let risk = format!("{prefix}</summary>\n{shortened}\n</details>{suffix}");
    let text = compose(&technical, top, &[&risk]);
    if text.chars().count() > MAX_BODY_CHARS {
        return original;
    }
    debug_assert!(top.first().is_none_or(|first| text.starts_with(first)));
    debug_assert!(text.ends_with(&format!("{RISK_END}\n")));
    text
}

fn shorten(text: &str, budget: usize) -> String {
    let total = text.chars().count();
    assert!(budget <= total, "shortening never adds a budget");
    if budget == total {
        return text.to_owned();
    }
    let mut kept = budget;
    loop {
        let separator = if kept == 0 { "" } else { "\n\n" };
        let note = format!(
            "{separator}(truncated: {} chars omitted; full text in the run summary)",
            total - kept
        );
        let size = kept + note.chars().count();
        if size <= budget || kept == 0 {
            let prefix: String = text.chars().take(kept).collect();
            debug_assert!(kept < total, "the note counts omitted characters");
            return format!("{prefix}{note}");
        }
        kept = kept.saturating_sub(size - budget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{EVIDENCE_END, EVIDENCE_START, summary_section, wrap};
    use archon_core::risk::RiskLevel;

    fn sections(details: usize) -> (String, String, String) {
        let summary = summary_section(
            RiskLevel::Medium,
            "implement",
            Some("Fix three behaviours."),
        );
        let evidence = wrap(EVIDENCE_START, EVIDENCE_END, &"e".repeat(16_000));
        let risk = wrap(
            RISK_START,
            RISK_END,
            &format!(
                "## Risk: medium\n\n<details>\n<summary>Why</summary>\n{}\n</details>",
                "r".repeat(details)
            ),
        );
        (summary, evidence, risk)
    }

    #[test]
    fn bodies_just_under_at_and_over_the_limit_keep_the_top_sections() {
        let (summary, evidence, risk) = sections(2_000);
        let top = [&*summary, &*evidence];
        let overhead = compose("## Technical detail\n\nx", &top, &[&risk])
            .chars()
            .count()
            - 1;
        for target in [MAX_BODY_CHARS - 1, MAX_BODY_CHARS, MAX_BODY_CHARS + 1] {
            let detail = "x".repeat(target - overhead);
            let body = format!("## Technical detail\n\n{detail}");
            let full = compose(&body, &top, &[&risk]);
            assert_eq!(full.chars().count(), target);
            let fitted = compose_pr(&body, &top, &risk);
            assert!(fitted.chars().count() <= MAX_BODY_CHARS);
            assert!(fitted.starts_with(&format!("{summary}\n\n{evidence}")));
            assert!(fitted.ends_with(&format!("{risk}\n")), "risk unchanged");
            if target <= MAX_BODY_CHARS {
                assert_eq!(fitted, full);
            } else {
                let prefix = "(truncated: ";
                let omitted: usize = fitted
                    .split_once(prefix)
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                let retained = fitted
                    .split_once("## Technical detail\n\n")
                    .unwrap()
                    .1
                    .split_once(prefix)
                    .unwrap()
                    .0
                    .trim_end();
                assert_eq!(retained.chars().count() + omitted, detail.len());
                assert!(fitted.contains("chars omitted; full text in the run summary)"));
            }
        }
    }

    #[test]
    fn risk_details_are_shortened_only_after_technical_detail_is_spent() {
        let (summary, evidence, risk) = sections(MAX_BODY_CHARS);
        let body = "## Technical detail\n\nTechnical text";
        let fitted = compose_pr(body, &[&summary, &evidence], &risk);
        assert!(fitted.chars().count() <= MAX_BODY_CHARS);
        assert!(fitted.contains("## Technical detail\n\n(truncated: 14 chars omitted;"));
        assert_eq!(fitted.matches("(truncated:").count(), 2);
        assert!(fitted.ends_with(&format!("\n</details>\n{RISK_END}\n")));
        assert!(fitted.contains(&evidence));
    }

    #[test]
    fn protected_sections_that_cannot_fit_leave_the_original_for_the_guard() {
        let (summary, _, risk) = sections(10);
        let evidence = wrap(EVIDENCE_START, EVIDENCE_END, &"e".repeat(MAX_BODY_CHARS));
        let top = [&*summary, &*evidence];
        let body = "## Technical detail\n\ntext";
        assert_eq!(compose_pr(body, &top, &risk), compose(body, &top, &[&risk]));
        assert!(compose_pr(body, &top, &risk).chars().count() > MAX_BODY_CHARS);
    }

    #[test]
    fn truncation_counts_unicode_characters_and_keeps_valid_utf8() {
        let (summary, evidence, risk) = sections(10);
        let body = format!("## Technical detail\n\n{}", "🦀".repeat(MAX_BODY_CHARS));
        let fitted = compose_pr(&body, &[&summary, &evidence], &risk);
        assert!(fitted.chars().count() <= MAX_BODY_CHARS);
        assert!(fitted.contains('🦀') && fitted.contains("(truncated:"));
        assert!(fitted.contains(&evidence) && fitted.contains(&risk));
    }
}
