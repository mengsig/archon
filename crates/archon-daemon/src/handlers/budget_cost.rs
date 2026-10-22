//! Refuse at enqueue a plan whose delegated reviewers cannot run under the item's
//! dollar budget, with the reason the run would otherwise fail with. Stage models
//! stay checked at spawn, where Aether's own provider metadata decides.

use archon_core::autonomy::ReviewLevel;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::ModelPlan;
use archon_core::repo::Repo;
use archon_pipeline::PipelineSpec;
use archon_pipeline::contract;

use crate::context::DaemonContext;
use crate::resolve::{self, RepoEnv};

/// The item a plan is for, as far as its budget is concerned.
pub(crate) struct Budgeted<'a> {
    pub budget_usd: Option<f64>,
    pub review: ReviewLevel,
    pub repo: Option<&'a Repo>,
}

/// Whether a dollar cap bounds the item: its own budget or the per-item cap
/// of its repository, while budgets are on.
fn bounded(ctx: &DaemonContext, item: &Budgeted<'_>) -> bool {
    let caps = resolve::item_caps(ctx, item.repo.map(|repo| &repo.id));
    let budget = item.budget_usd.unwrap_or(caps.budget_usd_per_item);
    assert!(budget.is_finite() && budget >= 0.0, "budgets are validated");
    caps.budgets_enabled && budget > 0.0
}

/// `plan` can run under `item`'s budget: its delegated reviewers, when the
/// review is owed, report a dollar cost.
pub(crate) fn check_budget_models(
    ctx: &DaemonContext,
    env: &RepoEnv,
    id: &QueueItemId,
    spec: &PipelineSpec,
    (plan, item): (&ModelPlan, &Budgeted<'_>),
) -> Result<(), ArchonError> {
    if !bounded(ctx, item) {
        return Ok(());
    }
    if item.review != ReviewLevel::None
        && let Some(key) = spec.reviewers_key()
        && let Some(named) = plan.choices_for(&key).filter(|c| !c.is_empty())
    {
        let catalog = archon_core::model_tier::ModelCatalog::new(Vec::new());
        let why = contract::governed_review_refusal(
            Some(named),
            &named[0].model,
            &env.policy(&ctx.config),
            (&ctx.config.review, &catalog),
        );
        if let Some(why) = why {
            return Err(ArchonError::Invalid(format!(
                "item {id} has review {} and a budget: {why}",
                item.review
            )));
        }
    }
    Ok(())
}
