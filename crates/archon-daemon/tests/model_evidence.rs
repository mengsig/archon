//! Read-only evidence tolerates unavailable definitions and prefers run snapshots.
mod common;

use archon_core::model_evidence::ModelEvidenceQuery;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::run::{StageState, Verdict};
use archon_daemon::handlers;
use common::{Harness, PIPELINE, completed};

#[tokio::test]
async fn deleted_pipeline_does_not_fail_evidence_for_other_items() {
    use archon_core::model::{Effort, ModelPlan};
    use common::{MODEL, choice};

    let h = Harness::new().await;
    let pipeline = "model-evidence-deleted-pipeline";
    h.add_pipeline(
        pipeline,
        r#"[pipeline]
name = "model-evidence-deleted-pipeline"
description = "removed historical pipeline"
version = 1
workspace = "worktree"
requires_repo = true
[[stage]]
id = "obsolete-check"
kind = "review"
prompt = "implement.md"
gate = "all_pass"
"#,
    );
    let plan = ModelPlan([("obsolete-check".into(), vec![choice(MODEL, Effort::Low)])].into());
    let obsolete = h
        .enqueue_in(pipeline, "historical item", plan)
        .await
        .unwrap();
    h.dispatch().await;
    h.settle_running(|_| completed("VERDICT: PASS")).await;
    let mut historical_stage = h.stages(&obsolete.id).await.remove(0);
    assert_eq!(historical_stage.state, StageState::Completed);
    assert_eq!(historical_stage.verdict, Some(Verdict::Pass));
    assert!(
        !h.ctx
            .paths
            .data_dir
            .join("pipeline-runs")
            .join(historical_stage.run.as_str())
            .exists(),
        "the finished run's frozen spec has been discarded"
    );
    historical_stage.finished_at = Some(historical_stage.started_at + chrono::Duration::minutes(2));
    historical_stage.cost_usd = 0.25;
    h.ctx
        .store
        .lock()
        .await
        .upsert_stage_run(&historical_stage)
        .unwrap();

    h.enqueue("normal item").await;
    h.dispatch().await;
    h.settle_running(|_| completed("Applied supplied fix"))
        .await;
    std::fs::rename(
        h.repo.join(".archon/pipelines").join(pipeline),
        h.dir.path().join("removed-pipeline"),
    )
    .unwrap();

    let reply = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery::default()))
        .await
        .expect("a deleted pipeline must not fail the whole evidence report");
    let Reply::ModelEvidence(report) = reply else {
        panic!("evidence reply");
    };
    let normal = report.rows.iter().find(|r| r.stage == "implement").unwrap();
    assert_eq!((normal.attempts, normal.completed), (1, 1));
    let historical = report
        .rows
        .iter()
        .find(|r| r.stage == "obsolete-check")
        .unwrap();
    assert_eq!((historical.attempts, historical.completed), (1, 1));
    assert_eq!(historical.median_cost_usd, Some(0.25));
    assert_eq!(historical.median_minutes, Some(2.0));
    assert_eq!((historical.passed, historical.verdicts), (0, 0));
}

#[tokio::test]
async fn mcp_can_read_evidence_and_only_review_or_test_verdicts_contribute() {
    let h = Harness::new().await;
    let item = h.enqueue("evidence fixture").await;
    h.dispatch().await;
    h.settle_running(|_| completed("Applied supplied fix"))
        .await;
    h.dispatch().await;
    h.settle_running(|_| completed("VERDICT: PASS")).await;
    let stages = h.stages(&item.id).await;
    assert!(
        stages
            .iter()
            .any(|s| s.stage_id == "implement" && s.verdict == Some(Verdict::Unknown))
    );
    let query = ModelEvidenceQuery {
        pipeline: Some(PIPELINE.into()),
        ..Default::default()
    };
    let reply = handlers::handle(&h.ctx, ClientKind::ArchonMcp, Request::ModelEvidence(query))
        .await
        .unwrap();
    let Reply::ModelEvidence(report) = reply else {
        panic!("evidence reply");
    };
    let work = report.rows.iter().find(|r| r.stage == "implement").unwrap();
    assert_eq!((work.completed, work.verdicts), (1, 0));
    let reviews: Vec<_> = report.rows.iter().filter(|r| r.stage == "review").collect();
    assert_eq!(reviews.len(), 2);
    assert!(
        reviews
            .iter()
            .all(|r| (r.completed, r.passed, r.verdicts) == (1, 1, 1))
    );
    assert!(
        stages
            .iter()
            .filter(|s| s.stage_id == "review")
            .all(|s| s.state == StageState::Completed)
    );
    assert_eq!(
        report.estimates_counted,
        h.ctx.config.limits.count_estimated_cost
    );
}

