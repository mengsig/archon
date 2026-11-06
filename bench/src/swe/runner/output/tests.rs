use serde_json::{Value, json};

use super::*;

const CANARY: &str = "sealed-canary-django__django-99999";

fn keys() -> Vec<ArmKey> {
    ["haiku/old", "haiku/new", "haiku/native"]
        .iter()
        .map(|k| k.parse().unwrap())
        .collect()
}

fn arm(name: &str, n: u32, passed: u32) -> Value {
    let (model, harness) = name.split_once('/').unwrap();
    json!({"model": model, "harness": harness, "trials": n, "infra": 0, "n": n, "passed": passed,
           "pass_rate": f64::from(passed) / f64::from(n), "pass_ci": [0.1, 0.9],
           "cost_usd_total": 3.0, "unpriced": 0, "cost_per_solved_usd": 1.5,
           "wall_seconds_total": 900.0, "wall_per_solved_seconds": 450.0})
}

fn pair(baseline: &str, candidate: &str, b: u32, c: u32) -> Value {
    json!({"model": "haiku", "baseline": baseline, "candidate": candidate, "tasks": 3, "pairs": 3,
           "b": b, "c": c, "p_one_sided": 0.5, "pass_diff": [0.0, -0.3, 0.3]})
}

/// Three tasks: native solves t1; old t1 and t2; new t1 and t3. Aether's
/// own gate reasons and stop reason carry the canary; dev adds the verdicts.
fn output(split: &str) -> Value {
    let mut value = json!({
        "schema": SCHEMA,
        "set": {"name": "swe-v1", "split": split, "sha256": "ab".repeat(32), "tasks": 3, "excluded": 1},
        "settings": {"confidence": 0.95, "accuracy_margin": 0.05, "native_alpha": 0.05},
        "arms": {"haiku/native": arm("haiku/native", 3, 1), "haiku/archon-old": arm("haiku/archon-old", 3, 2),
                 "haiku/archon-new": arm("haiku/archon-new", 3, 2)},
        "pairs": [pair("haiku/native", "haiku/archon-old", 0, 1),
                  pair("haiku/native", "haiku/archon-new", 0, 1),
                  pair("haiku/archon-old", "haiku/archon-new", 1, 1)],
        "gates": {"haiku": {"release": null, "native": {"haiku/archon-new": {"verdict": "FAIL", "reasons": [CANARY]}}}},
        "overall": "FAIL",
        "stop_reason": CANARY,
    });
    if split == "dev" {
        value["instances"] = instances();
    }
    value
}

fn instances() -> Value {
    let verdicts = [
        ("haiku/native", [true, false, false]),
        ("haiku/archon-old", [true, true, false]),
        ("haiku/archon-new", [true, false, true]),
    ];
    let rows: Vec<Value> = verdicts
        .iter()
        .flat_map(|(arm, passed)| {
            ["t1", "t2", "t3"].iter().zip(passed).map(
                move |(task, &passed)| json!({"task": task, "arm": arm, "repeat": 0, "passed": passed}),
            )
        })
        .collect();
    Value::Array(rows)
}

fn parse_value(value: &Value, split: Split) -> anyhow::Result<Aggregate> {
    parse(&value.to_string(), split, 1, None, &keys())
}

#[test]
fn a_dev_output_becomes_an_aggregate_oriented_as_the_runner_pairs_it() {
    let aggregate = parse_value(&output("dev"), Split::Dev).unwrap();
    let [old, new, native] = keys()[..] else {
        unreachable!()
    };
    assert_eq!(
        (aggregate.tasks, aggregate.repeats, aggregate.limit),
        (3, 1, None)
    );
    assert_eq!(aggregate.totals(new).unwrap().passed, 2);
    assert_eq!(aggregate.totals(native).unwrap().cost_usd, Some(3.0));
    assert_eq!(
        aggregate.paired(new, native),
        Some((3, 1, 0)),
        "new only t3"
    );
    assert_eq!(aggregate.paired(new, old), Some((3, 1, 1)));
    let saved = aggregate.to_json();
    assert!(!saved.contains(CANARY) && !saved.contains("t1"), "{saved}");
}

