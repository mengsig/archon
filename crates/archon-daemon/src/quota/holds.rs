//! Which queued items and pending spawns wait for quota: some tier-only stage
//! has every provider meeting its floor out. Drawn for each admission snapshot.

use archon_core::model_tier::ModelCatalog;
use archon_core::queue::QueueItem;
use archon_core::quota::{QuotaHold, QuotaView};
use archon_pipeline::autonomy::augment;
use archon_pipeline::{Catalog, PipelineSpec, SpawnPlan};

use crate::context::DaemonContext;
use crate::knobs::{self, ItemRoute};
use crate::resolve::RepoEnv;

/// `item`'s hold, when weights route it in `env` and one of its tier-only
/// choices has no provider with quota at its floor in `pipelines`.
pub(crate) fn item_hold(
    ctx: &DaemonContext,
    (env, pipelines): (&RepoEnv, &Catalog),
    item: &QueueItem,
    (catalog, view): (&ModelCatalog, &QuotaView),
) -> Option<QuotaHold> {
    assert_eq!(item.state.kind(), "queued", "only queued items are held");
    assert_eq!(
        item.repo.as_ref(),
        env.repo.as_ref().map(|r| &r.id),
        "the item's own repository config"
    );
    let weights = env.provider_weights(&ctx.config)?;
    if catalog.is_empty() || !view.any_exhausted() {
        return None;
    }
    let base = pipelines.get(&item.pipeline).ok()?;
    let spec = augment(base, &item.autonomy).ok()?;
    let policy = env.policy(&ctx.config);
    let route = ItemRoute::new(item).by(Some(&weights), Some(view))?;
    let hold = knobs::quota_hold(&item.model_plan, &spec, (catalog, &policy), route);
    if let Some(hold) = &hold {
        tracing::debug!(item = %item.id, stage = %hold.stage, until = ?hold.until, "held for provider quota");
    }
    hold
}

/// The hold of `item`'s pending `plans` in `spec`, when weights route it in
/// `env` and one of them waits for quota.
pub(crate) fn spawn_hold(
    ctx: &DaemonContext,
    env: &RepoEnv,
    (item, spec, plans): (&QueueItem, &PipelineSpec, &[SpawnPlan]),
    (catalog, view): (&ModelCatalog, &QuotaView),
) -> Option<QuotaHold> {
    assert_eq!(
        item.repo.as_ref(),
        env.repo.as_ref().map(|r| &r.id),
        "the item's own repository config"
    );
    assert!(
        (plans.iter()).all(|p| spec.file.stages.iter().any(|s| s.id == p.stage_id)),
        "the plans are stages of the run's pipeline"
    );
    let weights = env.provider_weights(&ctx.config)?;
    if plans.is_empty() || catalog.is_empty() || !view.any_exhausted() {
        return None;
    }
    let policy = env.policy(&ctx.config);
    let read = (catalog, view);
    super::reroute::first_hold(item, (spec, plans), (&weights, &policy), read)
}
