//! An item's autonomy knobs (ARCHITECTURE §18.1): which pipeline an
//! enqueue runs, the knobs it gets, the ceiling check, and the stages the
//! knobs add to its pipeline.

use archon_core::autonomy::{
    Autonomy, AutonomyPatch, AutonomyPolicy, MergeMode, ProposalMode, ReportMode,
};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model::ModelPlan;
use archon_core::queue::QueueItem;
use archon_pipeline::PipelineSpec;
use archon_pipeline::autonomy::{augment, review_level, ships};
use archon_queue::Store;

use crate::learned;
use crate::resolve::RepoEnv;

/// What an enqueue resolved to.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub pipeline: String,
    pub autonomy: Autonomy,
    /// The pipeline with the stages the knobs insert; what the item runs.
    pub spec: PipelineSpec,
    /// The pipeline as the catalog has it.
    pub base: PipelineSpec,
}

/// Resolve an enqueue's pipeline and knobs within the ceiling.
pub(crate) fn resolve(
    config: &Config,
    env: &RepoEnv,
    pipeline: Option<String>,
    patch: &AutonomyPatch,
) -> Result<Resolved, ArchonError> {
    let policy = env.autonomy(config);
    let name = pipeline_name(env, pipeline, patch, &policy);
    let base = env.catalog()?.get(&name)?.clone();
    let autonomy = knobs(&base, patch, &policy)?;
    check_ceiling(&policy, env, &base, autonomy)?;
    let spec = augment(&base, &autonomy)?;
    assert_eq!(spec.name(), name, "augmenting keeps the pipeline's name");
    Ok(Resolved {
        pipeline: name,
        autonomy,
        spec,
        base,
    })
}

/// An explicit pipeline wins; else the review knob's family, the
/// repository's default pipeline, and the default review's family.
fn pipeline_name(
    env: &RepoEnv,
    pipeline: Option<String>,
    patch: &AutonomyPatch,
    policy: &AutonomyPolicy,
) -> String {
    if let Some(name) = pipeline {
        return name;
    }
    if let Some(review) = patch.review {
        return review.pipeline().to_owned();
    }
    env.repo
        .as_ref()
        .and_then(|r| r.default_pipeline.clone())
        .or_else(|| env.config.repo.default_pipeline.clone())
        .unwrap_or_else(|| policy.default.review.pipeline().to_owned())
}

/// The patch over the defaults. The pipeline fixes `review`, and a
/// pipeline that does not ship takes no proposal, report or merge.
fn knobs(
    base: &PipelineSpec,
    patch: &AutonomyPatch,
    policy: &AutonomyPolicy,
) -> Result<Autonomy, ArchonError> {
    assert!(
        policy.default.excess(policy.ceiling).is_empty(),
        "an item that asks for nothing stays within the ceiling"
    );
    let review = review_level(base);
    if let Some(asked) = patch.review.filter(|asked| *asked != review) {
        return Err(ArchonError::Invalid(format!(
            "pipeline {:?} runs review={review}, not review={asked}; name pipeline {:?} \
             instead, or leave review out",
            base.name(),
            asked.pipeline()
        )));
    }
    if ships(base) {
        return Ok(Autonomy {
            review,
            ..patch.over(policy.default)
        });
    }
    let shipping_only = [
        patch
            .proposal
            .filter(|v| *v != ProposalMode::Off)
            .map(|_| "proposal"),
        patch
            .report
            .filter(|v| *v != ReportMode::Off)
            .map(|_| "report"),
        patch
            .merge
            .filter(|v| *v != MergeMode::Manual)
            .map(|_| "merge"),
    ];
    if let Some(knob) = shipping_only.into_iter().flatten().next() {
        return Err(ArchonError::Invalid(format!(
            "{knob} applies only to pipelines that ship; {:?} has no ship stage",
            base.name()
        )));
    }
    Ok(Autonomy {
        review,
        proposal: ProposalMode::Off,
        report: ReportMode::Off,
        merge: MergeMode::Manual,
        tools: patch.tools.unwrap_or(policy.default.tools),
    })
}

/// Refuse knobs above the ceiling. Only `tools` means anything for a
/// pipeline that does not ship.
pub(crate) fn check_ceiling(
    policy: &AutonomyPolicy,
    env: &RepoEnv,
    base: &PipelineSpec,
    autonomy: Autonomy,
) -> Result<(), ArchonError> {
    let shipping = ships(base);
    policy.check(autonomy, |knob| shipping || knob == "tools", &env.scope())
}

