//! `on_fail.when = "environment_finding"` and a verdict restated through
//! `submit_verdict`, driven through `sim::Sim`: an e2e failure re-runs `setup`
//! only for a readable fail whose findings name the environment, a verdict
//! that reads as neither pass nor fail never does, and a restated fail fails
//! whatever the stage then completed with, by its own findings.

mod sim;
mod support;

use std::sync::Arc;

use archon_core::run::{Restatement, RunState, Verdict, VerdictNote, VerdictReading};
use archon_pipeline::EngineAction;
use serde_json::{Value, json};
use sim::{Agent, Sim, finish_state, slots, spawn_plans, spawned};

const NOT_RE_RUN: &str = "No verdict read as `fail` with a finding that names the environment as at fault (`\"fault\": \"environment\"`), so `setup` was not re-run.";

/// `e2e-testing` started and its setup done, ready for the `e2e` stage.
fn at_e2e() -> Sim {
    let spec = support::builtin("e2e-testing");
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    let mut sim = Sim::new(spec, plan);
    sim.start();
    sim.settle("setup", 0, Agent::Done("start.sh ready"));
    sim
}

fn failed(actions: &[EngineAction]) -> (String, String) {
    match finish_state(actions) {
        Some(RunState::Failed { stage_id, error }) => (stage_id.clone(), error.clone()),
        other => panic!("expected failure, got {other:?} from {actions:#?}"),
    }
}

/// A pass beside one finding that blocks it, which reads as a disputed pass.
fn disputed_pass(fault: Option<&str>) -> Value {
    let mut finding = json!({
        "severity": "high", "blocking": true, "title": "checkout: pay button does nothing"
    });
    if let Some(fault) = fault {
        finding["fault"] = json!(fault);
    }
    json!({"verdict": "pass", "findings": [finding]})
}

/// One blocking finding that names the environment as at fault.
fn environment_finding() -> Value {
    json!([{"severity": "high", "blocking": true, "fault": "environment", "title": "no display"}])
}

fn setup_runs(sim: &Sim) -> usize {
    sim.stages.iter().filter(|s| s.stage_id == "setup").count()
}

#[test]
fn a_failure_naming_no_environment_fault_fails_without_re_running_setup() {
    let product = json!({"verdict": "fail", "findings": [
        {"severity": "high", "blocking": true, "fault": "product", "title": "login 500"}
    ]});
    let endings = [
        Agent::Fail("login page 500"),
        Agent::Recorded("Ran it.\nVERDICT: FAIL", product),
        Agent::Recorded("Ran it.\nVERDICT: PASS", disputed_pass(None)),
        Agent::Recorded("Ran it.\nVERDICT: PASS", disputed_pass(Some("product"))),
        Agent::NoVerdict,
        Agent::TimedOut,
        Agent::Error("the browser crashed"),
        // A verdict that reads as neither pass nor fail re-runs nothing,
        // though it names the environment: disputed, missing, hedged.
        Agent::Recorded("Ran it.\nVERDICT: PASS", disputed_pass(Some("environment"))),
        Agent::Recorded("Ran it.", json!({"findings": environment_finding()})),
        Agent::Recorded(
            "Ran it.\nVERDICT: PASS or FAIL",
            json!({"findings": environment_finding()}),
        ),
    ];
    for ending in endings {
        let mut sim = at_e2e();
        let done = sim.settle("e2e", 0, ending.clone());
        let (stage, error) = failed(&done);
        assert_eq!(stage, "e2e", "{ending:?}");
        // The driver's own opening, which the failure classes read.
        let opens = error.starts_with("stage `e2e` failed after 0 of 1 fix loop(s): ");
        assert!(opens, "{ending:?}: {error}");
        assert!(error.contains(NOT_RE_RUN), "{ending:?}: {error}");
        assert!(spawned(&done).is_empty(), "{ending:?}: nothing is re-run");
        assert_eq!(setup_runs(&sim), 1, "{ending:?}");
    }
}

