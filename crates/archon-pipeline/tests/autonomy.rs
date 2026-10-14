//! The autonomy knobs in the RunDriver (ARCHITECTURE §18): proposal and
//! change-report gates, their approve, revise and reject paths, a with-pr
//! report that never waits, and `merge = auto` reaching a ship agent's prompt.
//! Every step runs through `sim::Sim`, which checks the resume property.

mod sim;
mod support;

use std::sync::Arc;

use archon_core::autonomy::{Autonomy, ProposalMode, ReportMode};
use archon_core::run::{GateDecision, GateSubject, RunState};
use archon_pipeline::autonomy::{OperatorGates, augment};
use archon_pipeline::engine::MAX_REVISIONS;
use archon_pipeline::spec::PipelineSpec;
use archon_pipeline::{EngineAction, RunDriver};
use sim::{Agent, Sim, awaited, finish_state, ship_plans, slots, spawn_plans, spawned};

fn gated_sim(name: &str, proposal: ProposalMode, report: ReportMode) -> Sim {
    gated_spec_sim(&support::builtin(name), proposal, report)
}

fn gated_spec_sim(spec: &PipelineSpec, proposal: ProposalMode, report: ReportMode) -> Sim {
    let knobs = Autonomy {
        proposal,
        report,
        ..Autonomy::default()
    };
    let spec = Arc::new(augment(spec, &knobs).expect("knobs apply"));
    let gates = OperatorGates::for_run(&spec, &knobs);
    let plan = support::plan_for(&spec, &["claude-code/opus"]);
    Sim::with_gates(spec, plan, gates)
}

fn approve(note: Option<&str>) -> GateDecision {
    GateDecision::Approve {
        note: note.map(str::to_owned),
    }
}

fn revise(feedback: &str) -> GateDecision {
    GateDecision::Revise {
        feedback: feedback.into(),
    }
}

/// Settle the propose stage and return the gate it opens.
fn propose(sim: &mut Sim, attempt: u32) -> Vec<EngineAction> {
    let actions = sim.settle(
        "propose",
        0,
        Agent::Done("Add a parse flag; touch src/parse.rs."),
    );
    let waits = awaited(&actions);
    assert_eq!(waits.len(), 1, "{actions:#?}");
    assert_eq!(waits[0].subject, GateSubject::Proposal);
    assert_eq!(waits[0].attempt, attempt);
    assert!(
        spawned(&actions).is_empty(),
        "nothing runs while the gate waits"
    );
    assert!(matches!(sim.run.state, RunState::AwaitingOperator { .. }));
    actions
}

#[test]
fn an_approved_proposal_reaches_the_implementer_with_the_note() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Lavish, ReportMode::Off);
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("propose", 1, 0)]));
    assert!(
        spawn_plans(&first)[0]
            .instructions
            .contains("kind `propose`")
    );
    propose(&mut sim, 1);
    let next = sim.decide(approve(Some("keep the old flag working")));
    assert_eq!(spawned(&next), slots(&[("implement", 1, 0)]));
    let prompt = &spawn_plans(&next)[0].prompt;
    assert!(prompt.contains("## Direction from the propose stage"));
    assert!(prompt.contains("The operator approved this proposal"));
    assert!(prompt.contains("Add a parse flag; touch src/parse.rs."));
    assert!(prompt.contains("note on approving: keep the old flag working"));
    let ship = sim.settle("implement", 0, Agent::Done("Added the flag."));
    assert!(spawned(&ship).is_empty(), "no agent ships");
    assert_eq!(ship_plans(&ship).len(), 1, "{ship:#?}");
    assert_eq!(ship_plans(&ship)[0].record_stage, "implement");
}

#[test]
fn a_revised_proposal_is_rewritten_with_the_feedback() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Lavish, ReportMode::Off);
    sim.start();
    propose(&mut sim, 1);
    let again = sim.decide(revise("Use a trait instead of a flag."));
    assert_eq!(spawned(&again), slots(&[("propose", 2, 0)]));
    let prompt = &spawn_plans(&again)[0].prompt;
    assert!(prompt.contains("The operator reviewed the direction and asked for changes"));
    assert!(prompt.contains("Use a trait instead of a flag."));
    assert!(prompt.contains("This is attempt 2."));
    propose(&mut sim, 2);
    let next = sim.decide(approve(None));
    assert_eq!(spawned(&next), slots(&[("implement", 1, 0)]));
}

