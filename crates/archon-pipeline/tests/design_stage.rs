//! Live design sessions: `proposal = live` inserts the `design` stage first.

use archon_core::autonomy::{Autonomy, ProposalMode, ReportMode, ReviewLevel};
use archon_core::catalog::StageKind;
use archon_core::run::GateSubject;
use archon_pipeline::autonomy::{
    DESIGN_STAGE_ID, OperatorGates, augment, design_stage, propose_stage,
};
use archon_pipeline::contract::DESIGN_RULES;
use archon_pipeline::spec::PipelineSpec;

mod support;

use archon_pipeline::RunDriver;
use std::sync::Arc;

fn spec() -> Arc<PipelineSpec> {
    support::builtin("direct")
}

fn knobs(proposal: ProposalMode) -> Autonomy {
    Autonomy {
        review: ReviewLevel::None,
        proposal,
        report: ReportMode::Off,
        ..Autonomy::default()
    }
}

#[test]
fn live_inserts_exactly_one_design_stage_first() {
    let base = spec();
    let out = augment(&base, &knobs(ProposalMode::Live)).unwrap();
    assert_eq!(out.file.stages.len(), base.file.stages.len() + 1);
    let first = &out.file.stages[0];
    assert_eq!(first.id, DESIGN_STAGE_ID);
    assert_eq!(first.kind, StageKind::Design);
    assert_eq!(first.fan_out, 1);
    assert_eq!(first.timeout_minutes, 240);
    assert_eq!(
        out.file
            .stages
            .iter()
            .filter(|s| s.kind == StageKind::Design)
            .count(),
        1
    );
    assert!(out.prompts[&first.prompt].contains("design_preview"));
    assert!(design_stage(&out).is_some());
    assert_eq!(propose_stage(&out).map(|s| s.id.as_str()), Some("design"));
    assert!(out.file.stages.iter().all(|s| s.kind != StageKind::Propose));
}

#[test]
fn lavish_and_off_are_unchanged() {
    let base = spec();
    let lavish = augment(&base, &knobs(ProposalMode::Lavish)).unwrap();
    assert_eq!(lavish.file.stages[0].id, "propose");
    assert_eq!(lavish.file.stages[0].kind, StageKind::Propose);
    assert!(design_stage(&lavish).is_none());
    let off = augment(&base, &knobs(ProposalMode::Off)).unwrap();
    assert_eq!(off.file.stages.len(), base.file.stages.len());
    assert!(propose_stage(&off).is_none());
}

#[test]
fn the_design_gate_is_the_proposal_gate() {
    let autonomy = knobs(ProposalMode::Live);
    let out = augment(&spec(), &autonomy).unwrap();
    let gates = OperatorGates::for_run(&out, &autonomy);
    assert_eq!(gates.proposal.as_deref(), Some("design"));
    assert_eq!(gates.subject("design"), Some(GateSubject::Proposal));
    assert_eq!(gates.subject("implement"), None);
}

#[test]
fn design_rules_state_the_contract() {
    for needle in [
        "setsid",
        ".archon/services/<name>.pid",
        "design_preview",
        "wait_for",
        "Archon-Note: D<n>",
        "exactly one commit",
        "design_note",
        "needs_reply",
        "Drop D<n>",
        "never revert",
        "finish",
        "complete_task",
        "untrusted",
    ] {
        assert!(
            DESIGN_RULES.to_lowercase().contains(&needle.to_lowercase()),
            "missing {needle:?}"
        );
    }
}

#[test]
fn with_gates_accepts_the_design_stage_and_keeps_its_agent() {
    let autonomy = knobs(ProposalMode::Live);
    let out = Arc::new(augment(&spec(), &autonomy).unwrap());
    let gates = OperatorGates::for_run(&out, &autonomy);
    let plan = support::plan_for(&out, &["claude-code/opus"]);
    let base = support::base_vars_for(
        &out,
        &archon_core::id::PipelineRunId::new(),
        &archon_core::id::QueueItemId::new(),
    );
    let mut driver = RunDriver::new(out.clone(), plan, base)
        .unwrap()
        .with_gates(gates)
        .expect("a design stage takes the proposal gate");
    assert!(driver.keeps_agent("design"));
    assert!(!driver.keeps_agent("implement"));
    let _ = driver.start();
}

#[test]
fn a_gate_naming_a_work_stage_is_refused() {
    let out = Arc::new(augment(&spec(), &knobs(ProposalMode::Live)).unwrap());
    let plan = support::plan_for(&out, &["claude-code/opus"]);
    let base = support::base_vars_for(
        &out,
        &archon_core::id::PipelineRunId::new(),
        &archon_core::id::QueueItemId::new(),
    );
    let gates = OperatorGates {
        proposal: Some("implement".into()),
        report: None,
    };
    let driver = RunDriver::new(out, plan, base).unwrap();
    assert!(driver.with_gates(gates).is_err());
}
