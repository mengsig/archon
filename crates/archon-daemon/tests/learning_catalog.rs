mod link_common;

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
