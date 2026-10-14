mod common;
use archon_core::outcome::Correction;
use archon_core::queue::QueueState;
use archon_core::run::StageState;
use common::*;

#[test]
fn backfill_is_bounded_idempotent_and_never_touches_live_runs() {
    let mut store = store();
    for n in 0..5 {
        let item = store
            .insert_item(&item(None, &format!("item {n}")), None)
            .unwrap();
        let run = start(&mut store, &item.id);
        if n == 4 {
            continue;
        }
        settle_run(
            &mut store,
            &run,
            QueueState::Succeeded {
                run: run.id.clone(),
                summary: "done".into(),
            },
            t(50),
        );
    }
    assert!(!store.backfill_outcomes(2, true).unwrap());
    assert!(store.outcomes(None).unwrap().len() <= 2);
    while !store.backfill_outcomes(2, true).unwrap() {}
    assert_eq!(store.outcomes(None).unwrap().len(), 4);
    assert!(!store.backfill_outcomes(2, true).unwrap());
    assert_eq!(store.outcomes(None).unwrap().len(), 4);
}

#[test]
fn retry_of_success_is_a_correction_and_live_corrections_attach_at_finish() {
    let mut store = store();
    let item = store.insert_item(&item(None, "correct"), None).unwrap();
    let run = start(&mut store, &item.id);
    store
        .record_correction(&run.id, Correction::Steered)
        .unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    store
        .transition_item(&item.id, QueueState::Queued, t(60))
        .unwrap();
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(
        outcome.corrections,
        vec![Correction::Steered, Correction::RetryAfterDone]
    );
    assert!(!outcome.is_win());
}

#[test]
fn late_corrections_keep_imported_history_and_pending_merge_observations() {
    let mut store = store();
    let item = store.insert_item(&item(None, "history"), None).unwrap();
    let run = start(&mut store, &item.id);
    store.outcome_merged(&run.id, true).unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    let finished = store.run(&run.id).unwrap();
    store.capture_outcome(&finished, None, true).unwrap();
    let mut imported = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(imported.merged, Some(true));
    imported.corrections = vec![Correction::GateRejected];
    store.record_outcome(&imported).unwrap();
    store
        .record_correction(&run.id, Correction::Steered)
        .unwrap();
    store.outcome_merged(&run.id, false).unwrap();
    store.capture_outcome(&finished, None, true).unwrap();
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(outcome.merged, Some(false));
    assert_eq!(
        outcome.corrections,
        vec![
            Correction::GateRejected,
            Correction::Steered,
            Correction::PrClosed
        ]
    );
}

#[test]
fn outcome_preserves_stage_attempts_verdicts_and_size() {
    let mut store = store();
    let item = store.insert_item(&item(None, "stages"), None).unwrap();
    let run = start(&mut store, &item.id);
    for n in 0..2 {
        let mut stage = stage(&run, 0);
        stage.attempt = n + 1;
        stage.state = StageState::Completed;
        stage.finished_at = Some(t(25));
        stage.cost_usd = 0.4;
        store.upsert_stage_run(&stage).unwrap();
    }
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    store
        .capture_outcome(
            &store.run(&run.id).unwrap(),
            Some(archon_core::outcome::DiffStat {
                files: 1,
                added: 100,
                deleted: 0,
            }),
            true,
        )
        .unwrap();
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(outcome.size(), Some(archon_core::outcome::DiffSize::Medium));
    assert_eq!(outcome.stages[0].attempts, 2);
    assert_eq!(outcome.stages[0].cost_usd, 0.8);
}

