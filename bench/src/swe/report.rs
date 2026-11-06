//! The SWE section of an Archon release report: one table per gate
//! comparison (never worse than native, never worse than the best release)
//! and the arms' totals. Everything here is an aggregate.

use std::fmt::Write as _;

use super::aggregate::Aggregate;
use super::arms::{ArmKey, Harness};
use super::gate::{ALPHA, Comparison, GateReport, Reference, Verdict};
use super::stats;

/// The whole section, as Markdown. `old_label` names the best release.
pub fn render(report: &GateReport, aggregates: &[Aggregate], old_label: &str) -> String {
    assert!(!old_label.is_empty(), "the best release has a name");
    assert!(
        (report.alpha - ALPHA).abs() < f64::EPSILON,
        "the report uses the stated alpha"
    );
    let verdict = if report.passed() { "PASS" } else { "FAIL" };
    let mut out = format!("## SWE gate: {verdict}\n\n");
    let _ = writeln!(
        out,
        "Each row is an exact one-sided paired test (McNemar) of the candidate (`new`) \
         against the reference, at α = {ALPHA}. A row fails when the reference wins \
         significantly more of the discordant instances; an incomplete row also fails.\n"
    );
    out += &comparison_table(
        report,
        Reference::Native,
        "Never worse than native",
        "Native",
    );
    out += &comparison_table(
        report,
        Reference::BestRelease,
        &format!("Never worse than the best release ({old_label})"),
        old_label,
    );
    out += &arms_table(aggregates, old_label);
    out
}

fn comparison_table(report: &GateReport, reference: Reference, title: &str, other: &str) -> String {
    let mut out = format!("### {title}\n\n");
    let _ = writeln!(
        out,
        "| Model | Split | Paired | New only | {other} only | One-sided p | Verdict |\n\
         |---|---|---:|---:|---:|---:|---|"
    );
    let rows: Vec<&Comparison> = report.of(reference).collect();
    assert!(!rows.is_empty(), "every reference has rows");
    for c in rows {
        let (paired, new_only, other_only, p) = match c.paired {
            Some(p) => (
                p.n.to_string(),
                p.new_only.to_string(),
                p.reference_only.to_string(),
                format!("{:.3}", p.p_worse),
            ),
            None => ("–".into(), "–".into(), "–".into(), "–".into()),
        };
        let verdict = match c.verdict {
            Verdict::Pass => "pass".to_owned(),
            Verdict::Worse => "**FAIL: worse**".to_owned(),
            Verdict::Incomplete => format!("**FAIL: incomplete** ({})", c.reason),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {paired} | {new_only} | {other_only} | {p} | {verdict} |",
            c.model.name(),
            c.split
        );
    }
    out.push('\n');
    out
}

fn arms_table(aggregates: &[Aggregate], old_label: &str) -> String {
    let mut out = String::from(
        "### Arms\n\n\
         | Split | Arm | Passed | Pass rate (95% CI) | Cost per solved | Wall per solved | Infra |\n\
         |---|---|---:|---|---:|---:|---:|\n",
    );
    for aggregate in aggregates {
        for (key, t) in &aggregate.arms {
            let (low, high) = stats::wilson_95(t.passed, t.n);
            let rate = if t.n == 0 {
                0.0
            } else {
                f64::from(t.passed) / f64::from(t.n)
            };
            let cost =
                per_solved(t.cost_usd, t.passed).map_or("–".into(), |v| format!("${v:.3}"));
            let wall = per_solved(t.wall_s, t.passed).map_or("–".into(), |v| format!("{v:.0} s"));
            let _ = writeln!(
                out,
                "| {} | {} | {}/{} | {:.0}% ({:.0}–{:.0}%) | {} | {} | {} |",
                aggregate.split,
                arm_label(*key, old_label),
                t.passed,
                t.n,
                rate * 100.0,
                low * 100.0,
                high * 100.0,
                cost,
                wall,
                t.infra
            );
        }
    }
    out.push('\n');
    out
}

/// A total spread over the solved instances; nothing solved, or an unknown
/// total, has no such figure.
fn per_solved(total: Option<f64>, solved: u32) -> Option<f64> {
    total.filter(|_| solved > 0).map(|t| t / f64::from(solved))
}

fn arm_label(key: ArmKey, old_label: &str) -> String {
    match key.harness {
        Harness::Old => format!("{key} ({old_label})"),
        Harness::Native => format!("{key} ({})", key.model.native_cli()),
        Harness::New => key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::swe::aggregate::{SCHEMA, Split};
    use crate::swe::arms::Model;
    use crate::swe::gate;

    fn dev() -> Aggregate {
        let arm = |passed| json!({"n": 4, "passed": passed, "infra": 0, "cost_usd": 2.0, "wall_s": 800.0});
        let value = json!({
            "schema": SCHEMA, "split": "dev", "tasks": 4, "repeats": 1,
            "arms": {"haiku/old": arm(2), "haiku/new": arm(2), "haiku/native": arm(0)},
            "pairs": [
                {"a": "haiku/new", "b": "haiku/native", "n": 4, "a_only": 2, "b_only": 0},
                {"a": "haiku/new", "b": "haiku/old", "n": 4, "a_only": 1, "b_only": 1},
            ],
        });
        Aggregate::parse_saved(&value.to_string(), Split::Dev).unwrap()
    }

    #[test]
    fn the_report_has_one_table_per_reference_and_marks_the_missing_holdout() {
        let aggregates = [dev()];
        let report = gate::evaluate(&[Model::Haiku], &aggregates);
        let text = render(&report, &aggregates, "v1.4.9");
        assert!(text.starts_with("## SWE gate: FAIL"), "{text}");
        assert!(text.contains("### Never worse than native\n"));
        assert!(text.contains("### Never worse than the best release (v1.4.9)\n"));
        assert!(
            text.contains("| haiku | dev | 4 | 2 | 0 | 1.000 | pass |"),
            "{text}"
        );
        assert!(text.contains(
            "| haiku | holdout | – | – | – | – | **FAIL: incomplete** (no holdout aggregate) |"
        ));
        assert!(
            text.contains("| dev | haiku/old (v1.4.9) | 2/4 | 50% (15–85%) | $1.000 | 400 s | 0 |"),
            "{text}"
        );
        assert!(
            text.contains("| dev | haiku/native (claude-code) | 0/4 | 0% (0–49%) | – | – | 0 |"),
            "{text}"
        );
    }
}
