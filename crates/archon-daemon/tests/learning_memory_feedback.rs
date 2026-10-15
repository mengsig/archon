//! Durable feedback for the memories a corrected stage actually saw.

mod common;
mod feedback_common;
#[path = "learning_memory_feedback/recovery.rs"]
mod recovery;
#[path = "learning_memory_feedback/terminal.rs"]
mod terminal;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use archon_aether::AetherClient;
use archon_aether::fake::{self, FakeAether, Scripted};
use archon_core::aether_ids::{AetherMemoryId, AetherTaskId};
use archon_core::feedback::{FeedbackReceipt, MemoryFeedback};
use archon_core::outcome::CorrectionCause::Quality;
use archon_core::protocol::Request;
use archon_core::run::PipelineRun;
use archon_queue::store::keys;
use common::Harness;
use feedback_common::*;
use serde_json::{Value, json};

const MEMORY: &str = "mem_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "mem_01BX5ZZKBKACTAV9WEVGEMMVRZ";
const TASK: &str = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV";

type Receipts = Arc<Mutex<BTreeMap<String, Value>>>;

async fn setup(features: bool, truncated: bool) -> (Harness, FakeAether, PipelineRun, Receipts) {
    let h = Harness::new().await;
    let fake = FakeAether::start(&h.dir.path().join("aether.sock"))
        .await
        .unwrap();
    fake.set_submission_horizon(Some("epoch:0".into()));
    if features {
        fake.set_features(vec!["memory_feedback".into(), "memory_context".into()]);
    }
    let mut context = fake::memory_context(TASK, &[MEMORY, OTHER], truncated);
    context["snapshots"]
        .as_array_mut()
        .unwrap()
        .push(fake::memory_context(TASK, &[MEMORY], false)["snapshots"][0].clone());
    fake.set_default(
        "memories_in_context",
        Scripted::reply("memories_in_context", context),
    );
    let receipts: Receipts = Arc::new(Mutex::new(BTreeMap::new()));
    serve_feedback(&fake, &receipts);
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::AETHER_INSTANCE, &client.welcome().instance_id)
        .unwrap();
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
    let run = sample(&h, &model("cheap"), 0.1).await;
    let mut store = h.ctx.store.lock().await;
    let mut stage = store.stage_runs(&run.id).unwrap().remove(0);
    stage.aether_task = Some(AetherTaskId::parse(TASK).unwrap());
    store.upsert_stage_run(&stage).unwrap();
    assert_eq!(
        store.stage_runs(&run.id).unwrap()[0].aether_task,
        stage.aether_task
    );
    assert_eq!(store.run(&run.id).unwrap().item, run.item);
    drop(store);
    (h, fake, run, receipts)
}

fn serve_feedback(fake: &FakeAether, receipts: &Receipts) {
    let records = receipts.clone();
    fake.set_responder("resolve_submission", Arc::new(move |cmd| {
        assert_eq!(cmd["command"]["cmd"], "record_memory_feedback");
        assert!(cmd["command"]["idempotency_key"].is_string());
        let records = records.lock().unwrap();
        let data = match records.get(cmd["command"]["idempotency_key"].as_str().unwrap()) {
            Some(record) => json!({"outcome":"executed", "reply":{"reply":"memory_feedback", "data":record}}),
            None => json!({"outcome":"absent_safe"}),
        };
        Scripted::reply("submission_resolved", data)
    }));
    let records = receipts.clone();
    fake.set_responder(
        "record_memory_feedback",
        Arc::new(move |cmd| {
            assert_eq!(cmd["verdict"], "hurt");
            assert!(cmd["idempotency_key"].as_str().unwrap().len() <= 200);
            let mut records = records.lock().unwrap();
            let id = format!("mfb_{}", records.len() + 1);
            let record = records
                .entry(cmd["idempotency_key"].as_str().unwrap().into())
                .or_insert_with(|| fake::memory_feedback(cmd, &id));
            Scripted::reply("memory_feedback", record.clone())
        }),
    );
    let records = receipts.clone();
    fake.set_responder(
        "retract_memory_feedback",
        Arc::new(move |cmd| {
            let mut records = records.lock().unwrap();
            let record = records
                .values_mut()
                .find(|r| r["id"] == cmd["feedback"])
                .expect("stored receipt");
            assert!(!cmd["reason"].as_str().unwrap().is_empty());
            assert!(record["id"].as_str().unwrap().starts_with("mfb_"));
            record["retracted_at"] = json!("2026-10-15T10:00:00Z");
            Scripted::reply("memory_feedback", record.clone())
        }),
    );
}

