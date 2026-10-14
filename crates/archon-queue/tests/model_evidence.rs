//! Observations use stored attempts, ledger costs and local start days.
mod common;

use archon_core::cost::DayRange;
use archon_core::model::Effort;
use archon_core::model_evidence::{MAX_ROWS, ModelEvidenceQuery, ModelEvidenceReport};
use archon_core::protocol::RepoRef;
use archon_core::run::{PipelineRun, StageState, Verdict};
use archon_queue::{Store, store::SpendRow};
use common::{item, repo, running_stage, start, store, t};

fn range(from: i64, to: i64) -> DayRange {
    let day = |secs| t(secs).with_timezone(&chrono::Local).date_naive();
    DayRange {
        since: day(from),
        until: day(to),
    }
}

fn attempt(
    store: &mut Store,
    run: &PipelineRun,
    n: u32,
    name: &str,
    cost: f64,
    verdict: Option<Verdict>,
    state: StageState,
) {
    let mut stage = running_stage(run, n, n);
    stage.stage_id = name.into();
    stage.attempt = n + 1;
    stage.started_at = t(100);
    stage.finished_at = state.is_terminal().then(|| t(100 + i64::from(n + 1) * 60));
    stage.state = state;
    stage.cost_usd = cost;
    stage.verdict = verdict;
    store.upsert_stage_run(&stage).unwrap();
}

fn report(store: &Store, query: ModelEvidenceQuery, range: DayRange) -> ModelEvidenceReport {
    store
        .model_evidence(&query, &range, true, |_, stage| {
            Ok(stage == "review" || stage == "test")
        })
        .unwrap()
}

#[test]
fn groups_by_stage_model_effort_and_measures_completed_attempts_only() {
    let mut store = store();
    let item = store.insert_item(&item(None, "fixture"), None).unwrap();
    let run = start(&mut store, &item.id);
    for (n, cost) in [1., 2., 4., 8., 20.].into_iter().enumerate() {
        attempt(
            &mut store,
            &run,
            n as u32,
            "review",
            cost,
            Some([Verdict::Pass, Verdict::Fail, Verdict::Unknown][n % 3]),
            StageState::Completed,
        );
    }
    attempt(
        &mut store,
        &run,
        5,
        "review",
        500.,
        None,
        StageState::Failed {
            error: "failed".into(),
        },
    );
    attempt(
        &mut store,
        &run,
        6,
        "review",
        500.,
        None,
        StageState::Running,
    );
    attempt(
        &mut store,
        &run,
        7,
        "implement",
        0.5,
        Some(Verdict::Unknown),
        StageState::Completed,
    );
    let mut cheaper = running_stage(&run, 8, 8);
    cheaper.stage_id = "review".into();
    cheaper.choice.model = "codex/cheap".parse().unwrap();
    cheaper.choice.effort = Effort::Low;
    cheaper.state = StageState::Completed;
    cheaper.finished_at = Some(t(80));
    cheaper.cost_usd = 1.;
    store.upsert_stage_run(&cheaper).unwrap();
    let mut other_effort = running_stage(&run, 9, 9);
    other_effort.stage_id = "review".into();
    other_effort.choice.effort = Effort::Low;
    other_effort.state = StageState::Completed;
    other_effort.finished_at = Some(t(80));
    other_effort.cost_usd = 3.;
    store.upsert_stage_run(&other_effort).unwrap();
    let mut no_completions = running_stage(&run, 10, 10);
    no_completions.stage_id = "test".into();
    store.upsert_stage_run(&no_completions).unwrap();
    let report = report(&store, Default::default(), range(0, 0));
    assert_eq!(report.rows.len(), 5);
    assert_eq!(report.rows[0].stage, "implement");
    assert_eq!(report.rows[0].verdicts, 0);
    assert_eq!(report.rows[1].median_cost_usd, Some(1.));
    assert_eq!(report.rows[2].median_cost_usd, Some(3.));
    let review = &report.rows[3];
    assert_eq!((review.attempts, review.completed), (7, 5));
    assert_eq!(
        (
            review.median_cost_usd,
            review.p90_cost_usd,
            review.median_minutes
        ),
        (Some(4.), Some(20.), Some(3.))
    );
    assert_eq!((review.passed, review.verdicts), (2, 5));
    assert_eq!(report.rows[4].median_cost_usd, None);
    assert_eq!(report.rows[4].median_minutes, None);
    let text = report.text();
    assert!(
        text.contains("n=7 completed=5 median=$4.0000 p90=$20.0000 median_min=3.00 pass=2/5 (40%)"),
        "{text}"
    );
    assert_eq!(text.matches("small sample").count(), 4);
}

