use super::*;
use archon_core::outcome::CorrectionCause::{Budget, Infrastructure, Scope};

#[tokio::test]
async fn permanent_discovery_refusals_are_skipped_with_the_reason_in_learning_why() {
    for code in ["not_found", "invalid_argument"] {
        let (h, fake, run, _) = setup(true, false).await;
        fake.set_default(
            "memories_in_context",
            Scripted::refuse(code, "permanent context refusal"),
        );
        let first = correct(&h, request(&run, "implement", Quality))
            .await
            .unwrap();
        assert_eq!(first.view.memory_feedback.pending, 0);
        assert_eq!(first.view.memory_feedback.skipped, 1);
        assert!(
            ledger(&first).tasks[0]
                .skipped
                .as_ref()
                .unwrap()
                .contains("permanent context refusal")
        );
        let why = learning_why(&h, &run.item).await;
        assert!(
            why.feedback[0]
                .feedback
                .memory_feedback
                .as_ref()
                .unwrap()
                .tasks[0]
                .skipped
                .as_ref()
                .unwrap()
                .contains("permanent context refusal")
        );
        retry(&h, &first).await;
        assert_eq!(fake.received_named("memories_in_context").len(), 1);
    }
}

#[tokio::test]
async fn permanent_record_and_retraction_refusals_stop_retrying() {
    for command in ["record_memory_feedback", "retract_memory_feedback"] {
        for code in ["not_found", "invalid_argument"] {
            let (h, fake, run, _) = setup(true, false).await;
            fake.set_default(command, Scripted::refuse(code, "permanent write refusal"));
            let first = correct(&h, request(&run, "implement", Quality))
                .await
                .unwrap();
            let terminal = if command == "retract_memory_feedback" {
                undo(&h, &first).await
            } else {
                first
            };
            assert_eq!(terminal.view.memory_feedback.pending, 0);
            assert_eq!(terminal.view.memory_feedback.skipped, 2);
            assert!(ledger(&terminal).tasks[0].records.iter().all(|r| {
                r.failed
                    .as_ref()
                    .unwrap()
                    .contains("permanent write refusal")
            }));
            retry(&h, &terminal).await;
            assert_eq!(fake.received_named(command).len(), 2);
        }
    }
}

#[tokio::test]
async fn reconnecting_to_an_old_or_different_instance_terminates_pending_discovery() {
    for different_instance in [false, true] {
        let (h, fake, run, _) = setup(true, false).await;
        let first = feedback_common::correct(&h, request(&run, "implement", Quality))
            .await
            .unwrap();
        if different_instance {
            fake.set_instance_id("another-instance");
        } else {
            fake.set_features(Vec::new());
        }
        let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
        h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
        let stopped = settled(&h, first).await;
        assert_eq!(stopped.view.memory_feedback.pending, 0);
        assert_eq!(stopped.view.memory_feedback.skipped, 1);
        let note = ledger(&stopped).tasks[0].skipped.as_ref().unwrap();
        assert!(note.contains(if different_instance {
            "belongs to Aether instance"
        } else {
            "does not advertise"
        }));
        assert!(fake.received_named("memories_in_context").is_empty());
        assert!(fake.received_named("record_memory_feedback").is_empty());
    }
}

#[tokio::test]
async fn only_quality_gets_outcome_evidence_but_every_cause_can_judge_a_named_memory() {
    for cause in [Scope, Infrastructure, Budget] {
        let (h, fake, run, receipts) = setup(true, false).await;
        let first = correct(&h, request(&run, "implement", cause))
            .await
            .unwrap();
        assert_eq!(first.view.memory_feedback.pending, 0);
        assert!(ledger(&first).skipped.as_ref().unwrap().contains("quality"));
        assert!(receipts.lock().unwrap().is_empty());
        assert!(fake.received_named("memories_in_context").is_empty());
        serve_forget(&fake);
        let named = correct(
            &h,
            archon_core::feedback::CorrectionRequest {
                forget_memory: Some(AetherMemoryId::parse(MEMORY).unwrap()),
                ..request(&run, "implement", cause)
            },
        )
        .await
        .unwrap();
        assert_eq!(named.view.memory_feedback.sent, 1);
        assert_eq!(
            fake.received_named("record_memory_feedback")[0]["evidence"],
            "operator"
        );
        assert_eq!(fake.received_named("forget_memory").len(), 1);
    }
}

#[tokio::test]
async fn changed_instances_or_features_also_terminate_entries_already_awaiting_delivery() {
    for different_instance in [false, true] {
        let (h, fake, run, _) = setup(true, false).await;
        fake.script(
            "record_memory_feedback",
            Scripted::refuse("internal", "busy"),
        );
        let first = correct(&h, request(&run, "implement", Quality))
            .await
            .unwrap();
        assert_eq!(first.view.memory_feedback.pending, 1);
        if different_instance {
            fake.set_instance_id("other-instance");
        } else {
            fake.set_features(Vec::new());
        }
        let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
        h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
        let stopped = retry(&h, &first).await;
        let entry = &ledger(&stopped).tasks[0].records[0];
        assert!(
            entry
                .failed
                .as_ref()
                .unwrap()
                .contains(if different_instance {
                    "belongs to Aether instance"
                } else {
                    "does not advertise"
                })
        );
        assert_eq!(stopped.view.memory_feedback.pending, 0);
        assert_eq!(stopped.view.memory_feedback.skipped, 1);
        assert_eq!(fake.received_named("record_memory_feedback").len(), 2);
        assert!(fake.received_named("resolve_submission").is_empty());
    }
}
