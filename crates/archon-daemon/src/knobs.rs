//! An item's autonomy knobs (ARCHITECTURE §18.1): which pipeline an
//! enqueue runs, the knobs it gets, the ceiling check, and the stages the
//! knobs add to its pipeline.

use archon_core::autonomy::{
    Autonomy, AutonomyPatch, AutonomyPolicy, MergeMode, ProposalMode, ReportMode, ReviewLevel,
};
use archon_core::catalog::Tier;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::policy::ModelPolicy;
use archon_core::queue::QueueItem;
use archon_pipeline::PipelineSpec;
use archon_pipeline::autonomy::{augment, chosen_review, review_is_chosen, review_level, ships};
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

/// An item's review level on `base`: where a stage holds the review, the one
/// asked for, else `base`'s raised to the ceiling's; gated stages fix it.
fn review_of(
    base: &PipelineSpec,
    patch: &AutonomyPatch,
    policy: &AutonomyPolicy,
) -> Result<ReviewLevel, ArchonError> {
    if review_is_chosen(base) {
        return Ok(chosen_review(base, patch.review, policy.ceiling.review));
    }
    let review = review_level(base);
    if let Some(asked) = patch.review.filter(|asked| *asked != review) {
        return Err(ArchonError::Invalid(format!(
            "pipeline {:?} runs review={review} in its own stages, not review={asked}; name \
             pipeline {:?} or one that takes a review level (`adaptive`, `direct`) instead, \
             or leave review out",
            base.name(),
            asked.pipeline()
        )));
    }
    Ok(review)
}

/// The patch over the defaults, with the review [`review_of`] gives. A
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
    let review = review_of(base, patch, policy)?;
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
            "{knob} applies only to pipelines that ship; {:?} ships nothing",
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

/// A stage's choice when nothing names one: the orchestrator's model, else
/// `policy`'s first, at the tier's effort within its cap; `None` if none.
pub(crate) fn default_choice(
    config: &Config,
    policy: &ModelPolicy,
    spec: &PipelineSpec,
) -> Option<ModelChoice> {
    let own = &config.orchestrator.model;
    let model = match policy.permits_model(own) {
        true => own.clone(),
        false => {
            let allowed = policy.allow.iter().flatten();
            allowed
                .into_iter()
                .find(|m| policy.permits_model(m))?
                .clone()
        }
    };
    let suggested = match spec.file.nudges.tier {
        Some(Tier::Fast) => Effort::Low,
        Some(Tier::Balanced) | None => Effort::Medium,
        Some(Tier::Strongest) => Effort::High,
    };
    let effort = policy
        .max_effort
        .map_or(suggested, |cap| suggested.min(cap));
    let choice = ModelChoice { model, effort };
    debug_assert!(
        policy.check(&choice).is_ok(),
        "a default is within the policy"
    );
    Some(choice)
}

/// `plan` with the stages it leaves out: first what was learned for `repo`
/// within `policy`, then the static choices of [`fill_static_stages`].
pub(crate) fn fill_plan(
    store: &Store,
    repo: Option<&RepoId>,
    (base, spec): (&PipelineSpec, &PipelineSpec),
    mut plan: ModelPlan,
    policy: &ModelPolicy,
    fallback: Option<&ModelChoice>,
) -> Result<ModelPlan, ArchonError> {
    let learned = learned::default_plan(store, repo, spec, policy)?;
    for (stage, choices) in learned.0 {
        assert!(
            spec.stage(&stage).is_some(),
            "{stage:?} is no stage of the pipeline"
        );
        assert!(!choices.is_empty(), "a learned default names a choice");
        plan.0.entry(stage).or_insert(choices);
    }
    fill_static_stages(base, spec, plan, fallback)
}

