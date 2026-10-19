//! `archon learning` curating one named Aether memory against the fake
//! aetherd: the exact instance and id, convergent retries, and refusals.

use crate::common;
use crate::feedback_common;

use std::sync::{Arc, Mutex};

use archon_aether::AetherClient;
use archon_aether::fake::{self, FakeAether, Scripted};
use archon_core::aether_ids::AetherMemoryId;
use archon_core::error::ArchonError;
use archon_core::feedback::{
    CorrectionRequest, CurationState, FeedbackReceipt, MemoryBefore, MemoryCuration,
};
use archon_core::id::FeedbackId;
use archon_core::outcome::CorrectionCause::Quality;
use archon_core::protocol::Request;
use archon_core::run::PipelineRun;
use archon_queue::store::keys;
use common::Harness;
use feedback_common::*;
use serde_json::{Value, json};

const STALE: &str = "mem_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "mem_01BX5ZZKBKACTAV9WEVGEMMVRZ";
const NEWER: &str = "mem_01CX5ZZKBKACTAV9WEVGEMMVRZ";
const FORGOTTEN_AT: &str = "2026-10-01T09:00:00Z";

type Memories = Arc<Mutex<Vec<Value>>>;

async fn fake_aether(h: &Harness) -> FakeAether {
    FakeAether::start(&h.dir.path().join("aether.sock"))
        .await
        .unwrap()
}

/// Links archond to `fake` as its pump does: the instance is recorded first.
async fn connect(h: &Harness, fake: &FakeAether) {
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    known_instance(h, &client.welcome().instance_id).await;
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
}

async fn known_instance(h: &Harness, instance: &str) {
    let mut store = h.ctx.store.lock().await;
    store.kv_set(keys::AETHER_INSTANCE, instance).unwrap();
}

/// Serves `records` as Aether does: read by id, forgotten and restored in place.
fn serve(fake: &FakeAether, records: Vec<Value>) -> Memories {
    let memories: Memories = Arc::new(Mutex::new(records));
    let held = memories.clone();
    fake.set_responder(
        "memories",
        Arc::new(move |cmd| {
            let all = held.lock().unwrap();
            let found: Vec<_> = all
                .iter()
                .filter(|r| r["id"] == cmd["query"]["id"])
                .collect();
            Scripted::reply("memories", json!(found))
        }),
    );
    let held = memories.clone();
    fake.set_responder(
        "forget_memory",
        Arc::new(move |cmd| {
            change(&held, cmd, |record| {
                if record["forgotten_at"].is_null() {
                    record["forgotten_at"] = json!(FORGOTTEN_AT);
                }
            })
        }),
    );
    let held = memories.clone();
    fake.set_responder(
        "restore_memory",
        Arc::new(move |cmd| change(&held, cmd, |record| record["forgotten_at"] = Value::Null)),
    );
    memories
}

fn change(memories: &Memories, cmd: &Value, edit: impl Fn(&mut Value)) -> Scripted {
    let mut memories = memories.lock().unwrap();
    match memories.iter_mut().find(|r| r["id"] == cmd["id"]) {
        Some(record) => {
            edit(record);
            Scripted::reply("memory", record.clone())
        }
        None => Scripted::refuse("not_found", "no such memory"),
    }
}

fn forgetting(run: &PipelineRun, memory: &str) -> CorrectionRequest {
    CorrectionRequest {
        forget_memory: Some(AetherMemoryId::parse(memory).unwrap()),
        reason: format!("Recalled stale {memory}"),
        ..request(run, "implement", Quality)
    }
}

fn curation(receipt: &FeedbackReceipt) -> &MemoryCuration {
    receipt
        .view
        .feedback
        .curation
        .as_ref()
        .expect("a forget was asked for")
}

