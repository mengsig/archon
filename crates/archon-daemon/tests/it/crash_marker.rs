//! What survives an archond crash, and gate questions that must not outlive
//! their gate.

use crate::common;
use crate::link_common;

use archon_core::crash::{self, CrashRecord};
use archon_core::id::QuestionId;
use archon_core::notify::{NotificationKind, Question};
use archon_core::run::{GateDecision, GateSubject, OperatorGate};

/// A crash marker left by an earlier run is told once, in the chat, and kept
/// as the last crash for `archon status` and `archon doctor`.
#[tokio::test]
async fn a_crash_marker_is_reported_once_at_the_next_start() {
    let harness = link_common::Harness::new().await;
    let data_dir = harness.ctx.paths.data_dir.clone();
    let record = CrashRecord::new(
        "panic",
        "assertion failed: !ready || passed",
        Some("a.rs:1:2".into()),
    );
    crash::write_pending(&data_dir, &record).unwrap();

    archon_daemon::report_crash(&harness.ctx).await.unwrap();
    archon_daemon::report_crash(&harness.ctx).await.unwrap();

    let told = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(told.len(), 1, "told once: {told:?}");
    assert!(
        told[0].contains("assertion failed: !ready || passed"),
        "{told:?}"
    );
    assert!(told[0].contains("a.rs:1:2"), "{told:?}");
    assert_eq!(crash::read_pending(&data_dir).unwrap(), None);
    assert_eq!(crash::read_latest(&data_dir).unwrap(), Some(record));
}

/// The operator decided the gate, but the step that answers its question
/// never ran: the next maintenance pass closes the question.
#[tokio::test]
async fn maintenance_closes_the_question_of_a_decided_gate() {
    let h = common::Harness::new().await;
    let item = h.enqueue("gated").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: "Approve the report?".into(),
        options: vec!["yes".into(), "reject".into()],
        item: Some(item.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let question = {
        let mut store = h.ctx.store.lock().await;
        let question = store.insert_question(&question).unwrap();
        let mut gate = store
            .open_gate(&OperatorGate {
                run: run.id.clone(),
                item: item.id.clone(),
                stage_id: "propose".into(),
                attempt: 1,
                subject: GateSubject::Report,
                summary: "done".into(),
                page: None,
                location: None,
                report: None,
                question: Some(question.id.clone()),
                decision: None,
                withdrawn: None,
                opened_at: archon_core::now(),
                decided_at: None,
            })
            .unwrap();
        gate.decision = Some(GateDecision::Approve { note: None });
        gate.decided_at = Some(archon_core::now().max(gate.opened_at));
        store.update_gate(&gate).unwrap();
        assert!(store.question(&question.id).unwrap().is_open());
        question
    };

    archon_daemon::executor::coordination::reconcile(&h.ctx)
        .await
        .unwrap();

    let closed = h.ctx.store.lock().await.question(&question.id).unwrap();
    assert!(!closed.is_open(), "{closed:?}");
    assert!(closed.answer.unwrap().contains("approved"));
}