#[tokio::test]
async fn test_stage_with_a_custom_id_has_a_pass_share() {
    use archon_core::model::{Effort, ModelPlan};
    use common::{MODEL, choice};
    let h = Harness::new().await;
    h.add_pipeline(
        "custom-evidence",
        r#"[pipeline]
name = "custom-evidence"
description = "custom test"
version = 1
workspace = "worktree"
requires_repo = true
[[stage]]
id = "check-quality"
kind = "test"
prompt = "implement.md"
gate = "all_pass"
"#,
    );
    let plan = ModelPlan([("check-quality".into(), vec![choice(MODEL, Effort::Low)])].into());
    h.enqueue_in("custom-evidence", "custom verdict", plan)
        .await
        .unwrap();
    h.dispatch().await;
    h.settle_running(|_| completed("VERDICT: PASS")).await;
    let Reply::ModelEvidence(report) = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery {
            pipeline: Some("custom-evidence".into()),
            ..Default::default()
        }))
        .await
        .unwrap()
    else {
        panic!("evidence");
    };
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].stage, "check-quality");
    assert_eq!((report.rows[0].passed, report.rows[0].verdicts), (1, 1));
}

const CLASSIFICATION_PIPELINE: &str = "classification-evidence";

// The trailing work stage keeps the run live so its frozen spec survives.
async fn classification_item(h: &Harness, kind: &str) -> archon_core::queue::QueueItem {
    use archon_core::model::{Effort, ModelPlan};
    use common::{MODEL, choice};

    let fan_out = if kind == "work" { 1 } else { 2 };
    let gate = if kind == "work" {
        ""
    } else {
        "gate = \"all_pass\""
    };
    h.add_pipeline(
        CLASSIFICATION_PIPELINE,
        &format!(
            r#"[pipeline]
name = "classification-evidence"
description = "historical stage classification"
version = 1
workspace = "worktree"
requires_repo = true
[[stage]]
id = "historical-check"
kind = "{kind}"
prompt = "implement.md"
fan_out = {fan_out}
{gate}
[[stage]]
id = "after"
kind = "work"
prompt = "implement.md"
"#
        ),
    );
    let plan = ModelPlan(
        [
            (
                "historical-check".into(),
                vec![choice(MODEL, Effort::Low); fan_out],
            ),
            ("after".into(), vec![choice(MODEL, Effort::Low)]),
        ]
        .into(),
    );
    let item = h
        .enqueue_in(CLASSIFICATION_PIPELINE, "classification fixture", plan)
        .await
        .unwrap();
    h.dispatch().await;
    let checks: Vec<_> = h
        .running()
        .await
        .into_iter()
        .filter(|s| s.stage_id == "historical-check")
        .collect();
    assert_eq!(checks.len(), fan_out);
    for stage in checks {
        archon_daemon::executor::stage_settled(
            &h.ctx,
            stage.aether_agent.as_ref().unwrap(),
            completed("VERDICT: PASS"),
        )
        .await
        .unwrap();
    }
    let mut stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), fan_out);
    for stage in &mut stages {
        assert_eq!(stage.state, StageState::Completed);
        stage.cost_usd = 0.25 + f64::from(stage.fan_index) * 0.5;
        stage.finished_at =
            Some(stage.started_at + chrono::Duration::minutes(2 + i64::from(stage.fan_index) * 2));
        h.ctx.store.lock().await.upsert_stage_run(stage).unwrap();
    }
    item
}