/// The item's pipeline as the catalog has it now, and with its knobs'
/// stages; errors when a knob no longer fits the pipeline.
pub(crate) fn item_specs(
    env: &RepoEnv,
    item: &QueueItem,
) -> Result<(PipelineSpec, PipelineSpec), ArchonError> {
    let base = env.catalog()?.get(&item.pipeline)?.clone();
    let spec = augment(&base, &item.autonomy)?;
    assert_eq!(
        base.name(),
        item.pipeline,
        "the catalog returns the named pipeline"
    );
    Ok((base, spec))
}

/// `plan` with the stages it leaves out: first what was learned for `repo`,
/// then the first choice of the pipeline's first stage for those the knobs inserted.
pub(crate) fn fill_plan(
    store: &Store,
    repo: Option<&RepoId>,
    base: &PipelineSpec,
    spec: &PipelineSpec,
    mut plan: ModelPlan,
) -> Result<ModelPlan, ArchonError> {
    let learned = learned::default_plan(store, repo, spec)?.unwrap_or_default();
    for (stage, choices) in learned.0 {
        assert!(
            spec.stage(&stage).is_some(),
            "{stage:?} is no stage of the pipeline"
        );
        assert!(!choices.is_empty(), "a learned default names a choice");
        plan.0.entry(stage).or_insert(choices);
    }
    fill_static_stages(base, spec, plan)
}

pub(crate) fn fill_static_stages(
    base: &PipelineSpec,
    spec: &PipelineSpec,
    mut plan: ModelPlan,
) -> Result<ModelPlan, ArchonError> {
    let first = base
        .file
        .stages
        .first()
        .expect("validated pipelines have stages");
    let Some(choice) = plan.choices_for(&first.id).and_then(|c| c.first()).cloned() else {
        return Ok(plan);
    };
    for stage in &spec.file.stages {
        if base.stage(&stage.id).is_none() && plan.choices_for(&stage.id).is_none() {
            plan.0.insert(stage.id.clone(), vec![choice.clone()]);
        }
    }
    debug_assert!(plan.0.len() <= spec.file.stages.len() + base.file.stages.len());
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::autonomy::{ReviewLevel, ToolsMode};
    use archon_pipeline::Catalog;

    fn builtin(name: &str) -> PipelineSpec {
        Catalog::builtin().get(name).expect("built-in").clone()
    }

    fn policy() -> AutonomyPolicy {
        AutonomyPolicy::resolve(&Default::default(), None, None)
    }

    #[test]
    fn the_pipeline_fixes_review_and_a_conflict_is_refused() {
        let strict = builtin("strict-pr-review");
        let chosen = knobs(&strict, &AutonomyPatch::default(), &policy()).unwrap();
        assert_eq!(chosen.review, ReviewLevel::Strict);
        let conflict = AutonomyPatch {
            review: Some(ReviewLevel::None),
            ..AutonomyPatch::default()
        };
        let err = knobs(&strict, &conflict, &policy())
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"quick-pr\""), "{err}");
    }

    #[test]
    fn pipelines_that_do_not_ship_take_only_tools() {
        let ask = builtin("ask");
        let yolo = AutonomyPatch::parse_list("tools=yolo").unwrap();
        let chosen = knobs(&ask, &yolo, &policy()).unwrap();
        assert_eq!(chosen.tools, ToolsMode::Yolo);
        assert_eq!(chosen.merge, MergeMode::Manual);
        let merge = AutonomyPatch::parse_list("merge=auto").unwrap();
        let err = knobs(&ask, &merge, &policy()).unwrap_err().to_string();
        assert!(
            err.contains("merge applies only to pipelines that ship"),
            "{err}"
        );
    }

    #[test]
    fn inserted_stages_borrow_the_first_stage_choice() {
        let base = builtin("default-pr-review");
        let knobs = Autonomy {
            proposal: ProposalMode::Lavish,
            report: ReportMode::WithPr,
            ..Autonomy::default()
        };
        let spec = augment(&base, &knobs).unwrap();
        let choice = archon_core::model::ModelChoice {
            model: "claude-code/claude-opus-5-5".parse().unwrap(),
            effort: archon_core::model::Effort::High,
        };
        let mut plan = ModelPlan::default();
        for stage in &base.file.stages {
            plan.0.insert(stage.id.clone(), vec![choice.clone()]);
        }
        let store = Store::open_in_memory().unwrap();
        let filled = fill_plan(&store, None, &base, &spec, plan.clone()).unwrap();
        assert_eq!(filled.choices_for("propose"), Some(&[choice.clone()][..]));
        assert_eq!(filled.choices_for("report"), Some(&[choice][..]));
        assert_eq!(filled.0.len(), plan.0.len() + 2);
    }
}