#[test]
fn a_rejected_proposal_cancels_the_run() {
    let mut sim = gated_sim("default-pr-review", ProposalMode::Lavish, ReportMode::Off);
    sim.start();
    propose(&mut sim, 1);
    let done = sim.decide(GateDecision::Reject {
        reason: "not worth it".into(),
    });
    match finish_state(&done) {
        Some(RunState::Cancelled {
            reason: Some(reason),
        }) => {
            assert!(reason.contains("rejected the direction at stage `propose`"));
            assert!(reason.contains("not worth it"));
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
}

#[test]
fn revisions_past_the_limit_fail_the_run() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Lavish, ReportMode::Off);
    sim.start();
    for attempt in 1..=MAX_REVISIONS {
        propose(&mut sim, attempt);
        sim.decide(revise("again"));
    }
    propose(&mut sim, MAX_REVISIONS + 1);
    let done = sim.decide(revise("once more"));
    match finish_state(&done) {
        Some(RunState::Failed { stage_id, error }) => {
            assert_eq!(stage_id, "propose");
            assert!(error.contains("is the limit"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// Run `default-pr-review` with a before-pr report up to its gate.
fn to_report_gate(sim: &mut Sim, attempt: u32) {
    sim.settle("implement", 0, Agent::Done("Fixed parse()."));
    let report = sim.settle("review", 0, Agent::Pass);
    assert_eq!(spawned(&report), slots(&[("report", attempt, 0)]));
    let prompt = &spawn_plans(&report)[0].prompt;
    assert!(prompt.contains("## Earlier stages"));
    assert!(prompt.contains("### `implement`") && prompt.contains("Fixed parse()."));
    assert!(prompt.contains("### `review`"));
    let gate = sim.settle(
        "report",
        0,
        Agent::Done("Report at .archon/reports/change."),
    );
    let waits = awaited(&gate);
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].subject, GateSubject::Report);
}

#[test]
fn an_approved_before_pr_report_ships_with_the_report() {
    let mut sim = gated_sim("default-pr-review", ProposalMode::Off, ReportMode::BeforePr);
    sim.start();
    to_report_gate(&mut sim, 1);
    let ship = sim.decide(approve(None));
    assert!(spawned(&ship).is_empty(), "no agent ships");
    let body = &ship_plans(&ship)[0].body;
    assert!(body.contains("## Change report"), "{body}");
    assert!(body.contains("Report at .archon/reports/change."));
    assert!(
        body.contains("### `review`"),
        "the review that passed is in the body"
    );
}

#[test]
fn a_revised_before_pr_report_sends_the_work_back_to_implement() {
    let mut sim = gated_sim("default-pr-review", ProposalMode::Off, ReportMode::BeforePr);
    sim.start();
    to_report_gate(&mut sim, 1);
    let back = sim.decide(revise("Also handle whitespace-only input."));
    assert_eq!(spawned(&back), slots(&[("implement", 2, 0)]));
    let prompt = &spawn_plans(&back)[0].prompt;
    assert!(prompt.contains("The operator reviewed the change report and asked for changes"));
    assert!(prompt.contains("Also handle whitespace-only input."));
    assert!(
        !prompt.contains("## Feedback to address"),
        "a template showing {{{{feedback}}}} gets it once"
    );
    sim.settle("implement", 0, Agent::Done("Fixed parse()."));
    let report = sim.settle("review", 0, Agent::Pass);
    assert_eq!(spawned(&report), slots(&[("report", 2, 0)]));
    let gate = sim.settle("report", 0, Agent::Done("Report v2."));
    assert_eq!(awaited(&gate)[0].attempt, 2);
    let ship = sim.decide(approve(None));
    assert_eq!(ship_plans(&ship).len(), 1, "{ship:#?}");
    assert!(ship_plans(&ship)[0].body.contains("Report v2."));
}

#[test]
fn a_revised_quick_pr_report_reaches_the_implementer() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Off, ReportMode::BeforePr);
    sim.start();
    sim.settle("implement", 0, Agent::Done("Fixed parse()."));
    let gate = sim.settle("report", 0, Agent::Done("Report v1."));
    assert_eq!(awaited(&gate).len(), 1);
    let back = sim.decide(revise("UNIQUE-OPERATOR-FEEDBACK"));
    assert_eq!(spawned(&back), slots(&[("implement", 2, 0)]));
    let prompt = &spawn_plans(&back)[0].prompt;
    assert_eq!(
        prompt.matches("UNIQUE-OPERATOR-FEEDBACK").count(),
        1,
        "{prompt}"
    );
}

#[test]
fn a_revision_reaches_a_prompt_without_a_feedback_placeholder() {
    let toml = r#"
        [pipeline]
        name = "plain"
        description = "d"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "ship"
        kind = "ship"
        prompt = "ship.md"
    "#;
    let spec = support::spec_from(toml, &[("build.md", "Build {{task.title}}.")]);
    let mut sim = gated_spec_sim(&spec, ProposalMode::Off, ReportMode::BeforePr);
    sim.start();
    sim.settle("build", 0, Agent::Done("Built it."));
    sim.settle("report", 0, Agent::Done("Report v1."));
    let back = sim.decide(revise("Also handle whitespace-only input."));
    assert_eq!(spawned(&back), slots(&[("build", 2, 0)]));
    let prompt = &spawn_plans(&back)[0].prompt;
    assert!(prompt.starts_with("Build Fix the widget."), "{prompt}");
    assert!(prompt.contains("## Feedback to address"), "{prompt}");
    assert!(prompt.contains("The operator reviewed the change report and asked for changes"));
    assert!(prompt.contains("Also handle whitespace-only input."));
}

/// Fail `default-pr-review`'s review `fails` times, then pass it.
fn review_after_failures(sim: &mut Sim, fails: u32) {
    for _ in 0..fails {
        sim.settle("implement", 0, Agent::Done("Fixed parse()."));
        let back = sim.settle("review", 0, Agent::Fail("off by one"));
        assert_eq!(spawned(&back).len(), 1, "{back:#?}");
        assert_eq!(spawned(&back)[0].0, "implement");
    }
    sim.settle("implement", 0, Agent::Done("Fixed parse()."));
    sim.settle("review", 0, Agent::Pass);
}

#[test]
fn a_revised_report_starts_fresh_fix_loops() {
    let mut sim = gated_sim("default-pr-review", ProposalMode::Off, ReportMode::BeforePr);
    sim.start();
    review_after_failures(&mut sim, 2);
    let gate = sim.settle("report", 0, Agent::Done("Report v1."));
    assert_eq!(awaited(&gate).len(), 1);
    let back = sim.decide(revise("Also handle whitespace-only input."));
    assert_eq!(spawned(&back), slots(&[("implement", 4, 0)]));
    sim.settle("implement", 0, Agent::Done("Handled it."));
    let fixed = sim.settle("review", 0, Agent::Fail("misses tabs"));
    assert_eq!(
        spawned(&fixed),
        slots(&[("implement", 5, 0)]),
        "the revised work gets its own fix loops"
    );
    sim.settle("implement", 0, Agent::Done("Handled tabs."));
    sim.settle("review", 0, Agent::Fail("misses tabs"));
    sim.settle("implement", 0, Agent::Done("Handled tabs."));
    let done = sim.settle("review", 0, Agent::Fail("misses tabs"));
    match finish_state(&done) {
        Some(RunState::Failed { stage_id, error }) => {
            assert_eq!(stage_id, "review");
            assert!(error.contains("after 2 of 2 fix loop(s)"), "{error}");
        }
        other => panic!("the fresh budget is still bounded, got {other:?}"),
    }
}

#[test]
fn a_with_pr_report_never_waits() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Off, ReportMode::WithPr);
    assert!(sim.gates.is_empty());
    sim.start();
    let report = sim.settle("implement", 0, Agent::Done("Fixed parse()."));
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    let ship = sim.settle(
        "report",
        0,
        Agent::Done("Report at .archon/reports/change."),
    );
    assert!(awaited(&ship).is_empty());
    assert!(spawned(&ship).is_empty(), "no agent ships");
    let body = &ship_plans(&ship)[0].body;
    assert!(body.contains("Report at .archon/reports/change."), "{body}");
}

