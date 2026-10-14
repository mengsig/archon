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
    assert!(store.backfill_outcomes(2, true).unwrap());
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
