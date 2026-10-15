use super::*;
use archon_daemon::handlers::{deliver_memory_feedback, resume_memory_feedback};
use std::time::Duration;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(3);

async fn reconnect(h: &Harness, fake: &FakeAether) {
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    assert!(client.is_connected());
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
    assert!(h.ctx.aether.is_connected());
}

#[tokio::test]
async fn stalled_feedback_does_not_delay_corrections_or_undo_and_reconnect_resumes_it() {
    let (h, fake, run, receipts) = setup(true, false).await;
    fake.script("record_memory_feedback", Scripted::Silent);
    let worker = tokio::spawn(deliver_memory_feedback(h.ctx.clone()));
    let first = timeout(
        Duration::from_millis(250),
        feedback_common::correct(&h, request(&run, "implement", Quality)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.view.memory_feedback.pending, 1);
    fake.wait_for("record_memory_feedback", 1, WAIT).await;
    let mut next = request(&run, "implement", Quality);
    next.reason.push_str(" another correction");
    let second = timeout(
        Duration::from_millis(250),
        feedback_common::correct(&h, next),
    )
    .await
    .unwrap()
    .unwrap();
    let undone = timeout(
        Duration::from_millis(250),
        receipt(
            &h,
            Request::LearningUndo {
                feedback: first.view.feedback.id.clone(),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(undone.view.feedback.revoked_at.is_some());
    assert_ne!(first.view.feedback.id, second.view.feedback.id);
    fake.hangup(None);
    reconnect(&h, &fake).await;
    h.ctx.memory_feedback_ready.notify_one();
    timeout(WAIT, async {
        loop {
            let store = h.ctx.store.lock().await;
            let first = store.operator_feedback(&first.view.feedback.id).unwrap();
            let second = store.operator_feedback(&second.view.feedback.id).unwrap();
            if first.memory_feedback.unwrap().counts().retracted == 1
                && second.memory_feedback.unwrap().counts().sent == 2
            {
                break;
            }
            drop(store);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receipts.lock().unwrap().len(), 3);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn starting_the_worker_delivers_already_recorded_pending_entries() {
    let (h, fake, run, receipts) = setup(true, false).await;
    let first = feedback_common::correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.pending, 1);
    assert!(fake.received_named("record_memory_feedback").is_empty());
    let worker = tokio::spawn(deliver_memory_feedback(h.ctx.clone()));
    fake.wait_for("record_memory_feedback", 2, WAIT).await;
    let _guard = timeout(WAIT, h.ctx.learning_memory.lock()).await.unwrap();
    let stored = h
        .ctx
        .store
        .lock()
        .await
        .operator_feedback(&first.view.feedback.id)
        .unwrap();
    assert_eq!(stored.memory_feedback.unwrap().counts().pending, 0);
    assert_eq!(receipts.lock().unwrap().len(), 2);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn an_expired_receipt_is_never_blindly_resent_even_after_a_later_safe_answer() {
    let (h, fake, run, receipts) = setup(true, false).await;
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    let mut pending = ledger(&first).clone();
    pending.tasks[0].records[0].feedback = None;
    h.ctx
        .store
        .lock()
        .await
        .update_memory_feedback(&first.view.feedback.id, &pending)
        .unwrap();
    receipts.lock().unwrap().retain(|_, r| r["memory"] == OTHER);
    fake.set_default(
        "resolve_submission",
        Scripted::reply(
            "submission_resolved",
            json!({"outcome":"unknown", "reason":"forgotten"}),
        ),
    );
    let unknown = retry(&h, &first).await;
    let entry = &ledger(&unknown).tasks[0].records[0];
    assert!(entry.unconfirmed);
    assert!(entry.error.as_ref().unwrap().contains("forgotten"));
    assert_eq!(unknown.view.memory_feedback.pending, 1);
    serve_feedback(&fake, &receipts);
    let still_unknown = retry(&h, &first).await;
    assert_eq!(still_unknown.view.memory_feedback.pending, 1);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
    let undone = undo(&h, &first).await;
    assert_eq!(undone.view.memory_feedback.pending, 1);
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
}

#[tokio::test]
async fn safe_absence_resends_the_original_command_and_preserves_its_first_horizon() {
    let (h, fake, run, _) = setup(true, false).await;
    fake.script(
        "record_memory_feedback",
        Scripted::refuse("internal", "busy"),
    );
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    assert_eq!(first.view.memory_feedback.pending, 1);
    fake.set_submission_horizon(Some("epoch:99".into()));
    reconnect(&h, &fake).await;
    let retried = retry(&h, &first).await;
    let resolved = &fake.received_named("resolve_submission")[0];
    let commands = fake.received_named("record_memory_feedback");
    assert_eq!(resolved["horizon"], "epoch:0");
    assert_eq!(resolved["command"], {
        let mut c = commands[0].clone();
        c.as_object_mut().unwrap().remove("req");
        c
    });
    assert_eq!(
        commands[0]["idempotency_key"],
        commands[2]["idempotency_key"]
    );
    assert_eq!(
        ledger(&retried).tasks[0].records[0]
            .submission_horizon
            .as_deref(),
        Some("epoch:0")
    );
    assert_eq!(retried.view.memory_feedback.pending, 0);
}

#[tokio::test]
async fn no_resolution_support_keeps_attempted_entries_pending_without_a_resend() {
    let (h, fake, run, _) = setup(true, false).await;
    fake.script(
        "record_memory_feedback",
        Scripted::refuse("internal", "busy"),
    );
    let first = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    fake.set_submission_horizon(None);
    reconnect(&h, &fake).await;
    resume_memory_feedback(&h.ctx).await.unwrap();
    let current = h
        .ctx
        .store
        .lock()
        .await
        .operator_feedback(&first.view.feedback.id)
        .unwrap();
    let entry = &current.memory_feedback.as_ref().unwrap().tasks[0].records[0];
    assert!(entry.unconfirmed);
    assert!(entry.error.as_ref().unwrap().contains("upgrade Aether"));
    assert!(fake.received_named("resolve_submission").is_empty());
    assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
}
