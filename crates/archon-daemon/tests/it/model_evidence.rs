//! Read-only evidence tolerates unavailable definitions and prefers run snapshots.
use crate::common;

use archon_core::model_evidence::ModelEvidenceQuery;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::QueueItem;
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

/// The report's pipeline lines cover its range and its pipeline filter, while
/// learning keeps every run.
#[tokio::test]
async fn pipeline_lines_cover_the_report_s_range_and_pipeline() {
    use archon_core::cost::DayBound;
    let h = Harness::new().await;
    let item = h.enqueue("a run in range").await;
    h.dispatch().await;
    h.settle_running(|_| completed("Applied supplied fix"))
        .await;
    h.dispatch().await;
    h.settle_running(|_| completed("VERDICT: PASS")).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("settle it in range".into()),
    };
    h.cli(cancel).await.unwrap();
    assert!(h.state(&item.id).await.is_terminal());
    retired_run(&h, &item).await;
    let lines = |pipeline: Option<&str>, since: Option<DayBound>| {
        let query = ModelEvidenceQuery {
            pipeline: pipeline.map(Into::into),
            since,
            ..Default::default()
        };
        let h = &h;
        async move {
            let Reply::ModelEvidence(report) = h.cli(Request::ModelEvidence(query)).await.unwrap()
            else {
                panic!("evidence reply");
            };
            let names: Vec<String> = report
                .pipelines
                .iter()
                .map(|l| l.pipeline.clone())
                .collect();
            names
        }
    };
    assert_eq!(
        lines(None, None).await,
        [PIPELINE],
        "no line without a run in range"
    );
    assert_eq!(
        lines(None, Some(DayBound::Days(90))).await,
        ["retired", PIPELINE]
    );
    let retired = lines(Some("retired"), Some(DayBound::Days(90))).await;
    assert_eq!(retired, ["retired"], "the pipeline filter");
    assert!(lines(Some("retired"), None).await.is_empty());
    let stats = h.ctx.store.lock().await.learning_stats(None).unwrap();
    assert_eq!(stats.pipelines.len(), 2, "learning keeps the lifetime");
}

/// Records a run of `like`'s item under the pipeline `retired`, finished 60
/// days ago, with its outcome.
async fn retired_run(h: &Harness, like: &archon_core::queue::QueueItem) {
    use archon_core::queue::QueueState;
    use archon_core::run::{PipelineRun, RunState};
    let finished = archon_core::now() - chrono::Duration::days(60);
    let mut store = h.ctx.store.lock().await;
    let mut item = store.item(&like.id).unwrap();
    item.id = archon_core::id::QueueItemId::new();
    item.model_plan = item.model_plan.copy_for(&like.id, &item.id, None);
    item.pipeline = "retired".into();
    item.state = QueueState::Queued;
    let item = store.insert_item(&item, None).unwrap();
    store
        .transition_item(&item.id, QueueState::Dispatching, finished)
        .unwrap();
    let template = store.runs_for_item(&like.id).unwrap().remove(0);
    let run = PipelineRun {
        id: archon_core::id::PipelineRunId::new(),
        item: item.id.clone(),
        pipeline: "retired".into(),
        state: RunState::Running {
            stage_id: "implement".into(),
            attempt: 1,
        },
        started_at: finished - chrono::Duration::minutes(5),
        finished_at: None,
        ..template
    };
    store.insert_run(&run).unwrap();
    let running = QueueState::Running {
        run: run.id.clone(),
    };
    store.transition_item(&item.id, running, finished).unwrap();
    let summary = "done".to_owned();
    let done = PipelineRun {
        state: RunState::Succeeded {
            summary: summary.clone(),
        },
        finished_at: Some(finished),
        ..run
    };
    let next = QueueState::Succeeded {
        run: done.id.clone(),
        summary,
    };
    store.finish_run(&done, next, finished).unwrap();
    store
        .capture_outcome(&store.run(&done.id).unwrap(), None, true)
        .unwrap();
    assert!(
        store.outcome(&done.id).unwrap().is_some(),
        "an outcome 60 days old"
    );
}

