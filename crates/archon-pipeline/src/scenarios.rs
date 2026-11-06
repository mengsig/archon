//! The strict scenario gate (`scenarios = "strict"`): archond itself reads
//! the scenario list a test stage's result carries, so a run cannot pass on a
//! failed product journey, a skipped one, or by running fewer than its manifest.

use std::collections::BTreeSet;

use serde_json::Value;

/// The status a scenario must have to count as passed.
const PASS: &str = "pass";
/// Most scenarios a shortfall names one by one; the rest are counted.
const MAX_NAMED: usize = 20;

/// Why a strict stage's scenario list does not pass: a one-line `headline`
/// and the `detail` lines naming each failed or missing scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shortfall {
    pub headline: String,
    pub detail: String,
}

/// `None` when `structured` lists the manifest's scenario ids as `expected`
/// and every one of them, with every other scenario listed, has status `pass`.
pub(crate) fn shortfall(structured: Option<&Value>) -> Option<Shortfall> {
    let Some(structured) = structured else {
        return Some(unlisted("the result has no structured data"));
    };
    let expected = match ids(structured.get("expected")) {
        Ok(expected) if expected.is_empty() => {
            return Some(unlisted("`expected` names no scenario of the manifest"));
        }
        Ok(expected) => expected,
        Err(why) => return Some(unlisted(&format!("`expected` {why}"))),
    };
    let ran = match ran(structured.get("scenarios")) {
        Ok(ran) if ran.is_empty() => return Some(unlisted("`scenarios` lists no scenario run")),
        Ok(ran) => ran,
        Err(why) => return Some(unlisted(&format!("`scenarios` {why}"))),
    };
    judge(&expected, &ran)
}

/// The failed and the missing scenarios, when there are any.
fn judge(expected: &[String], ran: &[(String, String)]) -> Option<Shortfall> {
    assert!(!expected.is_empty(), "an empty manifest is refused first");
    assert!(!ran.is_empty(), "an empty run is refused first");
    let ran_ids: BTreeSet<&str> = ran.iter().map(|(id, _)| id.as_str()).collect();
    let failed: Vec<String> = ran
        .iter()
        .filter(|(_, status)| !status.trim().eq_ignore_ascii_case(PASS))
        .map(|(id, status)| format!("`{id}`: {}", status_word(status)))
        .collect();
    let missing: Vec<String> = expected
        .iter()
        .filter(|id| !ran_ids.contains(id.as_str()))
        .map(|id| format!("`{id}`: missing, the manifest lists it"))
        .collect();
    if failed.is_empty() && missing.is_empty() {
        return None;
    }
    let headline = format!(
        "strict scenario gate failed: {} of {} scenario(s) did not pass, {} of {} the manifest lists did not run",
        failed.len(),
        ran.len(),
        missing.len(),
        expected.len()
    );
    let named: Vec<String> = failed.into_iter().chain(missing).collect();
    Some(Shortfall {
        headline,
        detail: listed(&named),
    })
}

/// A list the gate cannot judge: it fails, as running nothing would.
fn unlisted(why: &str) -> Shortfall {
    assert!(!why.is_empty(), "a refusal says why");
    Shortfall {
        headline: format!("strict scenario gate failed: {why}"),
        detail: "A strict stage's structured data lists the manifest's scenario ids as \
                 `expected` and every scenario run as `scenarios`, each with an `id` and a \
                 `status`."
            .to_owned(),
    }
}

/// The manifest's ids: an array of non-empty strings.
fn ids(value: Option<&Value>) -> Result<Vec<String>, &'static str> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Err("is missing or not a list");
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str().map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => out.push(id.to_owned()),
            None => return Err("holds an entry that is not a scenario id"),
        }
    }
    Ok(out)
}