/// A pipeline that still ships with an agent reads the run's merge knob
/// from its template, as before archond shipped the built-ins.
#[test]
fn merge_auto_reaches_a_ship_agent_s_template() {
    let toml = r#"
        [pipeline]
        name = "own-ship"
        description = "d"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "ship"
        kind = "ship"
        prompt = "ship.md"
    "#;
    let prompts = [
        ("build.md", "Build {{task.title}}."),
        ("ship.md", "Auto-merge allowed: **`{{ship.auto_merge}}`**"),
    ];
    let spec = Arc::new(support::spec_from(toml, &prompts));
    let plan = support::plan_for(&spec, &[]);
    for (auto_merge, word) in [(true, "**`true`**"), (false, "**`false`**")] {
        let mut sim = Sim::new(spec.clone(), plan.clone());
        sim.base = support::base_vars_merging(&sim.run.id, &sim.run.item, auto_merge);
        sim.driver = RunDriver::new(spec.clone(), plan.clone(), sim.base.clone()).unwrap();
        sim.start();
        let ship = sim.settle("build", 0, Agent::Done("done"));
        assert_eq!(spawned(&ship), slots(&[("ship", 1, 0)]));
        assert!(ship_plans(&ship).is_empty(), "its own agent ships");
        let prompt = &spawn_plans(&ship)[0].prompt;
        assert!(
            prompt.contains(&format!("Auto-merge allowed: {word}")),
            "{prompt}"
        );
    }
}

