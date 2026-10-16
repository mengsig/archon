//! An item's autonomy knobs (ARCHITECTURE §18.1): which pipeline an
//! enqueue runs, the knobs it gets, the ceiling check, and the stages the
//! knobs add to its pipeline.

use archon_core::autonomy::{
    Autonomy, AutonomyPatch, AutonomyPolicy, MergeMode, ProposalMode, ReportMode, ReviewLevel,
};
use archon_core::catalog::Tier;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model::{Effort, ModelChoice, ModelPlan, ModelPlanDraft, ModelReasons, ModelRef};
use archon_core::model_tier::{ModelCatalog, TierChoice};
use archon_core::policy::ModelPolicy;
use archon_core::provider_route::{ProviderRoute, ProviderWeights};
use archon_core::queue::QueueItem;
use archon_pipeline::PipelineSpec;
use archon_pipeline::autonomy::{augment, chosen_review, review_is_chosen, review_level, ships};
use archon_pipeline::builtin::DEFAULT_PIPELINE;
use archon_queue::Store;

use crate::context::DaemonContext;
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
    ctx: &DaemonContext,
    env: &RepoEnv,
    pipeline: Option<String>,
    patch: &AutonomyPatch,
) -> Result<Resolved, ArchonError> {
    let policy = env.autonomy(ctx);
    let (name, patch) = match pipeline_name(ctx, env, pipeline, patch) {
        Some(name) => (name, *patch),
        None => (
            DEFAULT_PIPELINE.to_owned(),
            defaulted_review(patch, &policy),
        ),
    };
    let base = env.catalog()?.get(&name)?.clone();
    let autonomy = knobs(&base, &patch, &policy)?;
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

/// An explicit pipeline wins, then a default one (`Layers::default_pipeline`);
/// `None` when the enqueue asks for a review level instead, or nothing names one.
fn pipeline_name(
    ctx: &DaemonContext,
    env: &RepoEnv,
    pipeline: Option<String>,
    patch: &AutonomyPatch,
) -> Option<String> {
    if pipeline.is_some() || patch.review.is_some() {
        return pipeline;
    }
    env.default_pipeline(ctx)
}

/// `patch` on [`DEFAULT_PIPELINE`]: an enqueue that names neither a pipeline
/// nor a review level gets the default review, not the pipeline's cheapest.
fn defaulted_review(patch: &AutonomyPatch, policy: &AutonomyPolicy) -> AutonomyPatch {
    AutonomyPatch {
        review: Some(patch.review.unwrap_or(policy.default.review)),
        ..*patch
    }
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
             one that takes a review level (`adaptive`, `direct`) instead, or leave review out",
            base.name(),
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
    refuse_shipping_only(base, patch)?;
    Ok(Autonomy {
        review,
        proposal: ProposalMode::Off,
        report: ReportMode::Off,
        merge: MergeMode::Manual,
        tools: patch.tools.unwrap_or(policy.default.tools),
    })
}

/// A pipeline that does not ship takes no proposal, report or merge.
fn refuse_shipping_only(base: &PipelineSpec, patch: &AutonomyPatch) -> Result<(), ArchonError> {
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
    match shipping_only.into_iter().flatten().next() {
        Some(knob) if !ships(base) => Err(ArchonError::Invalid(format!(
            "{knob} applies only to pipelines that ship; {:?} ships nothing",
            base.name()
        ))),
        _ => Ok(()),
    }
}

/// `current` with `patch` over it, for an item that exists already: the
/// review level only where the pipeline's stage holds it.
pub(crate) fn revise(
    base: &PipelineSpec,
    current: Autonomy,
    patch: &AutonomyPatch,
    policy: &AutonomyPolicy,
) -> Result<Autonomy, ArchonError> {
    refuse_shipping_only(base, patch)?;
    let review = match patch.review {
        Some(_) => review_of(base, patch, policy)?,
        None => current.review,
    };
    let revised = Autonomy {
        review,
        ..patch.over(current)
    };
    assert!(
        ships(base)
            || (revised.proposal, revised.report, revised.merge)
                == (current.proposal, current.report, current.merge),
        "a pipeline that ships nothing keeps its shipping knobs"
    );
    Ok(revised)
}

/// Refuse the knobs `after` changed from `before` that sit above the
/// ceiling; a knob already above it (the ceiling was lowered) may stay.
pub(crate) fn check_raised(
    policy: &AutonomyPolicy,
    env: &RepoEnv,
    base: &PipelineSpec,
    before: Autonomy,
    after: Autonomy,
) -> Result<(), ArchonError> {
    let kept = before.narrowed(policy.ceiling);
    let changed = AutonomyPatch {
        review: (after.review != before.review).then_some(after.review),
        proposal: (after.proposal != before.proposal).then_some(after.proposal),
        report: (after.report != before.report).then_some(after.report),
        merge: (after.merge != before.merge).then_some(after.merge),
        tools: (after.tools != before.tools).then_some(after.tools),
    };
    check_ceiling(policy, env, base, changed.over(kept))
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

/// What a tier resolves against: Aether's catalog within the item's model
/// policy, on the provider the item's route picks, else the orchestrator's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TierScope<'a> {
    pub catalog: &'a ModelCatalog,
    pub policy: &'a ModelPolicy,
    pub prefer: &'a str,
    /// Set while `[models] provider_weights` routes tier-only choices.
    pub route: Option<ProviderRoute<'a>>,
}

impl<'a> TierScope<'a> {
    pub fn new(config: &'a Config, policy: &'a ModelPolicy, catalog: &'a ModelCatalog) -> Self {
        Self {
            catalog,
            policy,
            prefer: &config.orchestrator.model.provider,
            route: None,
        }
    }

    /// This scope with tier-only choices routed by `route`, when there is one.
    pub fn routed(self, route: Option<ProviderRoute<'a>>) -> Self {
        Self { route, ..self }
    }

    /// `tier` on `provider` when given; else on the routed provider that
    /// meets `floor`, or the orchestrator's provider when weights are off.
    pub fn resolve(
        &self,
        (tier, floor): (Tier, Tier),
        provider: Option<&str>,
    ) -> Result<(ModelRef, TierChoice), ArchonError> {
        assert!(floor <= tier, "a floor never exceeds the tier asked for");
        let routed = match (provider, self.route) {
            (None, Some(route)) => Some(self.weighted_provider(tier, floor, route)?),
            _ => None,
        };
        let resolved = self.catalog.resolve(
            tier,
            provider,
            Some(routed.unwrap_or(self.prefer)),
            self.policy,
        )?;
        debug_assert!(
            routed.is_none_or(|p| resolved.0.provider == p && resolved.1.resolved >= floor),
            "a routed choice stays on its provider, at or above the floor"
        );
        Ok(resolved)
    }

    /// The weighted provider whose best allowed model reaches `floor`;
    /// refused when none does, so a weight of 0 or the floor is never bypassed.
    fn weighted_provider(
        &self,
        tier: Tier,
        floor: Tier,
        route: ProviderRoute<'a>,
    ) -> Result<&'a str, ArchonError> {
        if let Some(provider) = self
            .catalog
            .routed_provider((tier, floor), route, self.policy)
        {
            return Ok(provider);
        }
        let best = self.catalog.best_tiers(tier, self.policy);
        let offered: Vec<String> = best
            .iter()
            .map(|(p, t)| format!("{p}: {t} (weight {})", route.weights.weight(p)))
            .collect();
        debug_assert!(
            best.iter()
                .all(|(p, t)| *t < floor || route.weights.weight(p) == 0)
        );
        Err(ArchonError::Invalid(format!(
            "no provider with a positive weight in [models] provider_weights has an allowed \
             model at or above tier {floor} (asked for {tier}); best allowed per provider: {}",
            if offered.is_empty() {
                "none".into()
            } else {
                offered.join("; ")
            }
        )))
    }
}

