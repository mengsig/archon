use crate::context::DaemonContext;
use archon_core::ArchonError;

pub(super) async fn refresh(ctx: &DaemonContext) -> Result<(), ArchonError> {
    if !ctx.aether.is_connected() || !ctx.store.lock().await.learning_models_stale() {
        return Ok(());
    }
    let models = ctx.aether.client()?.list_models().await?;
    let ready = models
        .into_iter()
        .filter(|m| m.configured)
        .map(|m| m.model)
        .collect();
    ctx.store.lock().await.set_learning_models(ready);
    Ok(())
}
