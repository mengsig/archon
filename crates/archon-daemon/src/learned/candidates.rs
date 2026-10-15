use crate::context::DaemonContext;
use archon_core::ArchonError;
use archon_core::model_tier::{CatalogModel, ModelCatalog};

pub(super) async fn refresh(ctx: &DaemonContext) -> Result<(), ArchonError> {
    if !ctx.aether.is_connected() || !ctx.store.lock().await.learning_models_stale() {
        return Ok(());
    }
    let models = ctx.aether.client()?.list_models().await?;
    let ready = models
        .into_iter()
        .filter(|m| m.configured)
        .map(|m| CatalogModel {
            model: m.model,
            tier: m.tier,
            is_default: m.is_default,
        })
        .collect();
    ctx.store
        .lock()
        .await
        .set_model_catalog(ModelCatalog::new(ready));
    Ok(())
}

/// Aether's connected models with their tiers. A failed refresh keeps the
/// last catalog read, which is empty before the first; tier resolution
/// then refuses, naming the tiers it has.
pub(crate) async fn catalog(ctx: &DaemonContext) -> Result<ModelCatalog, ArchonError> {
    if let Err(error) = refresh(ctx).await {
        if error.is_store() {
            return Err(error);
        }
        tracing::warn!(%error, "model catalog refresh failed; using the last one read");
    }
    Ok(ctx.store.lock().await.model_catalog())
}