#[test]
fn median_averages_middle_values_p90_uses_nearest_rank_and_costs_use_the_ledger() {
    let mut store = store();
    let item = store.insert_item(&item(None, "fixture"), None).unwrap();
    let run = start(&mut store, &item.id);
    for n in 0..10 {
        attempt(
            &mut store,
            &run,
            n,
            "test",
            999.,
            Some(Verdict::Pass),
            StageState::Completed,
        );
        store
            .record_spend(&SpendRow {
                agent: common::agent(n),
                item: Some(item.id.clone()),
                cost_usd: f64::from(n + 1),
                cost_basis: archon_core::cost::CostBasis::ProviderReported,
                cost_complete: None,
                estimated_cost_usd: 1.,
                input_tokens: 1,
                output_tokens: 1,
                at: t(200),
            })
            .unwrap();
    }
    let query = ModelEvidenceQuery::default();
    let with = report(&store, query.clone(), range(0, 0));
    let without = store
        .model_evidence(&query, &range(0, 0), false, |_, _| Ok(true))
        .unwrap();
    assert_eq!(
        (with.rows[0].median_cost_usd, with.rows[0].p90_cost_usd),
        (Some(6.5), Some(10.))
    );
    assert_eq!(
        (
            without.rows[0].median_cost_usd,
            without.rows[0].p90_cost_usd
        ),
        (Some(5.5), Some(9.))
    );
    assert_eq!(with.rows[0].median_minutes, Some(5.5));
    assert!(!with.text().contains("small sample"));
}

fn spend(store: &mut Store, item: &archon_core::id::QueueItemId, n: u32, cost: f64, estimate: f64) {
    let row = SpendRow {
        agent: common::agent(n),
        item: Some(item.clone()),
        cost_usd: cost,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: estimate,
        input_tokens: 1,
        output_tokens: 1,
        at: t(200),
    };
    store.record_spend(&row).unwrap();
}

/// A lead that delegates is not a cheap lead: its stage costs what it and
/// every agent beneath it were charged, each counted for one stage only.
#[test]
fn a_stage_cost_includes_what_the_agents_it_delegated_to_were_charged() {
    let mut store = store();
    let item = store.insert_item(&item(None, "fixture"), None).unwrap();
    let run = start(&mut store, &item.id);
    let model = "codex/helper".parse().unwrap();
    for n in [0, 1] {
        let (state, verdict) = (StageState::Completed, Some(Verdict::Pass));
        attempt(&mut store, &run, n, "build", 999., verdict, state);
    }
    // Stage agent 0 delegated to 10, which delegated to 11; agent 1 to nobody.
    let (lead, helper, below) = (common::agent(0), common::agent(10), common::agent(11));
    store
        .record_delegated(&helper, &lead, &model, None)
        .unwrap();
    store
        .record_delegated(&below, &helper, &model, Some(Effort::Low))
        .unwrap();
    // As the watchdog does for a child of an agent it knows.
    store.link_agent_run(&helper, &run.id).unwrap();
    store.link_agent_run(&below, &run.id).unwrap();
    for (n, cost, estimate) in [(0, 2., 0.5), (10, 3., 0.25), (11, 4., 0.25), (1, 1., 0.)] {
        spend(&mut store, &item.id, n, cost, estimate);
    }

    let query = ModelEvidenceQuery::default();
    let with = report(&store, query.clone(), range(0, 0));
    assert_eq!(with.rows.len(), 1, "{with:?}");
    // 2 + 3 + 4 + 0.5 + 0.25 + 0.25 = 10 for the lead, 1 for the other.
    assert_eq!(with.rows[0].median_cost_usd, Some(5.5));
    assert_eq!(with.rows[0].p90_cost_usd, Some(10.));
    let billed = store
        .model_evidence(&query, &range(0, 0), false, |_, _| Ok(true))
        .unwrap();
    assert_eq!(billed.rows[0].p90_cost_usd, Some(9.));

    let beneath = store.delegated_beneath(&lead, true).unwrap();
    let seen: Vec<_> = beneath
        .iter()
        .map(|d| (d.agent.clone(), d.parent.clone(), d.effort, d.cost_usd))
        .collect();
    assert_eq!(
        seen,
        [
            (helper.clone(), lead.clone(), None, 3.25),
            (below.clone(), helper.clone(), Some(Effort::Low), 4.25),
        ]
    );
    assert_eq!(store.delegated_beneath(&helper, false).unwrap().len(), 1);
    assert!(
        store
            .delegated_beneath(&common::agent(1), true)
            .unwrap()
            .is_empty()
    );
    let run_total = store.spend_for_run(&run.id, true).unwrap();
    assert_eq!(run_total, 11., "the run counts each agent once");
}