async fn evidence(h: &Harness) -> archon_core::model_evidence::ModelEvidenceReport {
    let Reply::ModelEvidence(report) = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery::default()))
        .await
        .expect("historical definitions must not fail evidence")
    else {
        panic!("evidence reply");
    };
    *report
}

async fn snapshot_dir(h: &Harness, item: &archon_core::queue::QueueItem) -> std::path::PathBuf {
    h.ctx
        .paths
        .data_dir
        .join("pipeline-runs")
        .join(h.run(&item.id).await.id.as_str())
        .join(CLASSIFICATION_PIPELINE)
}

#[tokio::test]
async fn frozen_review_counts_verdicts_when_current_pipeline_is_deleted() {
    let h = Harness::new().await;
    let item = classification_item(&h, "review").await;
    assert!(snapshot_dir(&h, &item).await.is_dir());
    std::fs::rename(
        h.repo
            .join(".archon/pipelines")
            .join(CLASSIFICATION_PIPELINE),
        h.dir.path().join("removed-pipeline"),
    )
    .unwrap();
    let report = evidence(&h).await;
    assert_eq!(report.rows.len(), 1);
    let row = &report.rows[0];
    assert_eq!(
        (row.attempts, row.completed, row.passed, row.verdicts),
        (2, 2, 2, 2)
    );
    assert_eq!(row.median_cost_usd, Some(0.5));
    assert_eq!(row.median_minutes, Some(3.0));
}

#[tokio::test]
async fn frozen_kinds_are_per_run_and_override_changed_catalog_kinds() {
    let h = Harness::new().await;
    let review = classification_item(&h, "review").await;
    let work = classification_item(&h, "work").await;
    assert_ne!(h.run(&review.id).await.id, h.run(&work.id).await.id);
    assert!(snapshot_dir(&h, &review).await.is_dir());
    assert!(snapshot_dir(&h, &work).await.is_dir());
    let report = evidence(&h).await;
    let row = report
        .rows
        .iter()
        .find(|r| r.stage == "historical-check")
        .unwrap();
    assert_eq!((row.attempts, row.completed), (3, 3));
    assert_eq!((row.passed, row.verdicts), (2, 2));
}

#[tokio::test]
async fn broken_frozen_spec_falls_back_to_current_catalog() {
    let h = Harness::new().await;
    let item = classification_item(&h, "review").await;
    std::fs::write(
        snapshot_dir(&h, &item).await.join("pipeline.toml"),
        "broken [",
    )
    .unwrap();
    let report = evidence(&h).await;
    assert_eq!(report.rows.len(), 1);
    assert_eq!((report.rows[0].passed, report.rows[0].verdicts), (2, 2));
}

