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

fn retired_catalog(h: &Harness) {
    h.fake.set_features(vec!["retired_models".into()]);
    h.fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            serde_json::json!([{"model": "p/new", "configured": true}]),
        ),
    );
    h.fake.set_default(
        "list_retired_models",
        Scripted::reply(
            "retired_models",
            serde_json::json!([
                {"model": "p/old", "successor": "new"},
                {"model": "p/gone"}
            ]),
        ),
    );
}

#[tokio::test]
async fn a_negotiated_retired_list_keeps_a_successor_only_where_aether_names_one() {
    let h = Harness::new().await;
    retired_catalog(&h);
    h.connect().await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    assert_eq!(h.fake.received_named("list_retired_models").len(), 1);
    let store = h.ctx.store.lock().await;
    let catalog = store.model_catalog();
    let (old, gone) = ("p/old".parse().unwrap(), "p/gone".parse().unwrap());
    assert!(catalog.is_retired(&old) && catalog.is_retired(&gone));
    assert_eq!(catalog.successor(&old).unwrap().to_string(), "p/new");
    assert!(catalog.successor(&gone).is_none());
    let hint = catalog.check(&old).unwrap_err().to_string();
    assert!(hint.contains("successor: p/new"), "{hint}");
    let bare = catalog.check(&gone).unwrap_err().to_string();
    assert!(
        bare.contains("retired") && !bare.contains("successor"),
        "{bare}"
    );
}

#[tokio::test]
async fn a_welcome_without_the_retired_feature_never_asks_for_the_list() {
    let h = Harness::new().await;
    retired_catalog(&h);
    h.fake.set_features(Vec::new());
    h.connect().await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    assert!(h.fake.received_named("list_retired_models").is_empty());
    let store = h.ctx.store.lock().await;
    assert!(!store.model_catalog().is_retired(&"p/old".parse().unwrap()));
    assert!(!store.model_catalog().models().is_empty());
}