/// `plan` completed without learning: `fallback` for the pipeline's own
/// stages, its first stage's first choice for those the knobs inserted.
pub(crate) fn fill_static_stages(
    base: &PipelineSpec,
    spec: &PipelineSpec,
    mut plan: ModelPlan,
    fallback: Option<&ModelChoice>,
) -> Result<ModelPlan, ArchonError> {
    for stage in base
        .file
        .stages
        .iter()
        .filter(|s| spec.stage(&s.id).is_some())
    {
        if let (None, Some(choice)) = (plan.choices_for(&stage.id), fallback) {
            plan.0.insert(stage.id.clone(), vec![choice.clone()]);
        }
    }
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
        let policy = ModelPolicy::default();
        let filled = fill_plan(&store, None, (&base, &spec), plan.clone(), &policy, None).unwrap();
        assert_eq!(filled.choices_for("propose"), Some(&[choice.clone()][..]));
        assert_eq!(filled.choices_for("report"), Some(&[choice][..]));
        assert_eq!(filled.0.len(), plan.0.len() + 2);
    }

    /// An enqueue may leave the whole plan out: every stage gets the default.
    #[test]
    fn stages_an_enqueue_leaves_out_get_the_default_choice() {
        let base = builtin("default-pr-review");
        let knobs = Autonomy {
            report: ReportMode::WithPr,
            ..Autonomy::default()
        };
        let spec = augment(&base, &knobs).unwrap();
        let config = Config::default();
        let default = default_choice(&config, &ModelPolicy::default(), &spec).unwrap();
        assert_eq!(default.model, config.orchestrator.model);
        let named = archon_core::model::ModelChoice {
            model: "codex/gpt-5.5".parse().unwrap(),
            effort: Effort::High,
        };
        let mut plan = ModelPlan::default();
        plan.0.insert("review".into(), vec![named.clone()]);
        let store = Store::open_in_memory().unwrap();
        let policy = ModelPolicy::default();
        let filled =
            fill_plan(&store, None, (&base, &spec), plan, &policy, Some(&default)).unwrap();
        assert_eq!(filled.choices_for("review"), Some(&[named][..]), "kept");
        for stage in ["implement", "report"] {
            assert_eq!(filled.choices_for(stage), Some(&[default.clone()][..]));
        }
        assert_eq!(filled.0.len(), spec.file.stages.len());
    }

    #[test]
    fn the_default_choice_follows_the_policy_and_the_pipeline_s_tier() {
        let config = Config::default();
        let own = config.orchestrator.model.clone();
        let other: archon_core::model::ModelRef = "codex/gpt-5.5".parse().unwrap();
        let direct = builtin("direct");
        let strict = builtin("strict-pr-review");
        let open = ModelPolicy::default();
        let effort = |spec| default_choice(&config, &open, spec).unwrap().effort;
        assert!(
            effort(&direct) < effort(&strict),
            "a fast pipeline thinks less"
        );
        let capped = ModelPolicy::from_lists(&[], &[], Some(Effort::Low));
        let low = default_choice(&config, &capped, &strict).unwrap();
        assert_eq!(low.effort, Effort::Low, "never above the policy's cap");
        let elsewhere = ModelPolicy::from_lists(std::slice::from_ref(&other), &[], None);
        let picked = default_choice(&config, &elsewhere, &strict).unwrap();
        assert_eq!(
            picked.model, other,
            "the orchestrator's model is not allowed"
        );
        let nothing = ModelPolicy::from_lists(&[], &[own], None);
        assert_eq!(default_choice(&config, &nothing, &strict), None);
    }

    /// The review level is the item's on a pipeline whose stage holds it.
    #[test]
    fn a_pipeline_that_holds_its_review_takes_the_level_an_item_asks_for() {
        let asked = |level| AutonomyPatch {
            review: Some(level),
            ..AutonomyPatch::default()
        };
        for name in ["adaptive", "direct"] {
            let spec = builtin(name);
            for level in [
                ReviewLevel::None,
                ReviewLevel::Standard,
                ReviewLevel::Strict,
            ] {
                let chosen = knobs(&spec, &asked(level), &policy()).unwrap();
                assert_eq!(chosen.review, level, "{name} at {level}");
            }
        }
        let own = |name| knobs(&builtin(name), &AutonomyPatch::default(), &policy()).unwrap();
        assert_eq!(own("adaptive").review, ReviewLevel::Standard);
        assert_eq!(own("direct").review, ReviewLevel::None);
        let mut reviewed = policy();
        reviewed.ceiling.review = ReviewLevel::Standard;
        let raised = knobs(&builtin("direct"), &AutonomyPatch::default(), &reviewed).unwrap();
        assert_eq!(
            raised.review,
            ReviewLevel::Standard,
            "raised to the ceiling"
        );
    }
}
