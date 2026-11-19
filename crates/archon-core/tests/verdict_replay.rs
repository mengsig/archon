//! The 9 completed `codex/gpt-6-astra` e2e results of the 2026-11 diagnosis
//! (finding F1), replayed: each wrote a pass beside `high` product findings
//! with no `blocking` mark, which the gate read as `Unknown`.

use archon_core::run::{Verdict, VerdictNote};
use serde_json::Value;

const RESULTS: &str = include_str!("fixtures/f1_astra_e2e.json");

/// Each stored result's summary and `structured` data.
fn results() -> Vec<(String, Value)> {
    let stored: Vec<Value> = serde_json::from_str(RESULTS).expect("the fixture is JSON");
    assert_eq!(stored.len(), 9, "the nine completed attempts of F1");
    stored
        .into_iter()
        .map(|result| {
            assert_eq!(result["stored_verdict"], "unknown", "as the gate stored it");
            let summary = result["summary"].as_str().expect("a summary").to_owned();
            (summary, result["structured"].clone())
        })
        .collect()
}

/// `structured` with every finding marked as the e2e prompt asks: a product
/// defect never blocks this gate.
fn marked_non_blocking(mut structured: Value) -> Value {
    let findings = structured["findings"]
        .as_array_mut()
        .expect("each result lists findings");
    assert!(!findings.is_empty(), "each result has findings");
    for finding in findings {
        assert!(finding.get("blocking").is_none(), "F1: none carried a mark");
        finding["blocking"] = Value::Bool(false);
    }
    structured
}

#[test]
fn the_stored_results_read_as_disputed_passes_not_as_fails() {
    for (summary, structured) in results() {
        let (verdict, note) = Verdict::read(&summary, Some(&structured));
        assert_eq!(verdict, Verdict::Unknown, "{summary}");
        assert_eq!(note, Some(VerdictNote::DisputedPass), "{summary}");
    }
}

#[test]
fn with_the_blocking_field_set_every_result_passes() {
    for (summary, structured) in results() {
        let marked = marked_non_blocking(structured);
        assert_eq!(
            Verdict::read(&summary, Some(&marked)),
            (Verdict::Pass, None),
            "{summary}"
        );
    }
}