/// An effort read later completes the record; one already known is kept, and
/// an agent keeps the parent it was first given.
#[test]
fn a_delegated_agent_keeps_its_parent_and_gains_its_effort() {
    let mut store = store();
    let (lead, helper) = (common::agent(0), common::agent(10));
    let model = "codex/helper".parse().unwrap();
    store
        .record_delegated(&helper, &lead, &model, None)
        .unwrap();
    store
        .record_delegated(&helper, &lead, &model, Some(Effort::High))
        .unwrap();
    store
        .record_delegated(&helper, &lead, &model, None)
        .unwrap();
    let beneath = store.delegated_beneath(&lead, true).unwrap();
    assert_eq!(beneath.len(), 1);
    assert_eq!(beneath[0].effort, Some(Effort::High));
    assert_eq!(beneath[0].cost_usd, 0., "no usage yet");

    let other = common::agent(1);
    let refused = store
        .record_delegated(&helper, &other, &model, None)
        .unwrap_err();
    assert!(
        refused.to_string().contains("already delegated to by"),
        "{refused}"
    );
    assert!(store.delegated_beneath(&other, true).unwrap().is_empty());
}

#[test]
fn filters_by_repo_pipeline_stage_and_since_without_counting_other_attempts() {
    let mut store = store();
    let alpha = repo("alpha");
    let beta = repo("beta");
    store.insert_repo(&alpha).unwrap();
    store.insert_repo(&beta).unwrap();
    for (n, owner, pipeline, started) in [
        (20, Some(&alpha), "direct", 100),
        (21, Some(&beta), "direct", 100),
        (22, Some(&alpha), "ask", 100),
        (23, Some(&alpha), "direct", -86_400),
        (24, None, "direct", 100),
    ] {
        let mut item = item(owner, "fixture");
        item.pipeline = pipeline.into();
        let item = store.insert_item(&item, None).unwrap();
        let run = start(&mut store, &item.id);
        let mut stage = running_stage(&run, 0, n);
        stage.started_at = t(started);
        store.upsert_stage_run(&stage).unwrap();
    }
    let query = ModelEvidenceQuery {
        repo: Some(RepoRef("alpha".into())),
        pipeline: Some("direct".into()),
        stage: Some("implement".into()),
        since: Some(archon_core::cost::DayBound::Days(1)),
    };
    let days = query.range(range(0, 0).until).unwrap();
    let selected = report(&store, query.clone(), days);
    assert_eq!(selected.rows[0].attempts, 1);
    let mut by_id = query.clone();
    by_id.repo = Some(RepoRef(alpha.id.to_string()));
    assert_eq!(report(&store, by_id, days), selected);
    let mut absent = query;
    absent.stage = Some("no-such-stage".into());
    assert!(report(&store, absent, days).rows.is_empty());
    assert_eq!(
        report(&store, Default::default(), range(-86_400, 0)).rows[0].attempts,
        5
    );
    assert!(
        store
            .model_evidence(
                &ModelEvidenceQuery {
                    repo: Some(RepoRef("missing".into())),
                    ..Default::default()
                },
                &days,
                true,
                |_, _| Ok(false)
            )
            .is_err()
    );
}

#[test]
fn bounded_output_reports_omitted_groups_and_empty_history() {
    let mut store = store();
    assert!(
        report(&store, Default::default(), range(0, 0))
            .text()
            .contains("No stage attempts")
    );
    let item = store.insert_item(&item(None, "fixture"), None).unwrap();
    let run = start(&mut store, &item.id);
    for n in 0..MAX_ROWS + 3 {
        attempt(
            &mut store,
            &run,
            n as u32,
            &format!("stage-{n:03}"),
            1.,
            None,
            StageState::Completed,
        );
    }
    let report = report(&store, Default::default(), range(0, 0));
    assert_eq!(report.rows.len(), MAX_ROWS);
    assert_eq!(report.omitted, 3);
    assert!(report.text().contains("Truncated: 3 more groups omitted."));
    assert!(report.rows.iter().all(|row| row.attempts == 1));
}

#[test]
fn snapshot_owns_attempts_and_can_be_classified_after_the_store_is_dropped() {
    let mut store = store();
    let item = store.insert_item(&item(None, "fixture"), None).unwrap();
    let run = start(&mut store, &item.id);
    attempt(
        &mut store,
        &run,
        0,
        "custom-check",
        0.5,
        Some(Verdict::Pass),
        StageState::Completed,
    );
    let snapshot = store
        .model_evidence_snapshot(&Default::default(), &range(0, 0), true)
        .unwrap();
    drop(store);
    let (owner, pipeline, stage) = snapshot.attempts().next().unwrap();
    assert_eq!(owner.id, item.id);
    assert_eq!(pipeline, run.pipeline);
    assert_eq!(stage.run, run.id);
    let report = snapshot.report(|_, stage| Ok(stage.run == run.id)).unwrap();
    assert_eq!(report.rows.len(), 1);
    let row = &report.rows[0];
    assert_eq!(
        (row.attempts, row.completed, row.passed, row.verdicts),
        (1, 1, 1, 1)
    );
    assert_eq!(row.median_cost_usd, Some(0.5));
}