#[test]
fn a_finding_naming_the_environment_re_runs_setup_once() {
    let worded = json!({"verdict": "fail", "findings": [
        {"severity": "low", "blocking": false, "fault": "product", "title": "typo"},
        {"severity": "high", "blocking": true, "fault": " Environment ", "title": "no display"}
    ]});
    let endings = [
        Agent::EnvironmentFail("display :99 never came up"),
        Agent::Recorded("Ran it.\nVERDICT: FAIL", worded),
    ];
    for ending in endings {
        let mut sim = at_e2e();
        let back = sim.settle("e2e", 0, ending.clone());
        assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]), "{ending:?}");
        sim.settle("setup", 0, Agent::Done("started Xvfb"));
        let again = sim.settle("e2e", 0, ending.clone());
        let (stage, error) = failed(&again);
        assert_eq!(stage, "e2e", "{ending:?}");
        assert!(error.contains("after 1 of 1 fix loop(s)"), "{error}");
        assert!(
            !error.contains(NOT_RE_RUN),
            "its one loop was spent: {error}"
        );
        assert_eq!(setup_runs(&sim), 2, "{ending:?}: one re-run, never two");
    }
}

#[test]
fn a_failure_after_the_one_loop_says_the_loop_was_spent() {
    let mut sim = at_e2e();
    sim.settle("e2e", 0, Agent::EnvironmentFail("no display"));
    sim.settle("setup", 0, Agent::Done("started Xvfb"));
    let (stage, error) = failed(&sim.settle("e2e", 0, Agent::Fail("login 500")));
    assert_eq!(stage, "e2e");
    let opens = error.starts_with("stage `e2e` failed after 1 of 1 fix loop(s): ");
    assert!(opens, "{error}");
    assert!(
        !error.contains(NOT_RE_RUN),
        "no loop was left to withhold: {error}"
    );
    assert_eq!(setup_runs(&sim), 2);
}

/// The restatement `submit_verdict` leaves after a disputed pass was asked
/// back and the agent restated `restated`, naming the environment or not.
fn asked_then(restated: (Verdict, Option<VerdictNote>), environment: bool) -> Restatement {
    Restatement {
        asked: VerdictNote::DisputedPass,
        restated: Some(VerdictReading::new(restated)),
        environment,
    }
}

fn e2e_run(sim: &Sim) -> &archon_core::run::StageRun {
    let e2e = sim.stages.iter().find(|s| s.stage_id == "e2e");
    e2e.expect("the e2e stage ran")
}

#[test]
fn a_disputed_pass_restated_as_a_pass_passes_the_gate_with_one_setup_run() {
    let mut sim = at_e2e();
    let restated = json!({"verdict": "pass", "findings": [{
        "severity": "high", "blocking": false, "fault": "product",
        "title": "checkout: pay button does nothing"
    }]});
    let ending = Agent::Restated(
        "Ran it.\nVERDICT: PASS",
        restated,
        asked_then((Verdict::Pass, None), false),
    );
    let report = sim.settle("e2e", 0, ending);
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    assert_eq!(setup_runs(&sim), 1, "a disputed pass costs no setup re-run");
}

#[test]
fn a_restated_fail_fails_whatever_the_stage_completed_with() {
    let mut sim = at_e2e();
    // The later pass notes the environment, which the restated fail did not.
    let pass = json!({"verdict": "pass", "findings": [
        {"severity": "low", "blocking": false, "fault": "environment", "title": "slow start"}
    ]});
    let ending = Agent::Restated(
        "All fine after all.\nVERDICT: PASS",
        pass,
        asked_then((Verdict::Fail, None), false),
    );
    let done = sim.settle("e2e", 0, ending);
    let (stage, error) = failed(&done);
    assert_eq!(stage, "e2e");
    assert!(error.contains(NOT_RE_RUN), "{error}");
    assert!(
        error.contains("verdict fail, as it restated before completing (final)"),
        "{error}"
    );
    assert_eq!(e2e_run(&sim).verdict, Some(Verdict::Fail));
    assert_eq!(setup_runs(&sim), 1, "its own findings named no environment");
}

