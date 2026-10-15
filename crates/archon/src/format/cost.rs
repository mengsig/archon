//! `archon cost`: a header naming the range and what counts, then one row
//! per group and the total, amounts counted as the budgets count them.

use archon_core::cost::{CostAmounts, CostReport};

use super::table;
use crate::wrap::sanitize_line;

pub fn cost(report: &CostReport) -> String {
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
        false => "reported cost; separate API-equivalent estimates do not count",
    };
    let header = format!(
        "Spend from {} to {} by {}; counted: {counted}.",
        range.since,
        range.until,
        report.by.name()
    );
    let mut cells = vec![[
        report.by.name().to_owned(),
        "counted".to_owned(),
        "billed".to_owned(),
        "list-price/API estimate".to_owned(),
        "unclassified".to_owned(),
    ]];
    for row in &report.rows {
        let key = row
            .key
            .as_deref()
            .map_or_else(|| "-".to_owned(), sanitize_line);
        cells.push(row_cells(key, &row.amounts));
    }
    cells.push(row_cells("total".to_owned(), &report.total));
    let mut text = format!("{header}\n{}", table(&cells));
    debug_assert_eq!(text.lines().count(), report.rows.len() + 3);
    // Uncounted estimates, such as Codex usage, must not read as free.
    if let Some(total) = report.total.uncounted_api_equivalent_usd() {
        text = format!("{text}\n≈ ${total:.2} API-equivalent with the estimates.");
    }
    text
}

fn row_cells(key: String, amounts: &CostAmounts) -> [String; 5] {
    [
        key,
        format!("${:.2}", amounts.counted_usd),
        format!("${:.2}", amounts.cost_usd),
        format!("${:.2}", amounts.estimated_cost_usd),
        format!("${:.2}", amounts.unclassified_cost_usd),
    ]
}

#[cfg(test)]
mod tests {
    use archon_core::cost::{CostGroup, CostRow, DayRange};

    use super::*;

    fn amounts(cost: f64, estimate: f64) -> CostAmounts {
        CostAmounts {
            counted_usd: cost + estimate,
            cost_usd: cost,
            estimated_cost_usd: estimate,
            unclassified_cost_usd: 0.0,
        }
    }

    fn report(rows: Vec<CostRow>, total: CostAmounts) -> CostReport {
        CostReport {
            range: serde_json::from_value::<DayRange>(
                serde_json::json!({"since": "2026-09-23", "until": "2026-09-29"}),
            )
            .unwrap(),
            by: CostGroup::Pipeline,
            estimates_counted: true,
            rows,
            total,
        }
    }

    #[test]
    fn a_report_is_a_table_with_the_total_last() {
        let rows = vec![
            CostRow {
                key: Some("default-pr-review".into()),
                amounts: amounts(10.0, 2.25),
            },
            CostRow {
                key: None,
                amounts: amounts(0.0, 1.5),
            },
        ];
        let text = cost(&report(rows, amounts(10.0, 3.75)));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "Spend from 2026-09-23 to 2026-09-29 by pipeline; counted: reported cost plus \
                 API-equivalent estimates.",
                "pipeline           counted  billed  list-price/API estimate  unclassified",
                "default-pr-review  $12.25   $10.00  $2.25                    $0.00",
                "-                  $1.50    $0.00   $1.50                    $0.00",
                "total              $13.75   $10.00  $3.75                    $0.00",
            ]
        );
    }

    #[test]
    fn list_prices_and_unknown_amounts_are_not_billed() {
        let amounts = CostAmounts {
            counted_usd: 2.0,
            cost_usd: 0.0,
            estimated_cost_usd: 2.0,
            unclassified_cost_usd: 1.0,
        };
        let mut report = report(vec![CostRow { key: None, amounts }], amounts);
        report.estimates_counted = false;
        let text = cost(&report);
        assert!(text.contains("separate API-equivalent estimates do not count"));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "total     $2.00    $0.00   $2.00                    $1.00",
                "≈ $3.00 API-equivalent with the estimates.",
            ]
        );
    }

    #[test]
    fn an_empty_range_says_so() {
        let text = cost(&report(vec![], CostAmounts::default()));
        assert_eq!(text, "No spend recorded from 2026-09-23 to 2026-09-29.");
    }
}