/// The item a plan's tier-only choices are routed for, and the provider it
/// holds already, if any.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemRoute<'a> {
    pub id: &'a QueueItemId,
    pub held: Option<&'a str>,
}

impl<'a> ItemRoute<'a> {
    pub fn new(item: &'a QueueItem) -> Self {
        Self {
            id: &item.id,
            held: item.routed_provider.as_deref(),
        }
    }

    /// How a [`TierScope`] routes for this item under `weights`, if any.
    pub fn by(self, weights: Option<&'a ProviderWeights>) -> Option<ProviderRoute<'a>> {
        Some(ProviderRoute {
            weights: weights?,
            key: self.id.as_str(),
            held: self.held,
        })
    }
}

/// The provider `item` holds under `weights`: the one it held while the
/// policy still allows a tiered model there, else its weighted pick. With
/// no weights or no catalog read yet, what it held stands.
pub(crate) fn item_provider(
    weights: Option<&ProviderWeights>,
    catalog: &ModelCatalog,
    policy: &ModelPolicy,
    item: ItemRoute<'_>,
) -> Option<String> {
    let held = item.held.map(str::to_owned);
    let Some(route) = item.by(weights) else {
        return held;
    };
    if catalog.is_empty() {
        return held;
    }
    let picked = catalog.item_provider(route, policy).map(str::to_owned);
    debug_assert!(
        picked
            .as_deref()
            .is_none_or(|p| catalog.models().iter().any(|m| m.model.provider == p)),
        "an item is routed to a provider of the catalog"
    );
    picked.or(held)
}

/// A stage's choice when nothing names one, at the pipeline tier's effort
/// within the policy's cap: a model of the pipeline's tier (at most
/// `max_tier`); else the orchestrator's model; else one of the balanced
/// tier, or the policy's first. Pins above `max_tier` are skipped; an
/// untiered model (no catalog tier) still qualifies. `None` if none.
pub(crate) fn default_choice(
    config: &Config,
    scope: TierScope<'_>,
    max_tier: Tier,
    spec: &PipelineSpec,
) -> Option<ModelChoice> {
    let policy = scope.policy;
    let nudged = spec.file.nudges.tier;
    let suggested = match nudged {
        Some(Tier::Fast) => Effort::Low,
        Some(Tier::Balanced) | None => Effort::Medium,
        Some(Tier::Strong | Tier::Frontier) => Effort::High,
    };
    let effort = policy
        .max_effort
        .map_or(suggested, |cap| suggested.min(cap));
    let tiered = |tier: Tier| {
        let tier = tier.min(max_tier);
        let (model, tier) = scope.resolve((tier, tier), None).ok()?;
        Some(ModelChoice {
            model,
            effort,
            tier: Some(tier),
        })
    };
    let pinnable = |m: &ModelRef| {
        policy.permits_model(m) && scope.catalog.tier_of(m).is_none_or(|t| t <= max_tier)
    };
    let own = &config.orchestrator.model;
    let choice = nudged
        .and_then(tiered)
        .or_else(|| pinnable(own).then(|| ModelChoice::pinned(own.clone(), effort)))
        .or_else(|| tiered(Tier::Balanced))
        .or_else(|| {
            let first = policy.allow.iter().flatten().find(|m| pinnable(m))?;
            Some(ModelChoice::pinned(first.clone(), effort))
        })?;
    debug_assert!(
        policy.check(&choice).is_ok(),
        "a default is within the policy"
    );
    debug_assert!(
        scope
            .catalog
            .tier_of(&choice.model)
            .is_none_or(|t| t <= max_tier),
        "a default is within the tier ceiling"
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
    (policy, max_tier): (&ModelPolicy, Tier),
    fallback: Option<&ModelChoice>,
) -> Result<ModelPlan, ArchonError> {
    let learned = learned::default_plan(store, repo, spec, policy, max_tier)?;
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

/// A request's plan with each missing effort set to its stage's default and
/// each tier resolved in `tiers`. An effort above that default needs a
/// `reasons` entry for the stage.
pub(crate) fn resolve_plan(
    spec: &PipelineSpec,
    draft: ModelPlanDraft,
    reasons: Option<&ModelReasons>,
    tiers: TierScope<'_>,
) -> Result<ModelPlan, ArchonError> {
    let plan = draft.resolve(
        |key| spec.default_effort(key),
        |key, tier, provider| tiers.resolve((tier, spec.tier_floor(key, tier)), provider),
    )?;
    for (key, choices) in &plan.0 {
        let top = choices.iter().map(|c| c.effort).max();
        if let (Some(top), Some(default)) = (top, spec.default_effort(key)) {
            check_effort_reason(key, top, default, reasons)?;
        }
    }
    Ok(plan)
}

/// Effort `effort` for plan key `key`, whose default is `default`, is
/// refused unless `reasons` says why when it is higher.
pub(crate) fn check_effort_reason(
    key: &str,
    effort: Effort,
    default: Effort,
    reasons: Option<&ModelReasons>,
) -> Result<(), ArchonError> {
    let reasoned = reasons
        .and_then(|reasons| reasons.get(key))
        .is_some_and(|reason| !reason.trim().is_empty());
    if effort <= default || reasoned {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "effort {effort} for {key:?} is above its default {default}: add a model_reasons entry for \
         {key:?} saying why (a stronger tier at {default} often beats a weaker one at {effort}), \
         or use {default} or lower"
    )))
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
        let strict = builtin("refactor");
        let chosen = knobs(&strict, &AutonomyPatch::default(), &policy()).unwrap();
        assert_eq!(chosen.review, ReviewLevel::Strict);
        let conflict = AutonomyPatch {
            review: Some(ReviewLevel::None),
            ..AutonomyPatch::default()
        };
        let err = knobs(&strict, &conflict, &policy())
            .unwrap_err()
            .to_string();
        assert!(err.contains("(`adaptive`, `direct`)"), "{err}");
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
        let base = builtin("refactor");
        let knobs = Autonomy {
            proposal: ProposalMode::Lavish,
            report: ReportMode::WithPr,
            ..Autonomy::default()
        };
        let spec = augment(&base, &knobs).unwrap();
        let choice = archon_core::model::ModelChoice {
            model: "claude-code/claude-opus-5-5".parse().unwrap(),
            effort: archon_core::model::Effort::High,
            tier: None,
        };
        let mut plan = ModelPlan::default();
        for stage in &base.file.stages {
            plan.0.insert(stage.id.clone(), vec![choice.clone()]);
        }
        let store = Store::open_in_memory().unwrap();
        let policy = ModelPolicy::default();
        let filled = fill_plan(
            &store,
            None,
            (&base, &spec),
            plan.clone(),
            (&policy, Tier::Strong),
            None,
        )
        .unwrap();
        assert_eq!(filled.choices_for("propose"), Some(&[choice.clone()][..]));
        assert_eq!(filled.choices_for("report"), Some(&[choice][..]));
        assert_eq!(filled.0.len(), plan.0.len() + 2);
    }