#[tokio::test]
async fn unresolved_current_definitions_retain_observations_without_verdicts() {
    for unavailable in [
        "manifest-removed",
        "pipeline-broken",
        "config-broken",
        "catalog-unreadable",
        "repo-unregistered",
    ] {
        let h = Harness::new().await;
        let item = classification_item(&h, "review").await;
        h.dispatch().await;
        h.settle_running(|_| completed("done")).await;
        assert!(h.state(&item.id).await.is_terminal());
        assert!(!snapshot_dir(&h, &item).await.exists());
        let manifest = h
            .repo
            .join(".archon/pipelines")
            .join(CLASSIFICATION_PIPELINE)
            .join("pipeline.toml");
        match unavailable {
            "manifest-removed" => {
                std::fs::rename(manifest, h.dir.path().join("removed-manifest")).unwrap()
            }
            "pipeline-broken" => std::fs::write(manifest, "broken [").unwrap(),
            "config-broken" => h.repo_config("broken ["),
            "catalog-unreadable" => {
                let catalog = h.repo.join(".archon/pipelines");
                std::fs::rename(&catalog, h.dir.path().join("removed-catalog")).unwrap();
                std::fs::write(catalog, "a file, not a directory").unwrap();
            }
            "repo-unregistered" => h
                .ctx
                .store
                .lock()
                .await
                .remove_repo(item.repo.as_ref().unwrap())
                .unwrap(),
            _ => unreachable!(),
        }
        let report = evidence(&h).await;
        let row = report
            .rows
            .iter()
            .find(|r| r.stage == "historical-check")
            .unwrap();
        assert_eq!((row.attempts, row.completed), (2, 2), "{unavailable}");
        assert_eq!(row.median_cost_usd, Some(0.5), "{unavailable}");
        assert_eq!(row.median_minutes, Some(3.0), "{unavailable}");
        assert_eq!((row.passed, row.verdicts), (0, 0), "{unavailable}");
    }
}

#[tokio::test]
async fn unknown_kinds_do_not_hide_store_validation_failures() {
    let h = Harness::new().await;
    let item = classification_item(&h, "review").await;
    h.dispatch().await;
    h.settle_running(|_| completed("done")).await;
    std::fs::rename(
        h.repo
            .join(".archon/pipelines")
            .join(CLASSIFICATION_PIPELINE),
        h.dir.path().join("removed-pipeline"),
    )
    .unwrap();
    let mut stage = h.stages(&item.id).await.remove(0);
    stage.finished_at = Some(stage.started_at - chrono::Duration::minutes(1));
    h.ctx.store.lock().await.upsert_stage_run(&stage).unwrap();
    let error = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery::default()))
        .await
        .unwrap_err();
    assert!(error.is_store(), "{error}");
    assert!(
        error.to_string().contains("finishes before it starts"),
        "{error}"
    );
}

/// Aether 1.0.9 books Codex usage as an estimate beside a zero cost. Budgets
/// that leave estimates out see $0, but evidence and learning never do.
#[tokio::test]
async fn evidence_and_outcomes_count_codex_estimates_when_budgets_do_not() {
    use archon_core::cost::CostBasis;
    use archon_queue::store::SpendRow;
    let mut config = archon_core::config::Config::default();
    config.limits.count_estimated_cost = false;
    let h = Harness::with_config(config).await;
    let item = h.enqueue("codex estimate").await;
    h.dispatch().await;
    let agent = h.running().await[0].aether_agent.clone().expect("agent");
    let codex = SpendRow {
        agent,
        item: Some(item.id.clone()),
        cost_usd: 0.0,
        cost_basis: CostBasis::ListPrice,
        cost_complete: Some(false),
        estimated_cost_usd: 0.75,
        input_tokens: 10,
        output_tokens: 5,
        at: archon_core::now(),
    };
    h.ctx.store.lock().await.record_spend(&codex).unwrap();
    h.settle_running(|_| completed("Applied supplied fix"))
        .await;
    h.dispatch().await;
    h.settle_running(|_| completed("VERDICT: PASS")).await;
    let reply = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery::default()))
        .await
        .unwrap();
    let Reply::ModelEvidence(report) = reply else {
        panic!("evidence reply");
    };
    assert!(report.estimates_counted);
    let work = report.rows.iter().find(|r| r.stage == "implement").unwrap();
    assert_eq!(work.median_cost_usd, Some(0.75));
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("learn from it now".into()),
    };
    h.cli(cancel).await.unwrap();
    let run = h.stages(&item.id).await[0].run.clone();
    let store = h.ctx.store.lock().await;
    let outcome = store.outcome(&run).unwrap().expect("captured at finish");
    assert_eq!((outcome.cost_usd, outcome.count_estimates), (0.75, true));
    assert_eq!(store.spend_for_item(&item.id, false).unwrap(), 0.0);
}
