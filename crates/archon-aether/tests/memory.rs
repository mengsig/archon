//! Curating one named memory against the fake daemon: the exact commands
//! sent and the checks on Aether's answers.

mod common;

use archon_aether::fake::{self, Scripted};
use archon_core::aether_ids::AetherMemoryId;
use archon_core::error::ArchonError;
use common::{connect, fake_aether};
use serde_json::json;

const STALE: &str = "mem_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "mem_01BX5ZZKBKACTAV9WEVGEMMVRZ";

fn id(raw: &str) -> AetherMemoryId {
    AetherMemoryId::parse(raw).unwrap()
}

#[tokio::test]
async fn one_named_memory_is_read_forgotten_and_restored_by_id() {
    let (_dir, fake) = fake_aether().await;
    let stale = fake::memory(STALE, "demo", "Commit as the work address");
    let mut forgotten = stale.clone();
    forgotten["forgotten_at"] = json!("2026-10-01T09:00:00Z");
    fake.script(
        "memories",
        Scripted::reply("memories", json!([stale.clone()])),
    );
    fake.script("forget_memory", Scripted::reply("memory", forgotten));
    fake.script("restore_memory", Scripted::reply("memory", stale));
    let client = connect(&fake).await;

    let record = client
        .memory(&id(STALE))
        .await
        .unwrap()
        .expect("Aether has it");
    let view = record.view();
    assert_eq!(
        (view.origin.as_str(), view.scope.as_str()),
        ("agent agt_01ARZ3NDEKTSV4RRFFQ69G5FAV", "repository demo")
    );
    assert!(
        !format!("{view:?}").contains("work address"),
        "the text is never kept"
    );
    let forgotten = client
        .forget_memory(&id(STALE), "archon fb_1: stale")
        .await
        .unwrap();
    assert!(forgotten.forgotten_at.is_some());
    assert!(
        client
            .restore_memory(&id(STALE))
            .await
            .unwrap()
            .forgotten_at
            .is_none()
    );

    let query = &fake.received_named("memories")[0]["query"];
    assert_eq!(
        query,
        &json!({"id": STALE, "include_forgotten": true, "include_superseded": true, "limit": 1})
    );
    let forget = &fake.received_named("forget_memory")[0];
    assert_eq!(
        (forget["id"].as_str(), forget["reason"].as_str()),
        (Some(STALE), Some("archon fb_1: stale"))
    );
    assert_eq!(fake.received_named("restore_memory")[0]["id"], STALE);
}

#[tokio::test]
async fn answers_about_another_memory_or_without_the_change_are_refused() {
    let (_dir, fake) = fake_aether().await;
    let other = fake::memory(OTHER, "demo", "unrelated");
    fake.script(
        "memories",
        Scripted::reply("memories", json!([other.clone()])),
    );
    fake.script("memories", Scripted::reply("memories", json!([])));
    fake.script(
        "forget_memory",
        Scripted::reply("memory", fake::memory(STALE, "demo", "x")),
    );
    fake.script(
        "forget_memory",
        Scripted::refuse("not_found", "no memory mem_…"),
    );
    let client = connect(&fake).await;
    let error = client.memory(&id(STALE)).await.unwrap_err();
    assert!(matches!(error, ArchonError::Protocol(_)), "{error:?}");
    assert!(
        client.memory(&id(STALE)).await.unwrap().is_none(),
        "absent is not an error"
    );
    let error = client.forget_memory(&id(STALE), "why").await.unwrap_err();
    assert!(
        error.to_string().contains("without forgetting it"),
        "{error}"
    );
    assert!(
        client.forget_memory(&id(STALE), "why").await.is_err(),
        "refusals surface"
    );
}
