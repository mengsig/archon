use serde_json::{Value, json};

use super::*;
use crate::swe::arms::{Harness, Model};

const CANARY: &str = "sealed-canary-django__django-99999";

fn key(text: &str) -> ArmKey {
    text.parse().unwrap()
}

fn arm(n: u32, passed: u32) -> Value {
    json!({"n": n, "passed": passed, "infra": 0, "cost_usd": 1.5, "wall_s": 600.0})
}

fn aggregate(split: &str) -> Value {
    json!({
        "schema": SCHEMA,
        "split": split,
        "arms": {"haiku/new": arm(3, 2), "haiku/native": arm(3, 1)},
        "pairs": [{"a": "haiku/new", "b": "haiku/native", "n": 3, "a_only": 1, "b_only": 0}],
    })
}

fn instances() -> Value {
    json!([
        {"instance_id": "a", "arm": "haiku/new", "resolved": true},
        {"instance_id": "b", "arm": "haiku/new", "resolved": true},
        {"instance_id": "c", "arm": "haiku/new", "resolved": false},
        {"instance_id": "a", "arm": "haiku/native", "resolved": true},
        {"instance_id": "b", "arm": "haiku/native", "resolved": false},
        {"instance_id": "c", "arm": "haiku/native", "resolved": false},
    ])
}

fn parse(value: &Value, split: Split) -> anyhow::Result<Aggregate> {
    Aggregate::parse(&value.to_string(), split, None)
}

#[test]
fn a_valid_aggregate_parses_and_pairs_read_either_way_round() {
    let parsed = parse(&aggregate("holdout"), Split::Holdout).unwrap();
    assert_eq!(parsed.totals(key("haiku/new")).unwrap().passed, 2);
    assert_eq!(
        parsed.paired(key("haiku/new"), key("haiku/native")),
        Some((3, 1, 0))
    );
    assert_eq!(
        parsed.paired(key("haiku/native"), key("haiku/new")),
        Some((3, 0, 1))
    );
    let new = ArmKey {
        model: Model::Haiku,
        harness: Harness::New,
    };
    let old = ArmKey {
        model: Model::Haiku,
        harness: Harness::Old,
    };
    assert_eq!(parsed.paired(new, old), None);
}

#[test]
fn the_requested_arms_must_be_exactly_the_aggregates_arms() {
    let text = aggregate("dev").to_string();
    let both = [key("haiku/new"), key("haiku/native")];
    assert!(Aggregate::parse(&text, Split::Dev, Some(&both)).is_ok());
    assert!(Aggregate::parse(&text, Split::Dev, Some(&both[..1])).is_err());
}

#[test]
fn a_dev_aggregate_may_carry_verdicts_that_must_add_up() {
    let mut value = aggregate("dev");
    value["instances"] = instances();
    assert!(parse(&value, Split::Dev).is_ok());
    value["instances"][1]["resolved"] = json!(false);
    let error = parse(&value, Split::Dev).unwrap_err().to_string();
    assert!(error.contains("resolved verdicts"), "{error}");
    value["instances"][1]["resolved"] = json!(true);
    value["pairs"][0]["a_only"] = json!(0);
    value["pairs"][0]["b_only"] = json!(1);
    assert!(
        parse(&value, Split::Dev).is_err(),
        "pairs that contradict the verdicts"
    );
}

#[test]
fn a_holdout_aggregate_with_instances_is_refused_without_quoting_them() {
    let mut value = aggregate("holdout");
    value["instances"] = instances();
    value["instances"][0]["instance_id"] = json!(CANARY);
    let error = format!("{:#}", parse(&value, Split::Holdout).unwrap_err());
    assert!(error.contains("per-instance"), "{error}");
    assert!(!error.contains(CANARY), "{error}");
}

#[test]
fn holdout_parse_errors_never_echo_content() {
    let mut cases = Vec::new();
    let mut extra = aggregate("holdout");
    extra["prompt"] = json!(CANARY);
    cases.push(extra);
    let mut wrong_type = aggregate("holdout");
    wrong_type["arms"]["haiku/new"]["cost_usd"] = json!(CANARY);
    cases.push(wrong_type);
    let mut unknown_arm = aggregate("holdout");
    unknown_arm["arms"][CANARY] = arm(1, 1);
    cases.push(unknown_arm);
    for case in cases {
        let error = format!("{:#}", parse(&case, Split::Holdout).unwrap_err());
        assert!(
            !error.contains(CANARY),
            "the error echoes holdout content: {error}"
        );
        assert!(error.contains("withheld"), "{error}");
    }
}

#[test]
fn inconsistent_counts_and_wrong_splits_are_refused() {
    let mut value = aggregate("dev");
    value["arms"]["haiku/new"]["passed"] = json!(4);
    assert!(
        parse(&value, Split::Dev).is_err(),
        "more passes than trials"
    );
    let mut value = aggregate("dev");
    value["pairs"][0]["n"] = json!(4);
    assert!(parse(&value, Split::Dev).is_err(), "more pairs than trials");
    let mut value = aggregate("dev");
    value["arms"]["haiku/new"]["cost_usd"] = json!(-1.0);
    assert!(parse(&value, Split::Dev).is_err(), "negative cost");
    assert!(
        parse(&aggregate("dev"), Split::Holdout).is_err(),
        "split mismatch"
    );
    let mut value = aggregate("dev");
    value["schema"] = json!("swe-aggregate/0");
    assert!(parse(&value, Split::Dev).is_err(), "schema mismatch");
}

#[test]
fn a_saved_aggregate_round_trips_and_drops_every_instance() {
    let mut value = aggregate("dev");
    value["instances"] = instances();
    let parsed = parse(&value, Split::Dev).unwrap();
    let saved = parsed.to_json();
    assert!(!saved.contains("instance_id"));
    assert_eq!(Aggregate::parse(&saved, Split::Dev, None).unwrap(), parsed);
}
