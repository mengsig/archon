//! Optional memory feedback wire commands and validation against fake Aether.

mod common;

use std::sync::Arc;

use archon_aether::fake::{self, Scripted};
use archon_aether::memory_feedback::{MemoryEvidence, RecordMemoryFeedback};
use archon_core::aether_ids::{AetherMemoryFeedbackId, AetherMemoryId, AetherTaskId};
use common::{TASK, connect, fake_aether};
use serde_json::json;

const MEMORY: &str = "mem_01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn request() -> RecordMemoryFeedback {
    RecordMemoryFeedback {
        memory: AetherMemoryId::parse(MEMORY).unwrap(),
        verdict: "hurt".into(),
        evidence: MemoryEvidence::Operator,
        task: AetherTaskId::parse(TASK).unwrap(),
        reason: "archon fb_1: wrong advice".into(),
        idempotency_key: "archon:fb_1:memory:task".into(),
    }
}

#[tokio::test]
async fn context_record_and_retraction_use_the_optional_wire_commands() {
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["memory_context".into(), "memory_feedback".into()]);
    fake.set_default(
        "memories_in_context",
        Scripted::reply(
            "memories_in_context",
            fake::memory_context(TASK, &[MEMORY], true),
        ),
    );
    fake.set_responder(
        "record_memory_feedback",
        Arc::new(|cmd| Scripted::reply("memory_feedback", fake::memory_feedback(cmd, "mfb_1"))),
    );
    let client = connect(&fake).await;
    let context = client.memories_in_context(&request().task).await.unwrap();
    assert!(context.truncated);
    assert_eq!(context.snapshots[0].memories[0].as_str(), MEMORY);
    let record = client.record_memory_feedback(request()).await.unwrap();
    assert_eq!(record.id.as_str(), "mfb_1");
    let command = &fake.received_named("record_memory_feedback")[0];
    assert_eq!(command["evidence"], "operator");
    assert_eq!(command["idempotency_key"], request().idempotency_key);
    assert!(command.get("invocation").is_none());
    let mut retracted = fake::memory_feedback(command, "mfb_1");
    retracted["retracted_at"] = json!("2026-10-15T10:00:00Z");
    fake.set_default(
        "retract_memory_feedback",
        Scripted::reply("memory_feedback", retracted),
    );
    assert!(
        client
            .retract_memory_feedback(&record.id, "undo")
            .await
            .unwrap()
            .retracted_at
            .is_some()
    );
    assert_eq!(
        fake.received_named("retract_memory_feedback")[0]["feedback"],
        "mfb_1"
    );
}

#[tokio::test]
async fn missing_features_refuse_without_sending_commands() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    assert!(client.record_memory_feedback(request()).await.is_err());
    assert!(client.memories_in_context(&request().task).await.is_err());
    assert!(
        client
            .retract_memory_feedback(&AetherMemoryFeedbackId::parse("mfb_1").unwrap(), "undo")
            .await
            .is_err()
    );
    for cmd in [
        "record_memory_feedback",
        "memories_in_context",
        "retract_memory_feedback",
    ] {
        assert!(fake.received_named(cmd).is_empty());
    }
}

#[tokio::test]
async fn invalid_reasons_and_keys_are_refused_before_sending() {
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["memory_context".into(), "memory_feedback".into()]);
    let client = connect(&fake).await;
    for reason in [" ".into(), "x".repeat(4097)] {
        assert!(
            client
                .record_memory_feedback(RecordMemoryFeedback {
                    reason,
                    ..request()
                })
                .await
                .is_err()
        );
    }
    for idempotency_key in ["bad key".into(), "x".repeat(201)] {
        assert!(
            client
                .record_memory_feedback(RecordMemoryFeedback {
                    idempotency_key,
                    ..request()
                })
                .await
                .is_err()
        );
    }
    assert!(fake.received_named("record_memory_feedback").is_empty());
}

#[tokio::test]
async fn mismatched_receipts_stay_visible() {
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["memory_context".into(), "memory_feedback".into()]);
    let client = connect(&fake).await;
    fake.set_responder(
        "record_memory_feedback",
        Arc::new(|cmd| {
            let mut record = fake::memory_feedback(cmd, "mfb_1");
            record["task"] = json!("tsk_other");
            Scripted::reply("memory_feedback", record)
        }),
    );
    assert!(
        client
            .record_memory_feedback(request())
            .await
            .unwrap_err()
            .to_string()
            .contains("differs")
    );
    fake.set_default(
        "memories_in_context",
        Scripted::reply(
            "memories_in_context",
            fake::memory_context("tsk_other", &[MEMORY], false),
        ),
    );
    assert!(
        client
            .memories_in_context(&request().task)
            .await
            .unwrap_err()
            .to_string()
            .contains("another task")
    );
    fake.set_default("retract_memory_feedback", Scripted::reply("memory_feedback", fake::memory_feedback(&json!({"cmd":"record_memory_feedback", "memory":MEMORY,"task":TASK,"verdict":"hurt","evidence":"operator","reason":"why"}), "mfb_other")));
    assert!(
        client
            .retract_memory_feedback(&AetherMemoryFeedbackId::parse("mfb_1").unwrap(), "undo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn snapshot_tasks_are_optional_and_missing_truncated_defaults_to_false() {
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["memory_context".into()]);
    fake.set_default(
        "memories_in_context",
        Scripted::reply(
            "memories_in_context",
            json!({
                "snapshots": [{"memories":[MEMORY]}, {"task":null,"memories":[]}]
            }),
        ),
    );
    let client = connect(&fake).await;
    let context = client.memories_in_context(&request().task).await.unwrap();
    assert!(!context.truncated);
    assert!(
        context
            .snapshots
            .iter()
            .all(|snapshot| snapshot.task.is_none())
    );
    assert_eq!(context.snapshots[0].memories[0].as_str(), MEMORY);
}

#[tokio::test]
async fn resolved_receipts_are_validated_against_the_original_submission() {
    use archon_aether::memory_feedback::FeedbackResolution;
    let (_dir, fake) = fake_aether().await;
    fake.set_features(vec!["memory_feedback".into()]);
    fake.set_submission_horizon(Some("epoch:20".into()));
    fake.set_responder(
        "resolve_submission",
        Arc::new(|cmd| {
            assert_eq!(cmd["horizon"], "epoch:0");
            assert_eq!(cmd["command"]["cmd"], "record_memory_feedback");
            let record = fake::memory_feedback(&cmd["command"], "mfb_resolved");
            Scripted::reply(
                "submission_resolved",
                json!({"outcome":"executed", "reply":{"reply":"memory_feedback", "data":record}}),
            )
        }),
    );
    let client = connect(&fake).await;
    let resolved = client
        .resolve_memory_feedback(request(), Some("epoch:0".into()))
        .await
        .unwrap();
    assert!(
        matches!(resolved, FeedbackResolution::Executed(record) if record.id.as_str() == "mfb_resolved")
    );
    let mut wrong = fake::memory_feedback(
        &serde_json::to_value(archon_aether::wire::Command::RecordMemoryFeedback(request()))
            .unwrap(),
        "mfb_wrong",
    );
    wrong["reason"] = json!("another submission");
    fake.set_default(
        "resolve_submission",
        Scripted::reply(
            "submission_resolved",
            json!({"outcome":"executed", "reply":{"reply":"memory_feedback", "data":wrong}}),
        ),
    );
    assert!(
        client
            .resolve_memory_feedback(request(), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("differs")
    );
}