    /// An enqueue may leave the whole plan out: every stage gets the default.
    #[test]
    fn stages_an_enqueue_leaves_out_get_the_default_choice() {
        let base = builtin("refactor");
        let knobs = Autonomy {
            report: ReportMode::WithPr,
            ..Autonomy::default()
        };
        let spec = augment(&base, &knobs).unwrap();
        let config = Config::default();
        let (open, none) = (ModelPolicy::default(), ModelCatalog::default());
        let tiers = TierScope::new(&config, &open, &none);
        let default = default_choice(&config, tiers, Tier::Strong, &spec).unwrap();
        assert_eq!(default.model, config.orchestrator.model);
        let named = archon_core::model::ModelChoice {
            model: "codex/gpt-5.5".parse().unwrap(),
            effort: Effort::High,
            tier: None,
        };
        let mut plan = ModelPlan::default();
        plan.0.insert("review".into(), vec![named.clone()]);
        let store = Store::open_in_memory().unwrap();
        let policy = ModelPolicy::default();
        let filled = fill_plan(
            &store,
            None,
            (&base, &spec),
            plan,
            (&policy, Tier::Strong),
            Some(&default),
        )
        .unwrap();
        assert_eq!(filled.choices_for("review"), Some(&[named][..]), "kept");
        for stage in ["baseline", "refactor", "verify", "report"] {
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
        let strict = builtin("performance");
        let none = ModelCatalog::default();
        let pick = |policy: &ModelPolicy, spec| {
            default_choice(
                &config,
                TierScope::new(&config, policy, &none),
                Tier::Strong,
                spec,
            )
        };
        let open = ModelPolicy::default();
        let effort = |spec| pick(&open, spec).unwrap().effort;
        assert!(
            effort(&direct) < effort(&strict),
            "a fast pipeline thinks less"
        );
        let capped = ModelPolicy::from_lists(&[], &[], Some(Effort::Low));
        let low = pick(&capped, &strict).unwrap();
        assert_eq!(low.effort, Effort::Low, "never above the policy's cap");
        let elsewhere = ModelPolicy::from_lists(std::slice::from_ref(&other), &[], None);
        let picked = pick(&elsewhere, &strict).unwrap();
        assert_eq!(
            picked.model, other,
            "the orchestrator's model is not allowed"
        );
        let nothing = ModelPolicy::from_lists(&[], &[own], None);
        assert_eq!(pick(&nothing, &strict), None);
    }

    fn tiered_catalog() -> ModelCatalog {
        use archon_core::model_tier::CatalogModel;
        let entry = |model: &str, tier| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(tier),
            is_default: false,
        };
        ModelCatalog::new(vec![
            entry("claude-code/one-fast", Tier::Fast),
            entry("claude-code/one-balanced", Tier::Balanced),
            entry("claude-code/one-strong", Tier::Strong),
            entry("claude-code/one-frontier", Tier::Frontier),
            entry("codex/two-strong", Tier::Strong),
        ])
    }

