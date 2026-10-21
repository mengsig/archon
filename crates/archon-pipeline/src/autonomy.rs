//! The autonomy knobs applied to a pipeline (ARCHITECTURE §18): the stages
//! `proposal` and `report` insert when the pipeline declares none, and the
//! stages whose output waits for the operator.

use archon_core::autonomy::{Autonomy, Knob, ProposalMode, ReportMode, ReviewLevel};
use archon_core::catalog::{Gate, StageKind};
use archon_core::error::ArchonError;
use archon_core::model::Effort;
use archon_core::run::GateSubject;

use crate::spec::{PipelineSpec, StageSpec};

pub const PROPOSE_STAGE: &str = "propose";
pub const REPORT_STAGE: &str = "report";
/// File names of the shared prompts inside a run's frozen snapshot.
pub const PROPOSE_PROMPT: &str = "archon-propose.md";
pub const REPORT_PROMPT: &str = "archon-report.md";
const PROPOSE_TEMPLATE: &str = include_str!("../../../pipelines/_shared/propose.md");
const REPORT_TEMPLATE: &str = include_str!("../../../pipelines/_shared/report.md");
const INSERTED_TIMEOUT_MINUTES: u32 = 45;

/// Whether the pipeline delivers a change: archond ships it, or it has a
/// ship stage.
pub fn ships(spec: &PipelineSpec) -> bool {
    ship_index(spec).is_some()
}

/// Where shipping happens among the stages: at the ship stage, or after the
/// last stage when archond ships.
fn ship_index(spec: &PipelineSpec) -> Option<usize> {
    let stages = &spec.file.stages;
    if spec.file.pipeline.ship.is_some() {
        return Some(stages.len());
    }
    stages.iter().position(|s| s.kind == StageKind::Ship)
}

/// A pipeline's review level: the stricter of what it declares and its gates'
/// (gated tests and gated reviews: strict; either alone: standard; else none).
pub fn review_level(spec: &PipelineSpec) -> ReviewLevel {
    let by_gates = gated_review_level(spec);
    match &spec.file.pipeline.review {
        Some(declared) => declared.level.lower(by_gates),
        None => by_gates,
    }
}

/// The review a pipeline's gated stages run whatever an item asks for.
fn gated_review_level(spec: &PipelineSpec) -> ReviewLevel {
    let gated = |kind: StageKind| {
        spec.file
            .stages
            .iter()
            .any(|s| s.kind == kind && s.gate != Gate::None)
    };
    match (gated(StageKind::Review), gated(StageKind::Test)) {
        (true, true) => ReviewLevel::Strict,
        (true, false) | (false, true) => ReviewLevel::Standard,
        (false, false) => ReviewLevel::None,
    }
}

/// Whether an item chooses the pipeline's review level: one of its stages
/// holds the review and delegates as many reviewers as the level needs.
pub fn review_is_chosen(spec: &PipelineSpec) -> bool {
    spec.file.pipeline.review.is_some()
}

/// The review level an item gets on `spec`: what it `asked` for, else the
/// pipeline's own raised to `floor`; never below what its gated stages run.
pub fn chosen_review(
    spec: &PipelineSpec,
    asked: Option<ReviewLevel>,
    floor: ReviewLevel,
) -> ReviewLevel {
    let declared = spec.file.pipeline.review.as_ref();
    let declared = declared.expect("only a pipeline that declares its review lets an item choose");
    let level = asked.unwrap_or_else(|| declared.level.lower(floor));
    level.lower(gated_review_level(spec))
}

/// The pipeline's propose stage, which is always first when present.
pub fn propose_stage(spec: &PipelineSpec) -> Option<&StageSpec> {
    let first = spec.file.stages.first()?;
    (first.kind == StageKind::Propose).then_some(first)
}

/// The change report: the last report stage before the ship stage.
pub fn change_report_stage(spec: &PipelineSpec) -> Option<&StageSpec> {
    let ship = ship_index(spec)?;
    spec.file.stages[..ship]
        .iter()
        .rev()
        .find(|s| s.kind == StageKind::Report)
}

