//! The strict scenario gate (#395) and the `release-journey` product gate
//! (#394), driven through `sim::Sim` like every other run scenario: a product
//! defect fails a strict run at once, an environment fault loops back once.

mod sim;
mod support;

use archon_core::run::RunState;
use archon_pipeline::EngineAction;
use serde_json::{Value, json};
use sim::{Agent, Sim, finish_state, slots, spawn_plans, spawned};

fn sim(name: &str) -> Sim {
    let spec = support::builtin(name);
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    Sim::new(spec, plan)
}

fn failed(actions: &[EngineAction]) -> (String, String) {
    match finish_state(actions) {
        Some(RunState::Failed { stage_id, error }) => (stage_id.clone(), error.clone()),
        other => panic!("expected failure, got {other:?} from {actions:#?}"),
    }
}

/// A passing verdict from a tester that ran `ran` of the manifest's `expected`.
fn tested(expected: &[&str], ran: &[(&str, &str)]) -> Value {
    let scenarios: Vec<Value> = ran
        .iter()
        .map(|(id, status)| json!({"id": id, "title": id, "status": status}))
        .collect();
    json!({"verdict": "pass", "expected": expected, "scenarios": scenarios, "findings": []})
}

/// One product defect, reported as the prompts ask: a pass verdict, the
/// failed scenario in the list, and a high finding that does not block.
fn one_product_defect() -> Value {
    let mut result = tested(
        &["login", "checkout"],
        &[("login", "pass"), ("checkout", "fail")],
    );
    result["findings"] = json!([{
        "severity": "high", "blocking": false, "title": "checkout: pay button does nothing",
        "detail": "product defect", "path": ".archon/reports/e2e/assets/checkout-3.png"
    }]);
    result
}

/// `pipeline` started and its setup done, ready for its test stage.
fn at_test_stage(pipeline: &str) -> Sim {
    let mut sim = sim(pipeline);
    sim.start();
    sim.settle("setup", 0, Agent::Done("start.sh ready"));
    sim
}

#[test]
fn strict_mode_fails_at_once_on_one_product_defect_and_names_it() {
    let mut sim = at_test_stage("e2e-strict");
    let done = sim.settle(
        "e2e",
        0,
        Agent::DoneStructured("checkout broke\nVERDICT: PASS", one_product_defect()),
    );
    assert!(
        spawned(&done).is_empty(),
        "no loop back to setup: {done:#?}"
    );
    let (stage, error) = failed(&done);
    assert_eq!(stage, "e2e");
    assert!(error.contains("1 of 2 scenario(s) did not pass"), "{error}");
    assert!(error.contains("- `checkout`: fail"), "{error}");
    assert!(
        !error.contains("fix loop"),
        "a defect spends no loop: {error}"
    );
}

#[test]
fn lenient_mode_still_passes_on_the_same_product_defect() {
    let mut sim = at_test_stage("e2e-testing");
    let next = sim.settle(
        "e2e",
        0,
        Agent::DoneStructured("checkout broke\nVERDICT: PASS", one_product_defect()),
    );
    assert_eq!(spawned(&next), slots(&[("report", 1, 0)]));
    assert!(spawn_plans(&next)[0].prompt.contains("checkout broke"));
}

#[test]
fn an_empty_or_short_scenario_list_fails_strict_mode() {
    let cases = [
        ("nothing ran", tested(&["login", "checkout"], &[])),
        ("no manifest", tested(&[], &[("login", "pass")])),
        (
            "one of two ran",
            tested(&["login", "checkout"], &[("login", "pass")]),
        ),
        ("unlisted", json!({"verdict": "pass", "findings": []})),
        ("skipped", tested(&["login"], &[("login", "skipped")])),
    ];
    for (case, structured) in cases {
        let mut sim = at_test_stage("e2e-strict");
        let done = sim.settle("e2e", 0, Agent::DoneStructured("VERDICT: PASS", structured));
        assert!(spawned(&done).is_empty(), "{case}: {done:#?}");
        let (stage, error) = failed(&done);
        assert_eq!(stage, "e2e", "{case}");
        assert!(
            error.contains("strict scenario gate failed"),
            "{case}: {error}"
        );
    }
}