#[test]
fn outcome_cost_includes_descendants_and_estimates() {
    let mut store = store();
    let item = store
        .insert_item(&item(None, "delegated cost"), None)
        .unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = running_stage(&run, 0, 0);
    stage.state = StageState::Completed;
    stage.finished_at = Some(t(30));
    store.upsert_stage_run(&stage).unwrap();
    store
        .record_delegated(&agent(1), &agent(0), &choice().model, Some(choice().effort))
        .unwrap();
    store.link_agent_run(&agent(1), &run.id).unwrap();
    for (n, cost) in [(0, 1.0), (1, 2.0)] {
        store
            .record_spend(&archon_queue::store::SpendRow {
                agent: agent(n),
                item: Some(item.id.clone()),
                cost_usd: cost,
                cost_basis: archon_core::cost::CostBasis::ProviderReported,
                cost_complete: Some(true),
                estimated_cost_usd: 0.5,
                input_tokens: 1,
                output_tokens: 1,
                at: t(25),
            })
            .unwrap();
    }
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(40),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(outcome.cost_usd, 4.0);
    assert_eq!(outcome.stages[0].cost_usd, 4.0);
    assert_eq!(outcome.stages[0].choice, choice());
}

#[test]
fn missing_outcome_reconciliation_continues_after_initial_backfill() {
    let mut store = store();
    let item = store.insert_item(&item(None, "late finish"), None).unwrap();
    let run = start(&mut store, &item.id);
    assert!(store.backfill_outcomes(20, true).unwrap());
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    assert!(store.outcome(&run.id).unwrap().is_none());
    store.backfill_outcomes(20, true).unwrap();
    assert!(store.outcome(&run.id).unwrap().is_some());
}

#[test]
fn malformed_reported_diff_cannot_prevent_capture_or_pin_backfill() {
    let mut store = store();
    let item = store
        .insert_item(&item(None, "untrusted diff"), None)
        .unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = stage(&run, 0);
    stage.structured = Some(serde_json::json!({"diff_stat":"3 files changed"}));
    store.upsert_stage_run(&stage).unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    let finished = store.run(&run.id).unwrap();
    let measured = archon_core::outcome::DiffStat {
        files: 2,
        added: 12,
        deleted: 3,
    };
    store
        .capture_outcome(&finished, Some(measured), true)
        .unwrap();
    assert_eq!(
        store.outcome(&run.id).unwrap().unwrap().diff,
        Some(measured)
    );
    store.capture_outcome(&finished, None, true).unwrap();
    assert!(store.backfill_outcomes(20, true).unwrap());
}

#[test]
fn outcome_cost_tracks_late_provider_settlement_in_both_directions() {
    let mut store = store();
    let item = store.insert_item(&item(None, "settlement"), None).unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = running_stage(&run, 0, 0);
    stage.state = StageState::Completed;
    stage.finished_at = Some(t(30));
    store.upsert_stage_run(&stage).unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    for (time, cost, basis) in [
        (55, 2.0, archon_core::cost::CostBasis::ListPrice),
        (60, 1.0, archon_core::cost::CostBasis::ProviderReported),
    ] {
        store
            .record_spend(&archon_queue::store::SpendRow {
                agent: agent(0),
                item: Some(item.id.clone()),
                cost_usd: cost,
                cost_basis: basis,
                cost_complete: Some(true),
                estimated_cost_usd: 0.0,
                input_tokens: 1,
                output_tokens: 1,
                at: t(time),
            })
            .unwrap();
        let outcome = store.outcome(&run.id).unwrap().unwrap();
        assert_eq!(outcome.cost_usd, cost);
        assert_eq!(outcome.stages[0].cost_usd, cost);
        assert_eq!(
            store.learning_stats(None).unwrap().pipelines[0].median_cost_usd,
            cost
        );
        assert_eq!(
            store
                .route_evidence(None, "quick-pr", "implement", None)
                .unwrap()[0]
                .median_cost_usd,
            cost
        );
    }
}