#[test]
fn the_arms_must_be_exactly_the_requested_ones_and_agree_with_their_names() {
    let mut value = output("dev");
    value["arms"]
        .as_object_mut()
        .unwrap()
        .remove("haiku/native");
    assert!(parse_value(&value, Split::Dev).is_err(), "a missing arm");
    let mut value = output("dev");
    value["arms"]["haiku/new"] = arm("haiku/new", 3, 1);
    assert!(parse_value(&value, Split::Dev).is_err(), "Aether's own arm");
    let mut value = output("dev");
    value["arms"]["haiku/archon-new"]["harness"] = json!("archon-old");
    assert!(
        parse_value(&value, Split::Dev).is_err(),
        "a mislabelled arm"
    );
    let mut value = output("dev");
    value["arms"]["haiku/archon-new"]["trials"] = json!(4);
    assert!(parse_value(&value, Split::Dev).is_err(), "uncounted trials");
    let mut value = output("dev");
    value["pairs"][0]["baseline"] = json!("haiku/new");
    assert!(
        parse_value(&value, Split::Dev).is_err(),
        "a pair outside the request"
    );
}

#[test]
fn the_set_must_be_swe_v1_on_the_requested_split_pinned_by_hash() {
    for (field, wrong) in [
        ("name", json!("v2")),
        ("split", json!("holdout")),
        ("sha256", json!("abc")),
    ] {
        let mut value = output("dev");
        value["set"][field] = wrong;
        assert!(parse_value(&value, Split::Dev).is_err(), "{field}");
    }
    let mut value = output("dev");
    value["schema"] = json!("aether-release-bench-aggregate/2");
    assert!(parse_value(&value, Split::Dev).is_err(), "another schema");
}

#[test]
fn dev_verdicts_must_add_up_to_the_totals_and_the_pairs() {
    let mut value = output("dev");
    value["instances"][8]["passed"] = json!(false);
    assert!(
        parse_value(&value, Split::Dev).is_err(),
        "a flipped verdict"
    );
    let mut value = output("dev");
    value["instances"][0]["repeat"] = json!(1);
    assert!(
        parse_value(&value, Split::Dev).is_err(),
        "a repeat outside the run"
    );
    let mut value = output("dev");
    let mut rows = instances();
    let first = rows[0].clone();
    rows.as_array_mut().unwrap().push(first);
    value["instances"] = rows;
    assert!(parse_value(&value, Split::Dev).is_err(), "graded twice");
    let mut value = output("dev");
    value.as_object_mut().unwrap().remove("instances");
    assert!(
        parse_value(&value, Split::Dev).is_err(),
        "no verdicts to check"
    );
}

#[test]
fn a_limited_dev_run_is_recorded_as_limited_and_a_limited_holdout_refused() {
    let limited = parse(&output("dev").to_string(), Split::Dev, 1, Some(3), &keys()).unwrap();
    assert_eq!(limited.limit, Some(3));
    let short = parse(&output("dev").to_string(), Split::Dev, 1, Some(2), &keys());
    assert!(short.is_err(), "more tasks than the limit");
    let holdout = output("holdout").to_string();
    assert!(parse(&holdout, Split::Holdout, 1, Some(3), &keys()).is_err());
}

#[test]
fn a_holdout_output_keeps_none_of_aethers_gate_reasons_or_stop_reason() {
    let aggregate = parse_value(&output("holdout"), Split::Holdout).unwrap();
    assert_eq!(aggregate.split, Split::Holdout);
    let saved = aggregate.to_json();
    assert!(!saved.contains(CANARY), "{saved}");
}

#[test]
fn a_holdout_output_with_instances_is_refused_without_quoting_them() {
    let mut value = output("holdout");
    value["instances"] =
        json!([{"task": CANARY, "arm": "haiku/native", "repeat": 0, "passed": true}]);
    let error = format!("{:#}", parse_value(&value, Split::Holdout).unwrap_err());
    assert!(error.contains("per-instance"), "{error}");
    assert!(!error.contains(CANARY), "{error}");
}

#[test]
fn holdout_parse_errors_never_echo_content() {
    let mut unknown = output("holdout");
    unknown[CANARY] = json!(1);
    let mut mistyped = output("holdout");
    mistyped["arms"]["haiku/native"]["n"] = json!(CANARY);
    let mut bad_split = output("holdout");
    bad_split["set"]["split"] = json!(CANARY);
    for value in [unknown, mistyped, bad_split] {
        let error = format!("{:#}", parse_value(&value, Split::Holdout).unwrap_err());
        assert!(!error.contains(CANARY), "{error}");
        assert!(error.contains("withheld"), "{error}");
    }
    let mut unknown = output("dev");
    unknown["surprise"] = json!(1);
    let error = format!("{:#}", parse_value(&unknown, Split::Dev).unwrap_err());
    assert!(
        error.contains("surprise"),
        "dev errors name the field: {error}"
    );
}