/// `spec` as run with `autonomy`: the stages its knobs add (`propose` first,
/// `report` before ship), and its review level where a stage holds the review.
pub fn augment(spec: &PipelineSpec, autonomy: &Autonomy) -> Result<PipelineSpec, ArchonError> {
    let mut out = spec.clone();
    if let Some(review) = &mut out.file.pipeline.review {
        review.level = autonomy.review;
    }
    let wants_proposal = autonomy.proposal == ProposalMode::Lavish;
    if wants_proposal && propose_stage(spec).is_none() {
        require_ship(spec, "proposal")?;
        let stage = inserted(PROPOSE_STAGE, StageKind::Propose, PROPOSE_PROMPT);
        insert(&mut out, 0, stage, PROPOSE_TEMPLATE)?;
    }
    if autonomy.report != ReportMode::Off && change_report_stage(spec).is_none() {
        require_ship(spec, "report")?;
        let at = ship_index(&out).expect("require_ship checked a ship stage");
        let stage = inserted(REPORT_STAGE, StageKind::Report, REPORT_PROMPT);
        insert(&mut out, at, stage, REPORT_TEMPLATE)?;
    }
    out.validate(spec.name())?;
    assert!(out.file.stages.len() <= spec.file.stages.len() + 2);
    Ok(out)
}

fn require_ship(spec: &PipelineSpec, knob: &str) -> Result<(), ArchonError> {
    if ships(spec) {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "{knob} applies only to pipelines that ship; {:?} ships nothing",
        spec.name()
    )))
}

fn inserted(id: &str, kind: StageKind, prompt: &str) -> StageSpec {
    StageSpec {
        id: id.to_owned(),
        kind,
        prompt: prompt.to_owned(),
        fan_out: 1,
        gate: Gate::None,
        timeout_minutes: INSERTED_TIMEOUT_MINUTES,
        effort: Effort::Medium,
        min_tier: None,
        tier: None,
        allow_delegation: false,
        nudge: None,
        on_fail: None,
        roles: Vec::new(),
        role_prompts: Default::default(),
        roles_from: None,
    }
}

fn insert(
    spec: &mut PipelineSpec,
    at: usize,
    stage: StageSpec,
    template: &str,
) -> Result<(), ArchonError> {
    if spec.stage(&stage.id).is_some() {
        return Err(ArchonError::Pipeline(format!(
            "pipeline {:?} already has a stage {:?} that is not a {:?} stage; rename it to use this knob",
            spec.name(),
            stage.id,
            stage.kind
        )));
    }
    if spec.prompts.contains_key(&stage.prompt) {
        return Err(ArchonError::Pipeline(format!(
            "pipeline {:?} has its own prompt file {:?}, which Archon reserves",
            spec.name(),
            stage.prompt
        )));
    }
    assert!(at <= spec.file.stages.len(), "insert position in range");
    spec.prompts
        .insert(stage.prompt.clone(), template.to_owned());
    spec.file.stages.insert(at, stage);
    Ok(())
}

/// The stages whose passing output waits for the operator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorGates {
    /// The propose stage, when `proposal = lavish`.
    pub proposal: Option<String>,
    /// The change report stage, when `report = before-pr`.
    pub report: Option<String>,
}

impl OperatorGates {
    /// Gates for an augmented `spec`; knobs whose stage is missing gate
    /// nothing, as [`augment`] has refused those already.
    pub fn for_run(spec: &PipelineSpec, autonomy: &Autonomy) -> Self {
        let proposal = (autonomy.proposal == ProposalMode::Lavish)
            .then(|| propose_stage(spec).map(|s| s.id.clone()))
            .flatten();
        let report = (autonomy.report == ReportMode::BeforePr)
            .then(|| change_report_stage(spec).map(|s| s.id.clone()))
            .flatten();
        Self { proposal, report }
    }