#[test]
fn infrastructure_failure_does_not_become_negative_model_evidence() {
    let mut store = store();
    let item = store.insert_item(&item(None, "infra"), None).unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = stage(&run, 0);
    stage.state = StageState::Failed {
        error: "Aether unavailable".into(),
    };
    stage.finished_at = Some(t(30));
    store.upsert_stage_run(&stage).unwrap();
    settle_run(
        &mut store,
        &run,
        QueueState::Failed {
            run: Some(run.id.clone()),
            error: "Aether unavailable".into(),
        },
        t(50),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    assert!(
        store
            .route_evidence(None, "quick-pr", "implement", None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn quality_feedback_is_stage_specific_and_scope_changes_are_not_failures() {
    use archon_core::outcome::{CorrectionCause, CorrectionEvent};
    let mut store = store();
    let item = store.insert_item(&item(None, "attribution"), None).unwrap();
    let run = start(&mut store, &item.id);
    for name in ["implement", "review"] {
        let mut stage = stage(&run, 0);
        stage.stage_id = name.into();
        stage.state = StageState::Completed;
        stage.finished_at = Some(t(30));
        if name == "review" {
            stage.verdict = Some(archon_core::run::Verdict::Fail);
        }
        store.upsert_stage_run(&stage).unwrap();
    }
    capture_success(&mut store, &run, None);
    let mut feedback = CorrectionEvent {
        item: item.id.clone(),
        stage: "implement".into(),
        cause: CorrectionCause::Scope,
        reason: "Additional scope requested".into(),
    };
    store.record_feedback(&run.id, feedback.clone()).unwrap();
    assert!(
        store
            .outcome(&run.id)
            .unwrap()
            .unwrap()
            .corrections
            .is_empty()
    );
    feedback.cause = CorrectionCause::Quality;
    feedback.reason = "The implementation missed the invariant".into();
    store.record_feedback(&run.id, feedback).unwrap();
    assert_eq!(
        store
            .route_evidence(None, "quick-pr", "implement", None)
            .unwrap()[0]
            .route
            .passed,
        0
    );
    assert_eq!(
        store
            .route_evidence(None, "quick-pr", "review", None)
            .unwrap()[0]
            .route
            .passed,
        1
    );
}

#[test]
fn model_segments_preserve_the_start_choice_and_attribute_the_final_result() {
    let mut store = store();
    let item = store.insert_item(&item(None, "segment"), None).unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = running_stage(&run, 0, 0);
    store.upsert_stage_run(&stage).unwrap();
    let mut next = choice();
    next.model = "codex/next".parse().unwrap();
    store
        .record_model_segment(&stage.id, "delivery", &next, t(25), true)
        .unwrap();
    store
        .record_model_segment(&stage.id, "delivery", &next, t(25), true)
        .unwrap();
    stage.state = StageState::Completed;
    stage.finished_at = Some(t(30));
    stage.cost_usd = 2.0;
    store.upsert_stage_run(&stage).unwrap();
    let saved = store.stage_run(&stage.id).unwrap();
    assert_eq!(saved.choice, stage.choice);
    assert_eq!(saved.current_choice(), &next);
    assert_eq!(saved.model_segments.len(), 1);
    settle_run(
        &mut store,
        &run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    let outcome = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(
        outcome
            .stages
            .iter()
            .find(|s| s.choice == next)
            .unwrap()
            .cost_usd,
        2.0
    );
    let evidence = store
        .route_evidence(None, "quick-pr", "implement", None)
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].route.choice, next);
    assert_eq!(evidence[0].route.passed, 1);
}

#[test]
fn corrupt_history_is_marked_and_does_not_pin_the_reconciliation_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let mut store = archon_queue::Store::open(&path).unwrap();
    let mut ids = Vec::new();
    for n in 0..3 {
        let item = store
            .insert_item(&item(None, &format!("historical {n}")), None)
            .unwrap();
        let run = start(&mut store, &item.id);
        settle_run(
            &mut store,
            &run,
            QueueState::Succeeded {
                run: run.id.clone(),
                summary: "done".into(),
            },
            t(50),
        );
        ids.push(run.id);
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE runs SET data='broken history' WHERE id=?1",
        [ids[0].as_str()],
    )
    .unwrap();
    for _ in 0..4 {
        store.backfill_outcomes(1, true).unwrap();
    }
    assert_eq!(store.outcomes(None).unwrap().len(), 2);
    assert!(
        store
            .kv_get(&format!("learning/backfill/skipped/{}", ids[0]))
            .unwrap()
            .is_some()
    );
    for _ in 0..4 {
        store.backfill_outcomes(1, true).unwrap();
    }
    assert_eq!(store.outcomes(None).unwrap().len(), 2);
}

#[test]
fn routing_matches_revision_review_task_class_and_known_diff_size() {
    use archon_core::outcome::{DiffSize, DiffStat, RouteContext};
    let mut store = store();
    let item = store.insert_item(&item(None, "context"), None).unwrap();
    let run = start(&mut store, &item.id);
    let mut stage = stage(&run, 0);
    stage.state = StageState::Completed;
    stage.finished_at = Some(t(25));
    store.upsert_stage_run(&stage).unwrap();
    capture_success(
        &mut store,
        &run,
        Some(DiffStat {
            files: 1,
            added: 100,
            deleted: 0,
        }),
    );
    let mut outcome = store.outcome(&run.id).unwrap().unwrap();
    let context = RouteContext {
        revision: "revision-a".into(),
        review: "strict".into(),
        task_class: "docs".into(),
    };
    outcome.context = Some(context.clone());
    store.record_outcome(&outcome).unwrap();
    let samples = |context: &RouteContext, size| {
        store
            .contextual_route_evidence(None, &run.pipeline, &stage.stage_id, size, Some(context))
            .unwrap()
    };
    assert_eq!(samples(&context, None)[0].route.runs, 1);
    assert_eq!(samples(&context, Some(DiffSize::Medium))[0].route.passed, 1);
    assert!(samples(&context, Some(DiffSize::Small)).is_empty());
    for mismatch in [
        RouteContext {
            revision: "revision-b".into(),
            ..context.clone()
        },
        RouteContext {
            review: "none".into(),
            ..context.clone()
        },
        RouteContext {
            task_class: "code".into(),
            ..context.clone()
        },
    ] {
        assert!(samples(&mismatch, None).is_empty());
    }
}

fn capture_success(
    store: &mut archon_queue::Store,
    run: &archon_core::run::PipelineRun,
    diff: Option<archon_core::outcome::DiffStat>,
) {
    assert!(!store.run(&run.id).unwrap().state.is_terminal());
    settle_run(
        store,
        run,
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "done".into(),
        },
        t(50),
    );
    store
        .capture_outcome(&store.run(&run.id).unwrap(), diff, true)
        .unwrap();
    assert!(store.outcome(&run.id).unwrap().unwrap().is_win());
}

#[test]
fn imported_corrections_do_not_leak_into_live_distillation_input() {
    let mut store = store();
    let repo = repo("history");
    store.insert_repo(&repo).unwrap();
    let item = store
        .insert_item(&item(Some(&repo), "imported correction"), None)
        .unwrap();
    let run = start(&mut store, &item.id);
    capture_success(&mut store, &run, None);
    store
        .record_correction(&run.id, Correction::OperatorContradiction)
        .unwrap();
    assert_eq!(store.corrections_since(&repo.id, "").unwrap().len(), 1);
    store
        .save_learning_value(&format!("learning/imported/{}", run.id), "1")
        .unwrap();
    assert!(store.corrections_since(&repo.id, "").unwrap().is_empty());
}

#[test]
fn live_correction_distillation_input_is_bounded() {
    let mut store = store();
    let repo = repo("bounded-corrections");
    store.insert_repo(&repo).unwrap();
    for n in 0..105 {
        let item = store
            .insert_item(&item(Some(&repo), &format!("correction {n}")), None)
            .unwrap();
        let run = start(&mut store, &item.id);
        capture_success(&mut store, &run, None);
        store
            .record_correction(&run.id, Correction::OperatorContradiction)
            .unwrap();
    }
    let input = store.corrections_since(&repo.id, "").unwrap();
    assert_eq!(input.len(), 100);
    assert!(input.iter().all(|o| o.repo.as_ref() == Some(&repo.id)));
}