#[test]
fn decisions_are_refused_for_stages_that_are_not_gates_or_already_decided() {
    let mut sim = gated_sim("quick-pr", ProposalMode::Lavish, ReportMode::Off);
    sim.start();
    propose(&mut sim, 1);
    let mut driver = sim.resumed_driver();
    let err = driver
        .record_decision("implement", 1, approve(None))
        .unwrap_err();
    assert!(err.to_string().contains("not an operator gate"), "{err}");
    driver.record_decision("propose", 1, approve(None)).unwrap();
    driver.record_decision("propose", 1, approve(None)).unwrap();
    let err = driver
        .record_decision("propose", 1, revise("x"))
        .unwrap_err();
    assert!(err.to_string().contains("already approved"), "{err}");
}

/// A lead of `adaptive` finishing with its review on record.
fn reviewed(summary: &'static str) -> Agent {
    let review = serde_json::json!({"verdict": "pass", "reviewers": ["agt_reviewer"]});
    let mut result = serde_json::json!({ "review": review });
    let reviewer = archon_pipeline::review::Reviewer {
        model: "codex/astra".into(),
        agent: "agt_reviewer".into(),
    };
    archon_pipeline::review::record_check(&mut result, Some(Ok(vec![reviewer])));
    Agent::DoneStructured(summary, result)
}

#[test]
fn adaptive_proposal_and_report_gates_preserve_delegation_and_context() {
    let mut sim = gated_sim("adaptive", ProposalMode::Lavish, ReportMode::BeforePr);
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("propose", 1, 0)]));
    assert!(!spawn_plans(&first)[0].allow_delegation);
    propose(&mut sim, 1);
    let build = sim.decide(approve(Some("Preserve the old API")));
    assert_eq!(spawned(&build), slots(&[("build", 1, 0)]));
    assert!(spawn_plans(&build)[0].allow_delegation);
    assert!(
        spawn_plans(&build)[0]
            .prompt
            .contains("Preserve the old API")
    );
    let report = sim.settle("build", 0, reviewed("Built and reviewed"));
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    assert!(!spawn_plans(&report)[0].allow_delegation);
    let gate = sim.settle("report", 0, Agent::Done("Report v1"));
    assert_eq!(awaited(&gate)[0].subject, GateSubject::Report);
    let revised = sim.decide(revise("Handle empty input"));
    assert_eq!(spawned(&revised), slots(&[("build", 2, 0)]));
    assert!(spawn_plans(&revised)[0].allow_delegation);
    assert!(
        spawn_plans(&revised)[0]
            .prompt
            .contains("Handle empty input")
    );
    sim.settle("build", 0, reviewed("Fixed, verified and re-reviewed"));
    sim.settle("report", 0, Agent::Done("Report v2"));
    let ship = sim.decide(approve(None));
    assert!(spawned(&ship).is_empty(), "no agent ships");
    let plan = ship_plans(&ship)[0];
    assert_eq!(plan.record_stage, "build");
    assert!(
        plan.body.contains("REVIEW: pass, by codex/astra"),
        "{}",
        plan.body
    );
    assert!(plan.body.contains("## Change report\n\nReport v2"));
    let done = sim.ship_pr("https://github.com/o/r/pull/7");
    match finish_state(&done) {
        Some(RunState::Succeeded { summary }) => {
            assert!(summary.ends_with(&format!(
                "SHIPPED: https://github.com/o/r/pull/7 (commit {})",
                sim::COMMIT
            )));
        }
        other => panic!("expected a shipped run, got {other:?}"),
    }
}

#[test]
fn adaptive_with_pr_report_does_not_wait_for_approval() {
    let mut sim = gated_sim("adaptive", ProposalMode::Off, ReportMode::WithPr);
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("build", 1, 0)]));
    sim.settle("build", 0, reviewed("Verified and reviewed"));
    let ship = sim.settle("report", 0, Agent::Done("Change report"));
    assert!(spawned(&ship).is_empty() && awaited(&ship).is_empty());
    assert!(
        ship_plans(&ship)[0]
            .body
            .contains("## Change report\n\nChange report")
    );
}