#[tokio::test]
async fn a_correction_forgets_exactly_the_named_memory_and_restores_it_apart_from_undo() {
    let h = Harness::new().await;
    let fake = fake_aether(&h).await;
    let stale = fake::memory(STALE, "demo", "Commit as the work address");
    let memories = serve(&fake, vec![stale, fake::memory(OTHER, "demo", "unrelated")]);
    connect(&h, &fake).await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    let wrong = forgetting(&run, STALE);
    let first = correct(&h, wrong.clone()).await.unwrap();
    let id = first.view.feedback.id.clone();
    let applied = curation(&first);
    assert_eq!(applied.state, CurationState::Applied);
    assert_eq!(applied.instance_id, "fake-instance-1");
    assert_eq!(applied.before, Some(MemoryBefore::Current));
    let view = first.view.memory.as_ref().expect("read back from Aether");
    assert_eq!(
        view.origin, "agent agt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "never the operator"
    );
    assert_eq!(view.scope, "repository demo");
    assert!(view.forgotten_at.is_some());
    let forgets = fake.received_named("forget_memory");
    assert_eq!(forgets.len(), 1);
    assert_eq!(forgets[0]["id"], STALE);
    assert_eq!(
        forgets[0]["reason"],
        format!("archon {id}: {}", wrong.reason)
    );
    assert!(
        memories.lock().unwrap()[1]["forgotten_at"].is_null(),
        "the other is kept"
    );

    let again = correct(&h, wrong).await.unwrap();
    assert!(again.replayed && again.view.feedback.id == id);
    assert_eq!(
        fake.received_named("forget_memory").len(),
        1,
        "applied once"
    );
    let undone = receipt(
        &h,
        Request::LearningUndo {
            feedback: id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        curation(&undone).state,
        CurationState::Applied,
        "undo keeps it forgotten"
    );
    assert!(fake.received_named("restore_memory").is_empty());

    restore_once(&h, &fake, &memories, id).await;
}

/// Restores the first served memory `feedback` forgot, then refuses to
/// restore or forget it again on that correction's behalf.
async fn restore_once(h: &Harness, fake: &FakeAether, memories: &Memories, feedback: FeedbackId) {
    let restore = Request::LearningRestoreMemory {
        feedback: feedback.clone(),
    };
    let restored = receipt(h, restore.clone()).await.unwrap();
    assert_eq!(curation(&restored).state, CurationState::Restored);
    assert_eq!(fake.received_named("restore_memory")[0]["id"], STALE);
    assert!(memories.lock().unwrap()[0]["forgotten_at"].is_null());
    let retry = Request::LearningRetryMemory { feedback };
    for refused in [restore, retry] {
        let error = receipt(h, refused).await.unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
    }
    assert_eq!(fake.received_named("restore_memory").len(), 1);
    assert_eq!(fake.received_named("forget_memory").len(), 1);
}

#[tokio::test]
async fn a_forget_is_refused_before_anything_is_recorded_while_no_instance_is_known() {
    let h = Harness::new().await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    let error = correct(&h, forgetting(&run, STALE)).await.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("no Aether instance"), "{error}");
    assert!(learning_why(&h, &run.item).await.feedback.is_empty());
    let plain = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert!(plain.view.feedback.curation.is_none());
}

#[tokio::test]
async fn a_forget_made_while_aether_is_down_is_retried_only_in_the_last_known_instance() {
    let h = Harness::new().await;
    known_instance(&h, "fake-instance-1").await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    let down = correct(&h, forgetting(&run, STALE)).await.unwrap();
    let id = down.view.feedback.id.clone();
    let failed = curation(&down);
    assert_eq!(
        (failed.state, failed.instance_id.as_str()),
        (CurationState::Failed, "fake-instance-1"),
        "bound before anything is sent"
    );
    assert!(failed.error.as_ref().unwrap().contains("not connected"));
    assert!(down.view.penalizing, "the model feedback stands");

    let fake = fake_aether(&h).await;
    let memories = serve(&fake, vec![fake::memory(STALE, "demo", "stale")]);
    let retry = Request::LearningRetryMemory {
        feedback: id.clone(),
    };
    refused_in_another_instance(&h, &fake, retry.clone()).await;
    fake.set_instance_id("fake-instance-1");
    fake.script("forget_memory", Scripted::Hangup { notice: None });
    connect(&h, &fake).await;
    let lost = receipt(&h, retry.clone()).await.unwrap();
    assert_eq!(curation(&lost).state, CurationState::Failed);
    assert_eq!(curation(&lost).instance_id, "fake-instance-1");
    assert_eq!(curation(&lost).before, Some(MemoryBefore::Current));
    memories.lock().unwrap()[0]["forgotten_at"] = json!(FORGOTTEN_AT);

    connect(&h, &fake).await;
    let applied = receipt(&h, retry).await.unwrap();
    let applied = curation(&applied);
    assert_eq!(applied.state, CurationState::Applied);
    assert_eq!(
        applied.before,
        Some(MemoryBefore::Current),
        "read before the first forget"
    );
    assert_eq!(applied.forgotten_at, Some(FORGOTTEN_AT.parse().unwrap()));
    assert_eq!(fake.received_named("forget_memory").len(), 2);
    assert_eq!(
        learning_why(&h, &run.item).await.feedback.len(),
        1,
        "one reward"
    );
}

