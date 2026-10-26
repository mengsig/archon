//! `cost_report`: its arguments and its text.

use archon_core::cost::{
    CostAmounts, CostGroup, CostQuery, CostReport, CostRow, DEFAULT_DAYS, DayBound, DayRange,
};
use serde_json::json;

use super::*;

fn query(arguments: Value) -> Result<CostQuery, String> {
    match to_request("cost_report", arguments) {
        Ok(Request::CostReport(query)) => Ok(query),
        Ok(other) => panic!("cost_report built {other:?}"),
        Err(ToolError(text)) => Err(text),
    }
}

#[test]
fn cost_report_maps_dates_days_and_the_grouping() {
    assert_eq!(query(json!({})).unwrap(), CostQuery::default());
    let query = query(json!({"since": "2026-09-01", "until": "2w", "by": "stage"})).unwrap();
    assert_eq!(query.since, Some(DayBound::parse("2026-09-01").unwrap()));
    assert_eq!(query.until, Some(DayBound::Days(14)));
    assert_eq!(query.by, CostGroup::Stage);
    assert!(
        definition("cost_report")
            .unwrap()
            .input_schema
            .to_string()
            .contains(&format!("Default: {DEFAULT_DAYS}d")),
        "the schema names the default"
    );
}

#[test]
fn cost_report_refuses_hours_and_unknown_groups_naming_the_argument() {
    let hours = query(json!({"since": "12h"})).unwrap_err();
    assert!(
        hours.starts_with("cost_report: `since` is invalid:") && hours.contains("calendar day"),
        "{hours}"
    );
    let week = query(json!({"by": "week"})).unwrap_err();
    assert!(week.contains("`by`"), "{week}");
    let blank = query(json!({"until": " "})).unwrap_err();
    assert!(blank.contains("`until` must not be empty"), "{blank}");
}

fn amounts(cost: f64, estimate: f64) -> CostAmounts {
    CostAmounts {
        counted_usd: cost + estimate,
        cost_usd: cost,
        estimated_cost_usd: estimate,
        unclassified_cost_usd: 0.0,
    }
}

fn report(rows: Vec<CostRow>) -> CostReport {
    CostReport {
        range: serde_json::from_value::<DayRange>(
            json!({"since": "2026-09-23", "until": "2026-09-29"}),
        )
        .unwrap(),
        by: CostGroup::Repo,
        estimates_counted: true,
        total: amounts(2.5, 1.0),
        rows,
    }
}

#[test]
fn rendered_cost_report_lists_each_group_then_the_total() {
    let rows = vec![
        CostRow {
            key: Some("web".into()),
            amounts: amounts(2.5, 0.0),
        },
        CostRow {
            key: None,
            amounts: amounts(0.0, 1.0),
        },
    ];
    let text = render("cost_report", &Reply::CostReport(Box::new(report(rows))));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "Spend from 2026-09-23 to 2026-09-29 by repo, as the budgets count it (reported cost \
             plus API-equivalent estimates):",
            "- web: $2.50 (billed $2.50, list-price/API estimate $0.00, unclassified $0.00)",
            "- (no repo): $1.00 (billed $0.00, list-price/API estimate $1.00, unclassified $0.00)",
            "Total: $3.50 (billed $2.50, list-price/API estimate $1.00, unclassified $0.00).",
        ]
    );
    let empty = render("cost_report", &Reply::CostReport(Box::new(report(vec![]))));
    assert_eq!(empty, "No spend recorded from 2026-09-23 to 2026-09-29.");
}

#[test]
fn model_evidence_maps_optional_filters_and_uses_thirty_days() {
    use archon_core::model_evidence::ModelEvidenceQuery;
    let Request::ModelEvidence(empty) = to_request("model_evidence", json!({})).unwrap() else {
        panic!("evidence");
    };
    assert_eq!(empty, ModelEvidenceQuery::default());
    let DayBound::Date(today) = DayBound::parse("2026-10-08").unwrap() else {
        panic!("date");
    };
    assert_eq!(
        empty.range(today).unwrap().since,
        "2026-09-09".parse().unwrap()
    );
    let Request::ModelEvidence(query) = to_request(
        "model_evidence",
        json!({"repo": "web", "pipeline": "p", "stage": "s", "since": "2w"}),
    )
    .unwrap() else {
        panic!("evidence");
    };
    assert_eq!(query.repo.unwrap().0, "web");
    assert_eq!(query.pipeline.as_deref(), Some("p"));
    assert_eq!(query.stage.as_deref(), Some("s"));
    assert_eq!(query.since, Some(DayBound::Days(14)));
    for args in [
        json!({"since": "12h"}),
        json!({"pipeline": " "}),
        json!({"stage": " "}),
        json!({"until": "1d"}),
    ] {
        assert!(to_request("model_evidence", args).is_err());
    }
    assert!(empty.range(today).unwrap().since < today);
    assert!(
        ModelEvidenceQuery {
            since: Some(DayBound::Days(0)),
            ..Default::default()
        }
        .range(today)
        .is_err()
    );
}

#[test]
fn rendered_list_prices_and_unknown_amounts_are_not_billed() {
    let amounts = CostAmounts {
        counted_usd: 2.0,
        cost_usd: 0.0,
        estimated_cost_usd: 2.0,
        unclassified_cost_usd: 1.0,
    };
    let mut report = report(vec![CostRow { key: None, amounts }]);
    report.total = amounts;
    report.estimates_counted = false;
    let text = render("cost_report", &Reply::CostReport(Box::new(report)));
    assert!(text.contains("separate API-equivalent estimates do not count"));
    assert_eq!(
        text.lines().last().unwrap(),
        "Total: $2.00 (billed $0.00, list-price/API estimate $2.00, unclassified $1.00; \
         ≈ $3.00 API-equivalent)."
    );
}