fn ledger(receipt: &FeedbackReceipt) -> &MemoryFeedback {
    receipt
        .view
        .feedback
        .memory_feedback
        .as_ref()
        .expect("durable ledger")
}

async fn settled(h: &Harness, mut first: FeedbackReceipt) -> FeedbackReceipt {
    archon_daemon::handlers::resume_memory_feedback(&h.ctx)
        .await
        .unwrap();
    let why = learning_why(h, &first.view.feedback.item).await;
    first.view = why
        .feedback
        .into_iter()
        .find(|v| v.feedback.id == first.view.feedback.id)
        .unwrap();
    assert!(first.view.feedback.memory_feedback.is_some());
    assert_eq!(first.view.memory_feedback, ledger(&first).counts());
    first
}

async fn correct(
    h: &Harness,
    request: archon_core::feedback::CorrectionRequest,
) -> Result<FeedbackReceipt, archon_core::error::ArchonError> {
    let first = feedback_common::correct(h, request).await?;
    Ok(settled(h, first).await)
}

async fn retry(h: &Harness, first: &FeedbackReceipt) -> FeedbackReceipt {
    receipt(
        h,
        Request::LearningRetryMemory {
            feedback: first.view.feedback.id.clone(),
        },
    )
    .await
    .unwrap()
}

async fn undo(h: &Harness, first: &FeedbackReceipt) -> FeedbackReceipt {
    let undone = receipt(
        h,
        Request::LearningUndo {
            feedback: first.view.feedback.id.clone(),
        },
    )
    .await
    .unwrap();
    settled(h, undone).await
}

fn serve_forget(fake: &FakeAether) {
    let memory = fake::memory(MEMORY, "repo", "unread text");
    fake.set_default(
        "memories",
        Scripted::reply("memories", json!([memory.clone()])),
    );
    let mut forgotten = memory;
    forgotten["forgotten_at"] = json!("2026-10-15T10:00:00Z");
    fake.set_default("forget_memory", Scripted::reply("memory", forgotten));
}

#[tokio::test]
async fn named_memory_gets_operator_evidence_and_still_gets_forgotten() {
    let (h, fake, run, _) = setup(true, false).await;
    serve_forget(&fake);
    let request = archon_core::feedback::CorrectionRequest {
        forget_memory: Some(AetherMemoryId::parse(MEMORY).unwrap()),
        ..request(&run, "implement", Quality)
    };
    let first = correct(&h, request.clone()).await.unwrap();
    assert_eq!(first.view.memory_feedback.sent, 1);
    assert_eq!(ledger(&first).tasks[0].records[0].memory.as_str(), MEMORY);
    let commands = fake.received_named("record_memory_feedback");
    assert_eq!(commands[0]["evidence"], "operator");
    assert_eq!(
        commands[0]["reason"],
        format!("archon {}: {}", first.view.feedback.id, request.reason)
    );
    assert_eq!(fake.received_named("forget_memory").len(), 1);
    assert!(correct(&h, request).await.unwrap().replayed);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 1);
    let undone = undo(&h, &first).await;
    assert_eq!(undone.view.memory_feedback.retracted, 1);
    assert!(fake.received_named("restore_memory").is_empty());
}

#[tokio::test]
async fn unnamed_correction_unions_snapshots_and_reports_partial_context() {
    let (h, fake, run, receipts) = setup(true, true).await;
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.sent, 2);
    assert_eq!(first.view.memory_feedback.partial_tasks, 1);
    assert_eq!(receipts.lock().unwrap().len(), 2);
    assert!(
        fake.received_named("record_memory_feedback")
            .iter()
            .all(|c| c["evidence"] == "outcome")
    );
    let why = learning_why(&h, &run.item).await;
    let json = serde_json::to_value(why).unwrap();
    assert_eq!(json["feedback"][0]["memory_feedback"]["sent"], 2);
    assert_eq!(json["feedback"][0]["memory_feedback"]["partial_tasks"], 1);
    assert_eq!(undo(&h, &first).await.view.memory_feedback.retracted, 2);
    assert!(undo(&h, &first).await.replayed);
    assert_eq!(fake.received_named("retract_memory_feedback").len(), 2);
}