/// Reconnects archond to another aetherd instance, where `retry` must send
/// nothing and say which instance the memory is curated in.
async fn refused_in_another_instance(h: &Harness, fake: &FakeAether, retry: Request) {
    let sent = fake.received_named("forget_memory").len();
    fake.set_instance_id("fake-instance-2");
    connect(h, fake).await;
    let elsewhere = receipt(h, retry).await.unwrap();
    assert_eq!(curation(&elsewhere).state, CurationState::Failed);
    let error = curation(&elsewhere).error.clone().unwrap();
    assert!(
        error.contains("fake-instance-1") && error.contains("fake-instance-2"),
        "{error}"
    );
    assert_eq!(
        fake.received_named("forget_memory").len(),
        sent,
        "nothing sent elsewhere"
    );
}

#[tokio::test]
async fn restore_refuses_what_this_correction_did_not_forget_or_cannot_bring_back() {
    let h = Harness::new().await;
    let fake = fake_aether(&h).await;
    let mut forgotten = fake::memory(STALE, "demo", "old");
    forgotten["forgotten_at"] = json!("2026-09-15T00:00:00Z");
    let mut superseded = fake::memory(OTHER, "demo", "older");
    superseded["superseded_by"] = json!(NEWER);
    let memories = serve(
        &fake,
        vec![forgotten, superseded, fake::memory(NEWER, "demo", "now")],
    );
    connect(&h, &fake).await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    for (memory, before) in [
        (STALE, MemoryBefore::Forgotten),
        (OTHER, MemoryBefore::Superseded),
    ] {
        let receipt = correct(&h, forgetting(&run, memory)).await.unwrap();
        assert_eq!(curation(&receipt).before, Some(before));
        let feedback = receipt.view.feedback.id.clone();
        let error = refusal(&h, Request::LearningRestoreMemory { feedback }).await;
        assert!(error.contains("already forgotten or superseded"), "{error}");
    }

    let receipt = correct(&h, forgetting(&run, NEWER)).await.unwrap();
    assert_eq!(curation(&receipt).before, Some(MemoryBefore::Current));
    let restore = Request::LearningRestoreMemory {
        feedback: receipt.view.feedback.id,
    };
    let later = [
        (json!(FORGOTTEN_AT), json!(OTHER), "superseded by"),
        (Value::Null, Value::Null, "no longer forgotten"),
        (
            json!("2026-10-02T00:00:00Z"),
            Value::Null,
            "forgotten again",
        ),
    ];
    for (forgotten_at, superseded_by, expected) in later {
        {
            let mut all = memories.lock().unwrap();
            all[2]["forgotten_at"] = forgotten_at;
            all[2]["superseded_by"] = superseded_by;
        }
        let error = refusal(&h, restore.clone()).await;
        assert!(error.contains(expected), "{error}");
    }
    assert!(
        fake.received_named("restore_memory").is_empty(),
        "nothing was restored"
    );
}

/// The refusal of `request`, as text.
async fn refusal(h: &Harness, request: Request) -> String {
    receipt(h, request).await.unwrap_err().to_string()
}

#[tokio::test]
async fn an_aetherd_below_protocol_15_refuses_the_forget_before_anything_is_recorded() {
    let h = Harness::new().await;
    let fake = fake_aether(&h).await;
    fake.set_protocol_window(13, 14);
    connect(&h, &fake).await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    let error = correct(&h, forgetting(&run, STALE)).await.unwrap_err();
    assert!(matches!(error, ArchonError::Aether(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("protocol 14") && text.contains("Nothing was changed"),
        "{text}"
    );
    assert!(learning_why(&h, &run.item).await.feedback.is_empty());
    for cmd in ["memories", "forget_memory"] {
        assert!(fake.received_named(cmd).is_empty(), "{cmd} was never sent");
    }
    let plain = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert!(
        plain.view.feedback.curation.is_none(),
        "a plain correction still records"
    );
}
