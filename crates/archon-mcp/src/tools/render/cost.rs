//! `cost_report` as text: one line per group, then the total, each with
//! what the budgets count first.

use std::fmt::Write as _;

use archon_core::cost::{CostAmounts, CostReport};

use super::usd;

pub(super) fn report(report: &CostReport) -> String {
    let range = &report.range;
    assert!(
        range.since <= range.until,
        "a report covers at least one day"
    );
    if report.rows.is_empty() {
        return format!("No spend recorded from {} to {}.", range.since, range.until);
    }
    let counted = match report.estimates_counted {
        true => "reported cost plus API-equivalent estimates",
        false => "reported cost only; separate API-equivalent estimates do not count",
    };
    let mut text = format!(
        "Spend from {} to {} by {}, as the budgets count it ({counted}):",
        range.since,
        range.until,
        report.by.name()
    );
    for row in &report.rows {
        let key = row.key.clone().unwrap_or_else(|| report.by.ownerless());
        let _ = write!(text, "\n- {key}: {}", amounts(&row.amounts));
    }
    let _ = write!(text, "\nTotal: {}.", amounts(&report.total));
    debug_assert_eq!(text.lines().count(), report.rows.len() + 2);
    text
}

/// Uncounted estimates also give the API-equivalent total, so unbilled
/// subscription usage, such as Codex, never reads as free.
fn amounts(amounts: &CostAmounts) -> String {
    let uncounted = amounts
        .uncounted_api_equivalent_usd()
        .map_or_else(String::new, |total| {
            format!("; ≈ {} API-equivalent", usd(total))
        });
    format!(
        "{} (billed {}, list-price/API estimate {}, unclassified {}{uncounted})",
        usd(amounts.counted_usd),
        usd(amounts.cost_usd),
        usd(amounts.estimated_cost_usd),
        usd(amounts.unclassified_cost_usd),
    )
}