#[tokio::test]
async fn old_aether_records_no_memory_evidence_and_never_fails_the_correction() {
    let (h, fake, run, receipts) = setup(false, false).await;
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.sent, 0);
    assert_eq!(first.view.memory_feedback.pending, 0);
    assert!(
        ledger(&first)
            .skipped
            .as_ref()
            .unwrap()
            .contains("advertise")
    );
    retry(&h, &first).await;
    assert!(receipts.lock().unwrap().is_empty());
    assert!(fake.received_named("memories_in_context").is_empty());
    assert!(fake.received_named("record_memory_feedback").is_empty());
}

#[tokio::test]
async fn named_memory_outside_context_is_skipped_but_forgotten() {
    let (h, fake, run, _) = setup(true, false).await;
    serve_forget(&fake);
    fake.set_default(
        "memories_in_context",
        Scripted::reply(
            "memories_in_context",
            fake::memory_context(TASK, &[OTHER], false),
        ),
    );
    let first = correct(
        &h,
        archon_core::feedback::CorrectionRequest {
            forget_memory: Some(AetherMemoryId::parse(MEMORY).unwrap()),
            ..request(&run, "implement", Quality)
        },
    )
    .await
    .unwrap();
    assert_eq!(first.view.memory_feedback.skipped, 1);
    assert!(
        ledger(&first).tasks[0]
            .skipped
            .as_ref()
            .unwrap()
            .contains("not listed")
    );
    assert!(fake.received_named("record_memory_feedback").is_empty());
    assert_eq!(fake.received_named("forget_memory").len(), 1);
}

#[tokio::test]
async fn failed_delivery_retries_the_same_key_through_correct_or_retry_memory() {
    let (h, fake, run, receipts) = setup(true, false).await;
    fake.script(
        "record_memory_feedback",
        Scripted::refuse("internal", "temporary write failure"),
    );
    let request = request(&run, "implement", Quality);
    let first = correct(&h, request.clone()).await.unwrap();
    assert_eq!(first.view.memory_feedback.pending, 1);
    assert_eq!(first.view.memory_feedback.sent, 1);
    assert!(ledger(&first).tasks[0].records[0].error.is_some());
    let again = correct(&h, request).await.unwrap();
    assert!(again.replayed);
    assert_eq!(again.view.memory_feedback.sent, 2);
    assert_eq!(again.view.memory_feedback.pending, 0);
    let commands = fake.received_named("record_memory_feedback");
    assert_eq!(
        commands[0]["idempotency_key"],
        commands[2]["idempotency_key"]
    );
    assert_eq!(commands[0]["reason"], commands[2]["reason"]);
    retry(&h, &first).await;
    assert_eq!(receipts.lock().unwrap().len(), 2);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 3);
    assert_eq!(fake.received_named("memories_in_context").len(), 1);
}

#[tokio::test]
async fn context_failures_are_durable_and_retry_memory_resumes_discovery() {
    let (h, fake, run, _) = setup(true, false).await;
    fake.script(
        "memories_in_context",
        Scripted::refuse("internal", "context unavailable"),
    );
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.pending, 1);
    assert!(
        ledger(&first).tasks[0]
            .error
            .as_ref()
            .unwrap()
            .contains("context unavailable")
    );
    let retried = retry(&h, &first).await;
    assert_eq!(retried.view.memory_feedback.sent, 2);
    assert_eq!(retried.view.memory_feedback.pending, 0);
    assert!(ledger(&retried).tasks[0].error.is_none());
}

#[tokio::test]
async fn undo_recovers_a_lost_receipt_and_drops_never_attempted_entries() {
    let (h, fake, run, receipts) = setup(true, false).await;
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    let mut pending = ledger(&first).clone();
    pending.tasks[0].records[0].feedback = None;
    pending.tasks[0].records[1].feedback = None;
    pending.tasks[0].records[1].attempted = false;
    pending.tasks[0].records[1].submission_horizon = None;
    receipts
        .lock()
        .unwrap()
        .retain(|_, r| r["memory"] == MEMORY);
    h.ctx
        .store
        .lock()
        .await
        .update_memory_feedback(&first.view.feedback.id, &pending)
        .unwrap();
    let undone = undo(&h, &first).await;
    assert_eq!(undone.view.memory_feedback.retracted, 1);
    assert_eq!(undone.view.memory_feedback.skipped, 1);
    assert!(ledger(&undone).tasks[0].records[1].dropped);
    assert_eq!(fake.received_named("resolve_submission").len(), 1);
    assert_eq!(fake.received_named("retract_memory_feedback").len(), 1);
    assert_eq!(retry(&h, &undone).await.view.memory_feedback.pending, 0);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
}

