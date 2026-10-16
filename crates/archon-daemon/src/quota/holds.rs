//! Which queued items wait for quota: every provider that one of their
//! tier-only stages may use at its floor is out of it. Drawn afresh each
//! dispatch pass, so an item starts again once a window resets.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model_tier::ModelCatalog;
use archon_core::protocol::ItemQuery;
use archon_core::queue::QueueItem;
use archon_core::quota::{QuotaHold, QuotaView};
use archon_pipeline::Catalog;
use archon_pipeline::autonomy::augment;

use crate::context::DaemonContext;
use crate::knobs::{self, ItemRoute};
use crate::resolve::RepoEnv;

/// A repository's config and pipelines; `None` when they do not load, and
/// its items are then held for nothing here: their start reports why.
type Pipelines = Option<(RepoEnv, Catalog)>;

/// Recompute the quota holds of every queued item. With no provider out of
/// quota, as with an aetherd that reports none, nothing is held.
pub(crate) async fn refresh_holds(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let Some(view) = super::view(ctx).filter(QuotaView::any_exhausted) else {
        ctx.quota.set_holds(BTreeMap::new());
        return Ok(());
    };
    let (catalog, queued) = {
        let store = ctx.store.lock().await;
        let query = ItemQuery {
            states: vec!["queued".into()],
            ..ItemQuery::default()
        };
        (store.model_catalog(), store.items(&query)?)
    };
    let mut repos: BTreeMap<Option<RepoId>, Pipelines> = BTreeMap::new();
    let mut holds = BTreeMap::new();
    for item in &queued {
        if !repos.contains_key(&item.repo) {
            let loaded = pipelines(ctx, item.repo.as_ref()).await?;
            repos.insert(item.repo.clone(), loaded);
        }
        let Some(pipelines) = &repos[&item.repo] else {
            continue;
        };
        if let Some(hold) = item_hold(ctx, pipelines, item, (&catalog, &view)) {
            tracing::debug!(item = %item.id, stage = %hold.stage, until = ?hold.until, "held for provider quota");
            holds.insert(item.id.clone(), hold);
        }
    }
    debug_assert!(holds.keys().all(|id| queued.iter().any(|i| &i.id == id)));
    ctx.quota.set_holds(holds);
    Ok(())
}

/// `repo`'s config and pipeline catalog. Only a store error fails the pass.
async fn pipelines(ctx: &DaemonContext, repo: Option<&RepoId>) -> Result<Pipelines, ArchonError> {
    let loaded = RepoEnv::load(ctx, repo)
        .await
        .and_then(|env| env.catalog().map(|catalog| (env, catalog)));
    match loaded {
        Ok(loaded) => Ok(Some(loaded)),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::debug!(?repo, %error, "pipelines do not load; no quota hold drawn");
            Ok(None)
        }
    }
}

/// `item`'s hold, when provider weights route it and one of its tier-only
/// choices has no provider with quota at its floor.
fn item_hold(
    ctx: &DaemonContext,
    (env, pipelines): &(RepoEnv, Catalog),
    item: &QueueItem,
    (catalog, view): (&ModelCatalog, &QuotaView),
) -> Option<QuotaHold> {
    assert_eq!(item.state.kind(), "queued", "only queued items are held");
    let weights = env.provider_weights(&ctx.config)?;
    if catalog.is_empty() {
        return None;
    }
    let base = pipelines.get(&item.pipeline).ok()?;
    let spec = augment(base, &item.autonomy).ok()?;
    let policy = env.policy(&ctx.config);
    let route = ItemRoute::new(item).by(Some(&weights), Some(view))?;
    knobs::quota_hold(&item.model_plan, &spec, (catalog, &policy), route)
}
