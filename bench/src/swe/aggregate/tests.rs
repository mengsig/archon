use serde_json::{Value, json};

use super::*;

const CANARY: &str = "sealed-canary-django__django-99999";

fn arm(n: u32, passed: u32) -> Value {
    json!({"n": n, "passed": passed, "infra": 0, "cost_usd": 1.5, "wall_s": 600.0})
}

fn saved(split: &str) -> Value {
    json!({
        "schema": SCHEMA,
        "split": split,
        "tasks": 3,
        "repeats": 1,
        "limit": null,
        "arms": {"haiku/new": arm(3, 2), "haiku/native": arm(3, 1)},
        "pairs": [{"a": "haiku/native", "b": "haiku/new", "n": 3, "a_only": 0, "b_only": 1}],
    })
}

fn parse(value: &Value, split: Split) -> anyhow::Result<Aggregate> {
    Aggregate::parse_saved(&value.to_string(), split)
}

#[test]
fn a_saved_aggregate_round_trips_and_reads_pairs_either_way_round() {
    let aggregate = parse(&saved("dev"), Split::Dev).unwrap();
    let (new, native) = (
        "haiku/new".parse().unwrap(),
        "haiku/native".parse().unwrap(),
    );
    assert_eq!(aggregate.paired(new, native), Some((3, 1, 0)));
    assert_eq!(aggregate.paired(native, new), Some((3, 0, 1)));
    let again = Aggregate::parse_saved(&aggregate.to_json(), Split::Dev).unwrap();
    assert_eq!(again, aggregate);
    let mut unpriced = saved("dev");
    unpriced["arms"]["haiku/new"]["cost_usd"] = Value::Null;
    let unpriced = parse(&unpriced, Split::Dev).unwrap();
    assert_eq!(unpriced.totals(new).unwrap().cost_usd, None);
}

/// A named way to break a valid saved aggregate.
type Corrupt = fn(&mut Value);

#[test]
fn inconsistent_counts_are_refused() {
    let cases: [(&str, Corrupt); 9] = [
        ("more passes than trials", |v| {
            v["arms"]["haiku/new"]["passed"] = json!(4)
        }),
        ("more pairs than trials", |v| v["pairs"][0]["n"] = json!(4)),
        ("more trials than cells", |v| {
            v["arms"]["haiku/new"]["infra"] = json!(1)
        }),
        ("negative cost", |v| {
            v["arms"]["haiku/new"]["cost_usd"] = json!(-1.0)
        }),
        ("no task", |v| v["tasks"] = json!(0)),
        ("too many repeats", |v| {
            v["repeats"] = json!(MAX_REPEATS + 1)
        }),
        ("a limit below the tasks", |v| v["limit"] = json!(2)),
        ("a cross-model pair", |v| {
            v["pairs"][0]["a"] = json!("luna/native")
        }),
        ("paired twice", |v| {
            let first = v["pairs"][0].clone();
            v["pairs"].as_array_mut().unwrap().push(first);
        }),
    ];
    for (what, corrupt) in cases {
        let mut value = saved("dev");
        corrupt(&mut value);
        assert!(parse(&value, Split::Dev).is_err(), "{what}");
    }
}

#[test]
fn pair_counts_that_overflow_or_exceed_the_exact_test_are_refused() {
    let mut value = saved("dev");
    // These sum to 2^32 + 3, which a wrapping u32 add would read as 3 <= n.
    value["pairs"][0]["a_only"] = json!(2_147_483_649u32);
    value["pairs"][0]["b_only"] = json!(2_147_483_650u32);
    assert!(parse(&value, Split::Dev).is_err(), "discordant overflow");
    let mut value = saved("dev");
    let big = stats::MAX_PAIRS + 1;
    value["tasks"] = json!(stats::MAX_PAIRS);
    value["repeats"] = json!(2);
    value["arms"]["haiku/new"] = arm(2 * stats::MAX_PAIRS, big);
    value["arms"]["haiku/native"] = arm(2 * stats::MAX_PAIRS, 0);
    value["pairs"][0]["n"] = json!(2 * stats::MAX_PAIRS);
    value["pairs"][0]["b_only"] = json!(big);
    let error = parse(&value, Split::Dev).unwrap_err().to_string();
    assert!(error.contains("exact test"), "{error}");
}

#[test]
fn wrong_schemas_splits_and_fields_are_refused() {
    assert!(
        parse(&saved("dev"), Split::Holdout).is_err(),
        "split mismatch"
    );
    let mut value = saved("holdout");
    value["limit"] = json!(3);
    assert!(parse(&value, Split::Holdout).is_err(), "a limited holdout");
    let mut value = saved("dev");
    value["schema"] = json!("archon-bench-swe-aggregate/0");
    assert!(parse(&value, Split::Dev).is_err(), "schema mismatch");
    let mut value = saved("dev");
    value["instances"] = json!([]);
    assert!(
        parse(&value, Split::Dev).is_err(),
        "no field outside the schema"
    );
}

#[test]
fn holdout_parse_errors_never_echo_content() {
    let mut unknown = saved("holdout");
    unknown[CANARY] = json!(1);
    let mut mistyped = saved("holdout");
    mistyped["arms"]["haiku/new"]["n"] = json!(CANARY);
    for value in [unknown, mistyped] {
        let error = format!("{:#}", parse(&value, Split::Holdout).unwrap_err());
        assert!(!error.contains(CANARY), "{error}");
        assert!(error.contains("withheld"), "{error}");
    }
}
