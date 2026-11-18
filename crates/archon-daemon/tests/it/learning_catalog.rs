use crate::link_common;

use archon_aether::fake::Scripted;
use archon_core::notify::NotificationKind;
use link_common::Harness;

#[tokio::test]
async fn a_catalog_outage_does_not_stop_reconciliation_and_reports_once() {
    let h = Harness::new().await;
    h.connect().await;
    h.fake.set_default(
        "list_models",
        Scripted::refuse("internal", "catalog unavailable"),
    );
    for _ in 0..2 {
        archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    }
    assert_eq!(h.kv("learning/backfill/done").await.as_deref(), Some("1"));
    assert_eq!(h.notifications(NotificationKind::Outcome).await.len(), 1);
}

#[tokio::test]
async fn reconnect_and_outage_invalidate_a_fresh_catalog() {
    let h = Harness::new().await;
    h.fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            serde_json::json!([
                {"model": "p/old", "configured": true}
            ]),
        ),
    );
    h.connect().await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    assert_eq!(
        h.ctx.store.lock().await.model_catalog().models()[0]
            .model
            .to_string(),
        "p/old"
    );
    h.fake
        .set_default("list_models", Scripted::refuse("internal", "offline"));
    h.connect().await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    let store = h.ctx.store.lock().await;
    assert!(store.model_catalog().unavailable_reason().is_some());
    assert!(store.learning_models_stale());
    drop(store);
    h.fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            serde_json::json!([
                {"model": "p/new", "configured": true}
            ]),
        ),
    );
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    assert_eq!(
        h.ctx.store.lock().await.model_catalog().models()[0]
            .model
            .to_string(),
        "p/new"
    );
}

#[tokio::test]
async fn a_failed_refresh_cannot_use_a_catalog_older_than_five_minutes() {
    let h = Harness::new().await;
    h.connect().await;
    let old = archon_core::now() - chrono::Duration::minutes(5);
    let generation = h.ctx.aether.generation().unwrap();
    {
        let store = h.ctx.store.lock().await;
        store.record_model_catalog(
            crate::common::catalog::default_models(),
            old,
            Some(generation),
        );
        assert!(store.fresh_model_catalog().is_none());
        assert!(!store.model_catalog().models().is_empty());
    }
    h.fake.set_default(
        "list_models",
        Scripted::refuse("internal", "refresh failed"),
    );
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    assert!(!h.fake.received_named("list_models").is_empty());
    let store = h.ctx.store.lock().await;
    assert!(store.fresh_model_catalog().is_none());
    assert!(
        store
            .model_catalog()
            .unavailable_reason()
            .unwrap()
            .contains("refresh failed")
    );
}
