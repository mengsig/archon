mod common;

use archon_aether::fake::Scripted;
use archon_core::ArchonError;
use common::{connect, fake_aether};
use serde_json::json;

#[tokio::test]
async fn storage_gc_is_never_sent_to_a_daemon_without_the_storage_feature() {
    for supported in [false, true] {
        let (_dir, fake) = fake_aether().await;
        fake.set_features(if supported {
            vec!["storage".into()]
        } else {
            vec![]
        });
        fake.set_default(
            "storage_gc",
            Scripted::reply("storage_report", json!({"freed": 3})),
        );
        let client = connect(&fake).await;
        assert_eq!(client.welcome().supports_storage(), supported);

        let report = client.storage_gc().await;
        if supported {
            assert_eq!(report.unwrap()["freed"], 3);
            let sent = fake.received_named("storage_gc");
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0]["dry_run"], false);
            assert!(sent[0].get("prune_build_output").is_none());
        } else {
            assert!(matches!(report, Err(ArchonError::Unavailable(_))));
            assert!(fake.received_named("storage_gc").is_empty());
        }
    }
}

#[tokio::test]
async fn a_refused_or_malformed_storage_report_is_an_error() {
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["storage".into()]);
    fake.script(
        "storage_gc",
        Scripted::refuse("sweep_busy", "a sweep is running"),
    );
    fake.script("storage_gc", Scripted::reply("storage_report", json!([1])));
    let client = connect(&fake).await;

    let busy = client.storage_gc().await.unwrap_err();
    assert!(busy.to_string().contains("sweep_busy"), "{busy}");
    assert!(matches!(
        client.storage_gc().await,
        Err(ArchonError::Protocol(_))
    ));
}