/// Each scenario run as `(id, status)`; one without an id or status is refused.
fn ran(value: Option<&Value>) -> Result<Vec<(String, String)>, &'static str> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Err("is missing or not a list");
    };
    let field = |item: &Value, key: &str| {
        let text = item.get(key).and_then(Value::as_str).map(str::trim);
        text.filter(|text| !text.is_empty()).map(str::to_owned)
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match (field(item, "id"), field(item, "status")) {
            (Some(id), Some(status)) => out.push((id, status)),
            _ => return Err("holds a scenario without an `id` and a `status`"),
        }
    }
    Ok(out)
}

/// A status as the shortfall names it, cut short when an agent wrote prose.
fn status_word(status: &str) -> String {
    let word: String = status.trim().chars().take(40).collect();
    assert!(!word.is_empty(), "statuses are non-empty once read");
    word
}

/// `- ` lines, at most [`MAX_NAMED`] of them, then how many more there were.
fn listed(named: &[String]) -> String {
    assert!(!named.is_empty(), "a shortfall names what fell short");
    let mut out: Vec<String> = named
        .iter()
        .take(MAX_NAMED)
        .map(|n| format!("- {n}"))
        .collect();
    if named.len() > MAX_NAMED {
        out.push(format!("- and {} more", named.len() - MAX_NAMED));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(expected: Value, scenarios: Value) -> Option<Shortfall> {
        shortfall(Some(
            &json!({"verdict": "pass", "expected": expected, "scenarios": scenarios}),
        ))
    }

    #[test]
    fn every_expected_scenario_passing_passes() {
        let all = run(
            json!(["login", "start"]),
            json!([{"id": "login", "status": "pass"}, {"id": "start", "status": " PASS "}]),
        );
        assert_eq!(all, None);
    }

    #[test]
    fn one_product_defect_fails_and_is_named() {
        let short = run(
            json!(["login", "start"]),
            json!([{"id": "login", "status": "pass"}, {"id": "start", "status": "fail"}]),
        )
        .expect("a failed scenario falls short");
        assert!(
            short.headline.contains("1 of 2 scenario(s) did not pass"),
            "{short:?}"
        );
        assert_eq!(short.detail, "- `start`: fail");
    }

    #[test]
    fn skipped_blocked_and_missing_scenarios_fail() {
        let short = run(
            json!(["a", "b", "c", "d"]),
            json!([{"id": "a", "status": "pass"}, {"id": "b", "status": "skipped"},
                   {"id": "c", "status": "blocked"}]),
        )
        .expect("skipped, blocked and missing fall short");
        assert!(
            short
                .headline
                .contains("1 of 4 the manifest lists did not run"),
            "{short:?}"
        );
        for named in ["`b`: skipped", "`c`: blocked", "`d`: missing"] {
            assert!(short.detail.contains(named), "{named} in {}", short.detail);
        }
    }

    #[test]
    fn an_empty_or_unreadable_list_fails() {
        let cases = [
            run(json!([]), json!([])),
            run(json!(["a"]), json!([])),
            run(json!(["a"]), json!("all passed")),
            run(json!(["a", 3]), json!([{"id": "a", "status": "pass"}])),
            run(json!(["a"]), json!([{"id": "a"}])),
            shortfall(Some(&json!({"verdict": "pass"}))),
            shortfall(None),
        ];
        for (at, case) in cases.iter().enumerate() {
            let short = case.as_ref().unwrap_or_else(|| panic!("case {at} passed"));
            assert!(
                short.headline.starts_with("strict scenario gate failed: "),
                "{short:?}"
            );
        }
    }

    #[test]
    fn a_long_shortfall_names_twenty_and_counts_the_rest() {
        let expected: Vec<String> = (0..25).map(|n| format!("s{n}")).collect();
        let short = run(json!(expected), json!([{"id": "s0", "status": "fail"}])).unwrap();
        assert_eq!(short.detail.lines().count(), MAX_NAMED + 1);
        assert!(short.detail.ends_with("- and 5 more"), "{}", short.detail);
    }
}