/// A review-only run's lead records a fail with `complete_task`: the stage
/// completes, the run and item fail, nothing ships, and evidence scores it.
#[tokio::test]
async fn a_review_only_lead_s_recorded_fail_fails_the_item_and_counts_as_a_verdict() {
    use archon_core::model::{Effort, ModelPlan};
    use archon_core::queue::QueueState;
    use common::{MODEL, choice};
    let h = Harness::new().await;
    h.add_pipeline(
        "review-only",
        r#"[pipeline]
name = "review-only"
description = "review a change, ship nothing"
version = 1
workspace = "worktree"
requires_repo = true
[pipeline.review]
level = "standard"
stage = "review"
[[stage]]
id = "review"
kind = "work"
prompt = "implement.md"
allow_delegation = true
"#,
    );
    let plan = ModelPlan([("review".into(), vec![choice(MODEL, Effort::Low)])].into());
    let item = h
        .enqueue_in("review-only", "review it", plan)
        .await
        .unwrap();
    h.dispatch().await;
    let fail = archon_core::run::StageOutcome::Completed {
        summary: "Reviewed.\nVERDICT: FAIL".into(),
        structured: Some(serde_json::json!({
            "verdict": "fail",
            "findings": [{"severity": "high", "blocking": true, "title": "t", "detail": "d"}],
        })),
    };
    assert_eq!(h.settle_running(|_| fail.clone()).await.len(), 1);
    let lead = h.stages(&item.id).await.remove(0);
    assert_eq!(lead.state, StageState::Completed);
    assert_eq!(
        (lead.verdict, lead.only_reviews),
        (Some(Verdict::Fail), Some(true))
    );
    let QueueState::Failed { error, .. } = h.item(&item.id).await.state else {
        panic!("a recorded fail fails the item");
    };
    assert!(error.contains("FAIL"), "{error}");
    assert_eq!(h.stages(&item.id).await.len(), 1, "nothing ran after it");
    let report = evidence(&h).await;
    let row = report.rows.iter().find(|r| r.stage == "review").unwrap();
    assert_eq!((row.completed, row.passed, row.verdicts), (1, 0, 1));
}

type AgentId = archon_core::aether_ids::AetherAgentId;

/// Charge `agent` a running total of `usd` with Aether event `seq`; a
/// `settled` total is the provider's final one and may lower the charge.
async fn usage(h: &Harness, agent: &AgentId, usd: f64, seq: u64, settled: bool) {
    let mut total = serde_json::json!({"cost_usd": usd, "cost_basis": "provider_reported"});
    if settled {
        total["cost_complete"] = serde_json::json!(true);
    }
    let mut wire = archon_aether::fake::event(
        Some(agent.as_str()),
        None,
        serde_json::json!({"type": "agent.usage_updated", "total": total}),
    );
    wire["seq"] = serde_json::json!(seq);
    wire["at"] = serde_json::json!(archon_core::now());
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
}

/// `lead` delegated to `child` at Aether event `seq`, as the watchdog records it.
async fn delegated(h: &Harness, child: &AgentId, lead: &AgentId, seq: u64) {
    let model = common::MODEL.parse().unwrap();
    let mut store = h.ctx.store.lock().await;
    store
        .record_delegation_event(child, lead, &model, seq)
        .unwrap();
}

/// An item whose lead delegated to `early`, charged `early_usd` before a failed
/// review kept the lead for a fix after Aether event 20; the item and lead.
async fn kept_for_a_fix(h: &Harness, early: &AgentId, early_usd: f64) -> (QueueItem, AgentId) {
    let item = h.enqueue("a lead that delegates, then a fix").await;
    h.dispatch().await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    delegated(h, early, &lead, 5).await;
    usage(h, &lead, 0.25, 6, false).await;
    usage(h, early, early_usd, 7, false).await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|s| common::verdict(s.fan_index != 0))
        .await;
    h.backend.set_event_watermark(20);
    h.dispatch().await;
    let fix = h.running().await.remove(0);
    assert_eq!((fix.attempt, fix.aether_agent.as_ref()), (2, Some(&lead)));
    (item, lead)
}