#[test]
fn every_scenario_passing_moves_a_strict_run_to_its_report() {
    let mut sim = at_test_stage("e2e-strict");
    let all = tested(
        &["login", "checkout"],
        &[("checkout", "pass"), ("login", "pass")],
    );
    let next = sim.settle("e2e", 0, Agent::DoneStructured("VERDICT: PASS", all));
    assert_eq!(spawned(&next), slots(&[("report", 1, 0)]));
}

#[test]
fn an_environment_fault_loops_back_to_setup_once_in_every_mode() {
    for (pipeline, test) in [
        ("e2e-testing", "e2e"),
        ("e2e-strict", "e2e"),
        ("release-journey", "journeys"),
    ] {
        let mut sim = at_test_stage(pipeline);
        let back = sim.settle(test, 0, Agent::Fail("display :99 never came up"));
        assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]), "{pipeline}");
        assert!(
            spawn_plans(&back)[0].prompt.contains("display :99"),
            "{pipeline}"
        );
        sim.settle("setup", 0, Agent::Done("started Xvfb"));
        let (stage, error) = failed(&sim.settle(test, 0, Agent::Fail("display :99 again")));
        assert_eq!(stage, test, "{pipeline}");
        assert!(
            error.contains("after 1 of 1 fix loop(s)"),
            "{pipeline}: {error}"
        );
    }
}

#[test]
fn a_seeded_broken_journey_fails_release_journey() {
    let mut sim = at_test_stage("release-journey");
    let expected = [
        "first-agent/empty/claude-code",
        "first-agent/populated/claude-code",
    ];
    let mut broken = tested(
        &expected,
        &[
            ("first-agent/empty/claude-code", "fail"),
            ("first-agent/populated/claude-code", "pass"),
        ],
    );
    broken["findings"] = json!([{
        "severity": "high", "blocking": false, "title": "start button disabled on an empty install",
        "detail": "the start button never enables", "path": "assets/first-agent/empty/1.png"
    }]);
    let done = sim.settle(
        "journeys",
        0,
        Agent::DoneStructured("start button disabled\nVERDICT: PASS", broken),
    );
    assert!(spawned(&done).is_empty(), "no report, no loop: {done:#?}");
    let (stage, error) = failed(&done);
    assert_eq!(stage, "journeys");
    assert!(
        error.contains("- `first-agent/empty/claude-code`: fail"),
        "{error}"
    );
    assert!(error.contains("start button disabled"), "{error}");
}

#[test]
fn passing_journeys_reach_the_report_and_succeed() {
    let mut sim = at_test_stage("release-journey");
    let ids = ["learn/empty/codex", "learn/populated/codex"];
    let all = tested(&ids, &[(ids[0], "pass"), (ids[1], "pass")]);
    let next = sim.settle("journeys", 0, Agent::DoneStructured("VERDICT: PASS", all));
    assert_eq!(spawned(&next), slots(&[("report", 1, 0)]));
    let report = sim.settle(
        "report",
        0,
        Agent::DoneStructured("report written", json!({"reports": []})),
    );
    assert!(
        matches!(finish_state(&report), Some(RunState::Succeeded { .. })),
        "{report:#?}"
    );
}

#[test]
fn a_strict_stage_contract_names_the_gate_and_the_private_stack() {
    let mut sim = at_test_stage("release-journey");
    let back = sim.settle("journeys", 0, Agent::Fail("no display"));
    let setup = &spawn_plans(&back)[0].instructions;
    assert!(!setup.contains("## Strict scenario gate"), "{setup}");
    let journeys = sim.settle("setup", 0, Agent::Done("ready"));
    let contract = &spawn_plans(&journeys)[0].instructions;
    for said in [
        "## Strict scenario gate",
        "\"expected\"",
        "with no loop back",
        "Never read or write the state of the daemons installed on this machine",
    ] {
        assert!(contract.contains(said), "missing {said:?} in {contract}");
    }
}