    /// The pipeline's tier picks a model of that tier, never above the
    /// default ceiling; frontier only when the operator raises it.
    #[test]
    fn the_nudged_tier_selects_a_model_of_that_tier_within_the_ceiling() {
        let mut config = Config::default();
        config.orchestrator.model = "claude-code/own".parse().unwrap();
        let catalog = tiered_catalog();
        let open = ModelPolicy::default();
        let tiers = TierScope::new(&config, &open, &catalog);
        let mut spec = builtin("performance");
        let strong = default_choice(&config, tiers, Tier::Strong, &spec).unwrap();
        assert_eq!(strong.model.to_string(), "claude-code/one-strong");
        assert_eq!(strong.tier.as_ref().unwrap().requested, Tier::Strong);
        spec.file.nudges.tier = Some(Tier::Frontier);
        let capped = default_choice(&config, tiers, Tier::Strong, &spec).unwrap();
        assert_eq!(
            capped.model.to_string(),
            "claude-code/one-strong",
            "never frontier on its own"
        );
        let raised = default_choice(&config, tiers, Tier::Frontier, &spec).unwrap();
        assert_eq!(raised.model.to_string(), "claude-code/one-frontier");
        let balanced = default_choice(&config, tiers, Tier::Strong, &builtin("direct")).unwrap();
        assert_eq!(balanced.model.to_string(), "claude-code/one-balanced");
        spec.file.nudges.tier = None;
        let own = default_choice(&config, tiers, Tier::Strong, &spec).unwrap();
        assert_eq!(
            (own.model, own.tier),
            (config.orchestrator.model.clone(), None)
        );
    }

