use crate::context::DaemonContext;
use archon_core::ArchonError;
use archon_core::model::ModelRef;
use archon_core::model_tier::{CatalogModel, ModelCatalog};

pub(super) async fn refresh(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let _guard = ctx.aether.lock_catalog().await;
    let generation = ctx.aether.generation()?;
    let store = ctx.store.lock().await;
    let fresh = !store.learning_models_stale() && store.catalog_generation() == Some(generation);
    drop(store);
    if fresh && ctx.aether.is_connected() {
        return Ok(());
    }
    read_and_cache(ctx, generation).await.map(drop)
}

fn reference(value: serde_json::Value, provider: &str) -> Result<ModelRef, ArchonError> {
    if let Some(id) = value.as_str().filter(|id| !id.contains('/')) {
        return format!("{provider}/{id}").parse();
    }
    serde_json::from_value(value)
        .map_err(|e| ArchonError::Protocol(format!("invalid catalog model reference: {e}")))
}

pub(crate) async fn request_catalog(ctx: &DaemonContext) -> Result<ModelCatalog, ArchonError> {
    let _guard = ctx.aether.lock_catalog().await;
    let generation = ctx.aether.generation()?;
    match read_and_cache(ctx, generation).await {
        Ok(catalog) => Ok(catalog),
        Err(error) if error.is_store() => Err(error),
        Err(error) => Ok(ModelCatalog::unavailable(error.to_string())),
    }
}

async fn read_and_cache(ctx: &DaemonContext, generation: u64) -> Result<ModelCatalog, ArchonError> {
    let result = read_catalog(ctx, generation).await;
    if let Err(error) = &result {
        ctx.store
            .lock()
            .await
            .invalidate_model_catalog(error.to_string());
    }
    result
}

async fn read_catalog(ctx: &DaemonContext, generation: u64) -> Result<ModelCatalog, ArchonError> {
    let client = ctx.aether.client()?;
    let models = client.list_models().await?;
    let retired = if client
        .welcome()
        .features
        .iter()
        .any(|f| f == "retired_models")
    {
        client.list_retired_models().await?
    } else {
        Vec::new()
    };
    let vision: Vec<_> = models
        .iter()
        .filter(|m| m.configured && m.capabilities.vision)
        .map(|m| m.model.clone())
        .collect();
    let catalog = make_catalog(models, retired)?;
    let mut store = ctx.store.lock().await;
    if ctx.aether.generation()? != generation || !ctx.aether.is_connected() {
        return Err(ArchonError::Unavailable(
            "Aether connection changed during catalog read".into(),
        ));
    }
    store.kv_set("acceptance_vision_models", &serde_json::to_string(&vision)?)?;
    store.set_model_catalog(catalog.clone());
    store.set_catalog_generation(generation);
    Ok(catalog)
}

fn make_catalog(
    models: Vec<archon_aether::wire::ModelInfo>,
    retired: Vec<archon_aether::wire::RetiredModel>,
) -> Result<ModelCatalog, ArchonError> {
    let ready: Vec<_> = models.into_iter().filter(|m| m.configured).collect();
    let mut catalog = ModelCatalog::new(
        ready
            .iter()
            .map(|m| CatalogModel {
                model: m.model.clone(),
                tier: m.tier,
                is_default: m.is_default,
            })
            .collect(),
    );
    for model in ready {
        for alias in model.aliases {
            let alias = reference(serde_json::Value::String(alias), &model.model.provider)?;
            catalog.add_alias(alias, model.model.clone());
        }
        if let Some(value) = model.superseded_by {
            let successor = reference(value, &model.model.provider)?;
            catalog.add_superseded(model.model, successor);
        }
    }
    for model in retired {
        let successor = model
            .successor
            .map(|v| reference(v, &model.model.provider))
            .transpose()?;
        catalog.add_retired(model.model, successor);
    }
    Ok(catalog)
}

pub(crate) async fn catalog(ctx: &DaemonContext) -> Result<ModelCatalog, ArchonError> {
    let _guard = ctx.aether.lock_catalog().await;
    let generation = ctx.aether.generation()?;
    let store = ctx.store.lock().await;
    let fresh = !store.learning_models_stale() && store.catalog_generation() == Some(generation);
    if fresh && ctx.aether.is_connected() {
        return Ok(store.model_catalog());
    }
    drop(store);
    match read_and_cache(ctx, generation).await {
        Ok(catalog) => Ok(catalog),
        Err(error) if error.is_store() => Err(error),
        Err(error) => Ok(ModelCatalog::unavailable(error.to_string())),
    }
}