#[tokio::test]
async fn failed_retractions_remain_pending_and_undo_retries_them() {
    let (h, fake, run, _) = setup(true, false).await;
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    fake.script(
        "retract_memory_feedback",
        Scripted::refuse("internal", "busy"),
    );
    let undone = undo(&h, &first).await;
    assert!(undone.view.feedback.revoked_at.is_some());
    assert_eq!(undone.view.memory_feedback.pending, 1);
    assert_eq!(undone.view.memory_feedback.retracted, 1);
    let again = undo(&h, &first).await;
    assert!(again.replayed);
    assert_eq!(again.view.memory_feedback.retracted, 2);
    assert_eq!(again.view.memory_feedback.pending, 0);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
}

#[tokio::test]
async fn feedback_follows_each_task_of_the_corrected_stage_only() {
    let (h, fake, _, _) = setup(true, false).await;
    let choice = model("cheap");
    let attempts = [
        ("implement", &choice),
        ("implement", &choice),
        ("review", &choice),
    ];
    let run = sample_with(&h, &attempts, 0.1, archon_core::now(), None).await;
    let mut store = h.ctx.store.lock().await;
    for mut stage in store.stage_runs(&run.id).unwrap() {
        let task = match (stage.stage_id.as_str(), stage.attempt) {
            ("implement", 1) => TASK,
            ("implement", 2) => "tsk_second",
            ("review", _) => "tsk_review",
            _ => panic!("only the fixture's attempts"),
        };
        stage.aether_task = Some(AetherTaskId::parse(task).unwrap());
        store.upsert_stage_run(&stage).unwrap();
    }
    drop(store);
    fake.set_responder(
        "memories_in_context",
        Arc::new(|cmd| {
            let task = cmd["task"].as_str().unwrap();
            assert_ne!(task, "tsk_review");
            assert!(task == TASK || task == "tsk_second");
            Scripted::reply(
                "memories_in_context",
                fake::memory_context(task, &[MEMORY], false),
            )
        }),
    );
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.sent, 2);
    assert_eq!(ledger(&first).tasks.len(), 2);
    let commands = fake.received_named("record_memory_feedback");
    assert_ne!(commands[0]["task"], commands[1]["task"]);
    assert_ne!(
        commands[0]["idempotency_key"],
        commands[1]["idempotency_key"]
    );
}

#[tokio::test]
async fn lost_transport_reply_is_recovered_with_its_original_key_before_undo() {
    let (h, fake, run, receipts) = setup(true, false).await;
    let records = receipts.clone();
    fake.set_responder(
        "record_memory_feedback",
        Arc::new(move |cmd| {
            assert_eq!(cmd["memory"], MEMORY);
            let record = fake::memory_feedback(cmd, "mfb_lost");
            assert!(
                records
                    .lock()
                    .unwrap()
                    .insert(cmd["idempotency_key"].as_str().unwrap().into(), record)
                    .is_none()
            );
            Scripted::Hangup { notice: None }
        }),
    );
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.sent, 0);
    assert_eq!(first.view.memory_feedback.pending, 2);
    assert_eq!(receipts.lock().unwrap().len(), 1);
    serve_feedback(&fake, &receipts);
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
    let undone = undo(&h, &first).await;
    assert_eq!(undone.view.memory_feedback.retracted, 1);
    assert_eq!(undone.view.memory_feedback.skipped, 1);
    let commands = fake.received_named("record_memory_feedback");
    assert_eq!(commands.len(), 1);
    assert_eq!(fake.received_named("resolve_submission").len(), 1);
    assert_eq!(receipts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_replays_share_one_delivery_and_do_not_hold_the_store_lock() {
    let (h, fake, run, _) = setup(true, false).await;
    let ctx = Arc::downgrade(&h.ctx);
    fake.set_responder(
        "record_memory_feedback",
        Arc::new(move |cmd| {
            assert!(
                ctx.upgrade().unwrap().store.try_lock().is_ok(),
                "Aether calls run without the store lock"
            );
            assert_eq!(cmd["task"], TASK);
            Scripted::reply(
                "memory_feedback",
                fake::memory_feedback(cmd, "mfb_concurrent"),
            )
        }),
    );
    let request = request(&run, "implement", Quality);
    let (first, again) = tokio::join!(correct(&h, request.clone()), correct(&h, request));
    let (first, again) = (first.unwrap(), again.unwrap());
    assert_ne!(first.replayed, again.replayed);
    assert_eq!(first.view.feedback.id, again.view.feedback.id);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
    assert_eq!(fake.received_named("memories_in_context").len(), 1);
}