#[test]
fn a_restated_environment_fail_re_runs_setup_though_the_result_lost_the_finding() {
    let mut sim = at_e2e();
    let pass = json!({"verdict": "pass", "findings": []});
    let ending = Agent::Restated(
        "All fine after all.\nVERDICT: PASS",
        pass,
        asked_then((Verdict::Fail, None), true),
    );
    let back = sim.settle("e2e", 0, ending);
    assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]));
    assert_eq!(e2e_run(&sim).verdict, Some(Verdict::Fail));
}

#[test]
fn a_result_that_fails_by_itself_is_routed_by_its_own_findings() {
    // Both fail alike, so the result's own verdict and findings stand.
    let own = json!({"verdict": "fail", "findings": environment_finding()});
    let mut names_it = at_e2e();
    let ending = Agent::Restated(
        "Ran it.\nVERDICT: FAIL",
        own,
        asked_then((Verdict::Fail, None), false),
    );
    let back = names_it.settle("e2e", 0, ending);
    assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]));

    let product = json!({"verdict": "fail", "findings": [
        {"severity": "high", "blocking": true, "fault": "product", "title": "login 500"}
    ]});
    let mut names_none = at_e2e();
    let ending = Agent::Restated(
        "Ran it.\nVERDICT: FAIL",
        product,
        asked_then((Verdict::Fail, None), true),
    );
    let (_, error) = failed(&names_none.settle("e2e", 0, ending));
    assert!(error.contains(NOT_RE_RUN), "{error}");
}

#[test]
fn a_restatement_still_disputed_fails_a_result_that_reads_as_a_pass() {
    let mut sim = at_e2e();
    let still = (Verdict::Unknown, Some(VerdictNote::DisputedPass));
    let pass = json!({"verdict": "pass", "findings": []});
    // It named the environment, and still re-runs nothing: it is no fail.
    let ending = Agent::Restated("Ran it.\nVERDICT: PASS", pass, asked_then(still, true));
    let done = sim.settle("e2e", 0, ending);
    let (stage, error) = failed(&done);
    assert_eq!(stage, "e2e");
    assert!(error.contains(NOT_RE_RUN), "{error}");
    assert!(
        error.contains("as it restated before completing (final; counts as fail)"),
        "{error}"
    );
    let e2e = e2e_run(&sim);
    assert_eq!(
        (e2e.verdict, e2e.verdict_note),
        (Some(Verdict::Unknown), Some(VerdictNote::DisputedPass))
    );
    assert_eq!(setup_runs(&sim), 1);
}

/// A fanned-out gated test whose loop is conditional, as a user pipeline may write it.
const FANNED: &str = r#"
[pipeline]
name = "fanned"
description = "Setup, two gated testers that loop back for the environment only"
version = 1
workspace = "scratch"

[[stage]]
id = "setup"
kind = "work"
prompt = "setup.md"

[[stage]]
id = "e2e"
kind = "test"
prompt = "e2e.md"
fan_out = 2
gate = "all_pass"
on_fail = { goto = "setup", max_loops = 1, when = "environment_finding" }
"#;

#[test]
fn only_a_failing_members_finding_takes_the_loop() {
    let fanned = || {
        let spec = Arc::new(support::spec_from(FANNED, &[]));
        spec.validate("fanned").expect("the fixture is valid");
        let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
        let mut sim = Sim::new(spec, plan);
        sim.start();
        sim.settle("setup", 0, Agent::Done("ready"));
        sim
    };
    let mut one_names_it = fanned();
    one_names_it.settle("e2e", 0, Agent::Fail("login 500"));
    let back = one_names_it.settle("e2e", 1, Agent::EnvironmentFail("no display"));
    assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]));
    assert!(spawn_plans(&back)[0].prompt.contains("no display"));

    // A passing member's environment note is no reason to re-run setup.
    let noted = json!({"verdict": "pass", "findings": [
        {"severity": "low", "blocking": false, "fault": "environment", "title": "slow start"}
    ]});
    let mut passer_notes_it = fanned();
    passer_notes_it.settle("e2e", 0, Agent::Recorded("Ran it.\nVERDICT: PASS", noted));
    let done = passer_notes_it.settle("e2e", 1, Agent::Fail("login 500"));
    let (stage, error) = failed(&done);
    assert_eq!(stage, "e2e");
    assert!(error.contains(NOT_RE_RUN), "{error}");
}
