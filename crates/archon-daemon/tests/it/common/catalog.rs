use archon_aether::fake::{FakeAether, Scripted};
use archon_core::model::ModelRef;
use archon_core::model_tier::{CatalogModel, ModelCatalog};
use serde_json::json;

use super::{Harness, MODEL, REVIEW_MODEL};

pub fn default_models() -> ModelCatalog {
    untiered(
        [
            MODEL,
            REVIEW_MODEL,
            "claude-code/claude-opus-5-5",
            "claude-code/claude-haiku-5-5",
            "codex/cheap",
            "codex/expensive",
            "codex/dear",
            "codex/large",
            "codex/small",
            "codex/sol",
            "codex/mini",
            "codex/gpt-6-luna",
        ]
        .map(|id| id.parse().unwrap()),
    )
}

pub fn untiered(models: impl IntoIterator<Item = ModelRef>) -> ModelCatalog {
    ModelCatalog::new(
        models
            .into_iter()
            .map(|model| CatalogModel {
                model,
                tier: None,
                is_default: false,
            })
            .collect(),
    )
}

pub fn answer(fake: &FakeAether, catalog: &ModelCatalog) {
    let rows = catalog
        .models()
        .iter()
        .map(|entry| {
            json!({
                "model": entry.model, "configured": true, "tier": entry.tier,
                "is_default": entry.is_default,
            })
        })
        .collect::<Vec<_>>();
    fake.set_default("list_models", Scripted::reply("models", json!(rows)));
}

impl Harness {
    pub async fn set_catalog(&self, catalog: ModelCatalog) {
        answer(&self.catalog_server, &catalog);
        self.ctx.store.lock().await.set_model_catalog(catalog);
    }

    pub async fn set_learning_models(&self, models: Vec<ModelRef>) {
        self.set_catalog(untiered(models)).await;
    }
}