    /// The pinned fallbacks keep to the ceiling too: an orchestrator or
    /// allowlisted model the catalog places above it is skipped.
    #[test]
    fn a_pinned_fallback_above_the_ceiling_is_not_a_default() {
        use archon_core::model_tier::CatalogModel;
        let mut config = Config::default();
        config.orchestrator.model = "p/top".parse().unwrap();
        let entry = |model: &str, tier| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(tier),
            is_default: false,
        };
        let only_frontier = ModelCatalog::new(vec![entry("p/top", Tier::Frontier)]);
        let open = ModelPolicy::default();
        let spec = builtin("direct");
        let tiers = TierScope::new(&config, &open, &only_frontier);
        assert_eq!(default_choice(&config, tiers, Tier::Strong, &spec), None);
        let raised = default_choice(&config, tiers, Tier::Frontier, &spec).unwrap();
        assert_eq!(raised.model.to_string(), "p/top", "the operator raised it");

        let strong = ModelCatalog::new(vec![entry("p/top", Tier::Strong)]);
        let allowed: Vec<ModelRef> = vec!["p/top".parse().unwrap(), "q/untiered".parse().unwrap()];
        let listed = ModelPolicy::from_lists(&allowed, &[], None);
        let tiers = TierScope::new(&config, &listed, &strong);
        let lowered = default_choice(&config, tiers, Tier::Balanced, &spec).unwrap();
        assert_eq!(
            (lowered.model.to_string(), lowered.tier),
            ("q/untiered".to_owned(), None),
            "a repository-lowered ceiling skips the strong pins"
        );
    }

    #[test]
    fn a_tiered_plan_entry_resolves_and_a_pin_is_kept() {
        let config = Config::default();
        let catalog = tiered_catalog();
        let open = ModelPolicy::default();
        let tiers = TierScope::new(&config, &open, &catalog);
        let spec = builtin("direct");
        let draft: ModelPlanDraft = serde_json::from_value(serde_json::json!({
            "implement": [{"tier": "frontier", "provider": "codex", "effort": "low"}],
        }))
        .unwrap();
        let plan = resolve_plan(&spec, draft, None, tiers).unwrap();
        let choice = &plan.choices_for("implement").unwrap()[0];
        assert_eq!(choice.model.to_string(), "codex/two-strong");
        assert!(choice.tier.as_ref().unwrap().fell_back(), "{choice:?}");
        let draft: ModelPlanDraft = serde_json::from_value(serde_json::json!({
            "implement": [{"tier": "fast", "provider": "codex"}],
        }))
        .unwrap();
        let error = resolve_plan(&spec, draft, None, tiers)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "available tiers: claude-code: fast, balanced, strong, frontier; codex: strong"
            ),
            "{error}"
        );
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
