//! `archon cost`: a header naming the range and what counts, then one row
//! per group and the total, amounts counted as the budgets count them.

use archon_core::cost::{CostAmounts, CostReport, format_usd};

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
            .map_or_else(|| report.by.ownerless(), sanitize_line);
        cells.push(row_cells(key, &row.amounts));
    }
    cells.push(row_cells("total".to_owned(), &report.total));
    let mut text = format!("{header}\n{}", cost_table(cells, report));
    debug_assert_eq!(text.lines().count(), report.rows.len() + 3);
    // Uncounted estimates, such as Codex usage, must not read as free.
    if let Some(total) = report.total.uncounted_api_equivalent_usd() {
        text = format!(
            "{text}\n≈ {} API-equivalent with the estimates.",
            format_usd(total)
        );
    }
    text
}

fn cost_table(cells: Vec<[String; 5]>, report: &CostReport) -> String {
    let Some(total) = report.split else {
        return table(&cells);
    };
    assert_eq!(cells.len(), report.rows.len() + 2);
    let splits = report.rows.iter().map(|row| row.split).chain([Some(total)]);
    let mut cells = cells.into_iter();
    let [key, counted, billed, estimate, unclassified] = cells.next().expect("a cost header");
    let mut with_split = vec![[
        key,
        counted,
        billed,
        estimate,
        unclassified,
        "lead".into(),
        "delegated".into(),
    ]];
    for ([key, counted, billed, estimate, unclassified], split) in cells.zip(splits) {
        let [lead, delegated] = split.map_or_else(
            || ["-".into(), "-".into()],
            |split| [format_usd(split.lead_usd), format_usd(split.delegated_usd)],
        );
        with_split.push([
            key,
            counted,
            billed,
            estimate,
            unclassified,
            lead,
            delegated,
        ]);
    }
    table(&with_split)
}

fn row_cells(key: String, amounts: &CostAmounts) -> [String; 5] {
    [
        key,
        format_usd(amounts.counted_usd),
        format_usd(amounts.cost_usd),
        format_usd(amounts.estimated_cost_usd),
        format_usd(amounts.unclassified_cost_usd),
    ]
}

#[cfg(test)]
mod tests {
    use archon_core::cost::{CostGroup, CostRow, CostSplit, DayRange};

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
            split: None,
        }
    }

    #[test]
    fn a_report_is_a_table_with_the_total_last() {
        let rows = vec![
            CostRow {
                key: Some("default-pr-review".into()),
                amounts: amounts(10.0, 2.25),
                split: None,
            },
            CostRow {
                key: None,
                amounts: amounts(0.0, 1.5),
                split: None,
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
                "(orchestrator)     $1.50    $0.00   $1.50                    $0.00",
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
        let mut report = report(
            vec![CostRow {
                key: None,
                amounts,
                split: None,
            }],
            amounts,
        );
        report.estimates_counted = false;
        let text = cost(&report);
        assert!(text.contains("separate API-equivalent estimates do not count"));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "total           $2.00    $0.00   $2.00                    $1.00",
                "≈ $3.00 API-equivalent with the estimates.",
            ]
        );
    }

    #[test]
    fn the_cost_table_shows_lead_and_delegated_amounts() {
        let split = Some(CostSplit {
            lead_usd: 2.0,
            delegated_usd: 1.5,
        });
        let mut report = report(
            vec![CostRow {
                key: Some("high".into()),
                amounts: amounts(2.0, 1.5),
                split,
            }],
            amounts(2.0, 1.5),
        );
        report.by = CostGroup::Effort;
        report.split = split;
        let text = cost(&report);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines[1]
                .split_whitespace()
                .rev()
                .take(2)
                .eq(["delegated", "lead"])
        );
        for (line, key) in lines[2..].iter().zip(["high", "total"]) {
            assert!(line.starts_with(key), "{text}");
            assert!(
                line.split_whitespace().rev().take(2).eq(["$1.50", "$2.00"]),
                "{text}"
            );
        }
    }

    #[test]
    fn sub_cent_rows_are_marked_not_rounded_to_zero() {
        let row = |key: &str| CostRow {
            key: Some(key.into()),
            amounts: amounts(0.004, 0.0),
            split: None,
        };
        let text = cost(&report(
            vec![row("a"), row("b"), row("c")],
            amounts(0.012, 0.0),
        ));
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[2..5] {
            let cells: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(cells[1..3], ["<$0.01", "<$0.01"], "{line}");
        }
        assert!(lines[5].starts_with("total  "), "{text}");
        assert!(lines[5].contains("$0.01"), "{text}");
    }

    #[test]
    fn spend_with_no_owner_names_what_it_lacks() {
        let mut report = report(
            vec![CostRow {
                key: None,
                amounts: amounts(1.0, 0.0),
                split: None,
            }],
            amounts(1.0, 0.0),
        );
        report.by = CostGroup::Item;
        let text = cost(&report);
        assert!(text.contains("\n(orchestrator)  $1.00"), "{text}");
        assert!(!text.contains("\n- "), "{text}");
    }

    #[test]
    fn an_empty_range_says_so() {
        let text = cost(&report(vec![], CostAmounts::default()));
        assert_eq!(text, "No spend recorded from 2026-09-23 to 2026-09-29.");
    }
}