    pub fn subject(&self, stage_id: &str) -> Option<GateSubject> {
        if self.proposal.as_deref() == Some(stage_id) {
            return Some(GateSubject::Proposal);
        }
        if self.report.as_deref() == Some(stage_id) {
            return Some(GateSubject::Report);
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.proposal.is_none() && self.report.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    fn builtin_spec(name: &str) -> PipelineSpec {
        Catalog::builtin().get(name).expect("built-in").clone()
    }

    fn knobs(proposal: ProposalMode, report: ReportMode) -> Autonomy {
        Autonomy {
            proposal,
            report,
            ..Autonomy::default()
        }
    }

    /// An enqueue that names no pipeline may ask for any review level.
    #[test]
    fn the_default_pipeline_ships_at_every_review_level() {
        let spec = builtin_spec(crate::builtin::DEFAULT_PIPELINE);
        assert!(ships(&spec) && review_is_chosen(&spec));
        for level in [
            ReviewLevel::None,
            ReviewLevel::Standard,
            ReviewLevel::Strict,
        ] {
            assert_eq!(chosen_review(&spec, Some(level), ReviewLevel::None), level);
        }
        let refactor = builtin_spec("refactor");
        assert!(!review_is_chosen(&refactor), "its gates fix its review");
        assert_eq!(review_level(&refactor), ReviewLevel::Strict);
        assert!(!ships(&builtin_spec("ask")));
    }

    #[test]
    fn knobs_insert_propose_first_and_report_before_ship() {
        let spec = builtin_spec("refactor");
        let both = augment(&spec, &knobs(ProposalMode::Lavish, ReportMode::BeforePr)).unwrap();
        let ids: Vec<&str> = both.file.stages.iter().map(|s| s.id.as_str()).collect();
        let stages = [
            "propose", "baseline", "refactor", "verify", "review", "report",
        ];
        assert_eq!(ids, stages);
        assert_eq!(ship_index(&both), Some(6), "archond ships after the report");
        assert!(both.prompts.contains_key(PROPOSE_PROMPT));
        let gates =
            OperatorGates::for_run(&both, &knobs(ProposalMode::Lavish, ReportMode::BeforePr));
        assert_eq!(gates.subject("propose"), Some(GateSubject::Proposal));
        assert_eq!(gates.subject("report"), Some(GateSubject::Report));
        let with_pr = knobs(ProposalMode::Off, ReportMode::WithPr);
        let plain = augment(&spec, &with_pr).unwrap();
        assert_eq!(plain.file.stages.len(), 5);
        assert!(
            OperatorGates::for_run(&plain, &with_pr).is_empty(),
            "with-pr never waits"
        );
    }

    #[test]
    fn shared_page_prompts_scale_detail_without_changing_their_contracts() {
        for (template, path) in [
            (PROPOSE_TEMPLATE, ".archon/reports/proposal/index.html"),
            (REPORT_TEMPLATE, ".archon/reports/change/index.html"),
        ] {
            for required in ["Goal", "Approach", "Risk", "Acceptance check", "Open"] {
                assert!(template.contains(required), "missing {required} in prompt");
            }
            assert!(template.contains("few lines"));
            assert!(template.contains(path));
            assert!(template.contains("structured"));
        }
        assert!(PROPOSE_TEMPLATE.contains("previous proposal"));
        assert!(REPORT_TEMPLATE.contains("review verdicts"));
    }

    #[test]
    fn an_augmented_manifest_round_trips_through_toml() {
        let spec = builtin_spec("refactor");
        let both = augment(&spec, &knobs(ProposalMode::Lavish, ReportMode::BeforePr)).unwrap();
        let text = toml::to_string(&both.file).unwrap();
        let back: crate::spec::PipelineFile = toml::from_str(&text).unwrap();
        assert_eq!(back, both.file);
        assert_eq!(back.stages.len(), spec.file.stages.len() + 2);
    }

    #[test]
    fn the_summary_marks_the_stage_that_delegates_the_reviewers() {
        let summary = builtin_spec("direct").summary();
        let marked: Vec<&str> = summary
            .stages
            .iter()
            .filter(|stage| stage.delegates_reviewers)
            .map(|stage| stage.id.as_str())
            .collect();
        assert_eq!(marked, ["implement"]);
        assert!(
            !builtin_spec("ask")
                .summary()
                .stages
                .iter()
                .any(|s| s.delegates_reviewers)
        );
    }

    #[test]
    fn declared_stages_are_not_duplicated_and_non_shipping_pipelines_refuse() {
        let spec = builtin_spec("direct");
        let wanted = knobs(ProposalMode::Lavish, ReportMode::WithPr);
        let once = augment(&spec, &wanted).unwrap();
        let twice = augment(&once, &wanted).unwrap();
        assert_eq!(once.file.stages, twice.file.stages);
        let ask = builtin_spec("ask");
        let err = augment(&ask, &wanted).unwrap_err().to_string();
        assert!(err.contains("only to pipelines that ship"), "{err}");
        assert_eq!(augment(&ask, &Autonomy::default()).unwrap(), ask);
    }
}