/// The stage view of each implement attempt: its cost and its delegates' charges.
async fn implement_costs(h: &Harness, item: &QueueItem) -> Vec<(u32, f64, Vec<(AgentId, f64)>)> {
    let query = archon_core::protocol::StageQuery {
        item: item.id.clone(),
        run: None,
        stage: None,
        attempt: None,
        slot: None,
        offset: 0,
        limit: None,
    };
    let Reply::StageDetail(detail) = h.cli(Request::GetStage(query)).await.unwrap() else {
        panic!("stage detail");
    };
    let implement = detail.stages.iter().filter(|s| s.stage_id == "implement");
    implement
        .map(|s| {
            let agents = s.delegation.iter().flat_map(|split| &split.agents);
            let agents = agents.map(|a| (a.agent.clone(), a.cost_usd)).collect();
            (s.attempt, s.cost_usd, agents)
        })
        .collect()
}

/// Model evidence's two completed implement attempts, dearer first: with two,
/// p90 is the dearer and the median their mean.
async fn evidence_costs(h: &Harness) -> (f64, f64) {
    let report = evidence(h).await;
    let row = report.rows.iter().find(|r| r.stage == "implement").unwrap();
    assert_eq!(row.completed, 2, "{row:?}");
    let dearer = row.p90_cost_usd.unwrap();
    (dearer, 2.0 * row.median_cost_usd.unwrap() - dearer)
}

/// A delegate's usage that lands after its lead was kept for a fix stays with
/// the attempt that delegated it, in the stage view as in model evidence.
#[tokio::test]
async fn late_delegate_usage_stays_with_its_attempt_in_both_views() {
    let h = Harness::new().await;
    let parse = |raw| AgentId::parse(raw).unwrap();
    let (early, late) = (parse("agt_early"), parse("agt_late"));
    let (item, lead) = kept_for_a_fix(&h, &early, 0.5).await;
    // The early delegate reports after the hand-off; a new one works for the fix.
    usage(&h, &early, 1.5, 21, false).await;
    usage(&h, &lead, 1.0, 22, false).await;
    delegated(&h, &late, &lead, 23).await;
    usage(&h, &late, 0.125, 24, false).await;
    h.settle_running(|_| completed("fixed")).await;

    let expected = [
        (1, 1.75, vec![(early.clone(), 1.5)]),
        (2, 0.875, vec![(late.clone(), 0.125)]),
    ];
    let viewed = implement_costs(&h, &item).await;
    assert_eq!(
        viewed, expected,
        "each delegate on the attempt that made it"
    );
    assert_eq!(evidence_costs(&h).await, (1.75, 0.875));
    let spent = h.ctx.store.lock().await.spend_for_item(&item.id, false);
    assert_eq!(spent.unwrap(), 2.625, "the item total is unchanged");
    let unlinked = h.ctx.store.lock().await.unlinked_spend().unwrap();
    assert!(unlinked.is_empty(), "{unlinked:?}");
}

/// A delegate's settled total that lands below its charge at the hand-off
/// refunds its own attempt: the stage costs what the ledger holds, in both views.
#[tokio::test]
async fn a_settled_refund_after_a_hand_off_lowers_the_delegates_attempt_in_both_views() {
    let h = Harness::new().await;
    let early = AgentId::parse("agt_early").unwrap();
    let (item, lead) = kept_for_a_fix(&h, &early, 1.0).await;
    usage(&h, &early, 0.5, 21, true).await;
    usage(&h, &lead, 1.0, 22, false).await;
    h.settle_running(|_| completed("fixed")).await;

    let viewed = implement_costs(&h, &item).await;
    let expected = [(1, 0.75, vec![(early.clone(), 0.5)]), (2, 0.75, vec![])];
    assert_eq!(viewed, expected, "the refund lowers the attempt it charged");
    assert_eq!(evidence_costs(&h).await, (0.75, 0.75));
    let spent = h.ctx.store.lock().await.spend_for_item(&item.id, false);
    let stage_total: f64 = viewed.iter().map(|(_, cost, _)| cost).sum();
    assert_eq!(
        spent.unwrap(),
        stage_total,
        "the stage costs what the ledger holds"
    );
}
