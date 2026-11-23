//! Bounded historical import and finish-time capture from the existing ledger.

use super::super::Store;
use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::outcome::{
    ClassSource, DiffStat, FailureClass, Outcome, OutcomeResult, OutcomeStage,
};
use archon_core::queue::QueueItem;
use archon_core::run::{PipelineRun, RunState, StageRun, Verdict};
use std::collections::{BTreeMap, BTreeSet};

impl Store {
    pub fn capture_outcome(
        &self,
        run: &PipelineRun,
        diff: Option<DiffStat>,
        estimates: bool,
    ) -> Result<(), ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let item = self.item(&run.item)?;
        if item.tags.iter().any(|t| t == "archon:distillation")
            || self
                .kv_get(&format!("learning/distillation/{}", item.id))?
                .is_some()
        {
            return Ok(());
        }
        let finished = run
            .finished_at
            .ok_or_else(|| ArchonError::Invalid("outcome requires a finished run".into()))?;
        let stages = self.stage_runs(&run.id)?;
        let failed_reviews = super::class::failed_reviews(&self.conn, &stages)?;
        let (result, failure) = result(run, &stages, &failed_reviews)?;
        assert!(run.state.is_terminal());
        assert_eq!(item.id, run.item);
        let observations = stage_observations(self, &stages, estimates)?;
        let review_pr = reviewed_pr(&item, &stages, &failed_reviews);
        let total = self
            .spend_for_run(&run.id, estimates)?
            .max(run.cost_usd)
            .max(observations.iter().map(|s| s.cost_usd).sum());
        let outcome = Outcome {
            run: run.id.clone(),
            item: run.item.clone(),
            repo: item.repo,
            pipeline: run.pipeline.clone(),
            diff: diff.or_else(|| reported_diff(&stages)),
            review_rounds: stages
                .iter()
                .filter(|s| matches!(s.verdict, Some(Verdict::Pass | Verdict::Fail)))
                .map(|s| s.attempt)
                .max()
                .unwrap_or(0),
            stages: observations,
            result,
            failure,
            failure_source: failure.map(|_| ClassSource::Structure),
            review_pr,
            corrections: historical_corrections(self, run)?,
            feedback: Vec::new(),
            context: None,
            merged: None,
            cost_usd: total,
            cost_provisional: true,
            count_estimates: estimates,
            wall_secs: elapsed(run.started_at, finished)?,
            finished_at: finished,
        };
        self.record_outcome(&outcome)
    }
}

pub(super) fn stage_observations(
    store: &Store,
    stages: &[StageRun],
    estimates: bool,
) -> Result<Vec<OutcomeStage>, ArchonError> {
    let mut grouped: BTreeMap<(String, String, String), (OutcomeStage, u32)> = BTreeMap::new();
    let mut charged = BTreeSet::new();
    for stage in stages {
        assert!(stage.attempt > 0);
        let cost = if stage
            .aether_agent
            .as_ref()
            .is_none_or(|a| charged.insert(a.clone()))
        {
            stage_charge(store, stage, estimates)?
        } else {
            0.0
        };
        if stage.zero_cost_create_refusal() {
            continue;
        }
        for observed in super::segments::observations(stage, cost)? {
            merge_observation(&mut grouped, observed, stage.attempt);
        }
    }
    assert!(grouped.len() <= stages.iter().map(|s| 1 + s.model_segments.len()).sum());
    Ok(grouped.into_values().map(|(stage, _)| stage).collect())
}

fn merge_observation(
    grouped: &mut BTreeMap<(String, String, String), (OutcomeStage, u32)>,
    incoming: OutcomeStage,
    attempt: u32,
) {
    assert!(attempt > 0);
    assert!(incoming.cost_usd.is_finite() && incoming.cost_usd >= 0.0);
    let key = (
        incoming.stage.clone(),
        incoming.choice.model.to_string(),
        incoming.choice.effort.to_string(),
    );
    let (observed, last_attempt) = grouped.entry(key).or_insert_with(|| {
        (
            OutcomeStage {
                cost_usd: 0.0,
                wall_secs: 0,
                attempts: 0,
                ..incoming.clone()
            },
            attempt,
        )
    });
    observed.cost_usd += incoming.cost_usd;
    observed.wall_secs += incoming.wall_secs;
    observed.attempts += 1;
    if attempt > *last_attempt {
        observed.passed = incoming.passed;
        *last_attempt = attempt;
    } else if incoming.passed.is_some() {
        observed.passed = Some(observed.passed != Some(false) && incoming.passed == Some(true));
    }
}

fn elapsed(start: archon_core::Timestamp, end: archon_core::Timestamp) -> Result<u64, ArchonError> {
    u64::try_from((end - start).num_seconds())
        .map_err(|_| ArchonError::Store("outcome finishes before it starts".into()))
}

fn result(
    run: &PipelineRun,
    stages: &[StageRun],
    failed_reviews: &BTreeSet<StageRunId>,
) -> Result<(OutcomeResult, Option<FailureClass>), ArchonError> {
    match &run.state {
        RunState::Succeeded { .. } => Ok((OutcomeResult::Succeeded, None)),
        RunState::Cancelled { .. } => Ok((OutcomeResult::Cancelled, None)),
        RunState::Failed { .. } => Ok((
            OutcomeResult::Failed,
            Some(super::class::failure_class(run, stages, failed_reviews)),
        )),
        _ => Err(ArchonError::Invalid("a live run has no outcome".into())),
    }
}

/// The pull request the run reviewed: its item's target when the item pushes
/// nothing and a reviewer's verdict is on record for one of the run's stages.
/// The target alone, or a stage assigned the review, is no review.
fn reviewed_pr(
    item: &QueueItem,
    stages: &[StageRun],
    failed_reviews: &BTreeSet<StageRunId>,
) -> Option<String> {
    let onto = item.onto.as_ref().filter(|onto| onto.review_only)?;
    assert!(
        failed_reviews
            .iter()
            .all(|id| stages.iter().any(|s| &s.id == id)),
        "a tally is of one of the run's stages"
    );
    let reviewed = |stage: &StageRun| super::class::review_on_record(stage, failed_reviews);
    stages.iter().any(reviewed).then(|| onto.pr.url())
}

fn reported_diff(stages: &[StageRun]) -> Option<DiffStat> {
    for value in stages
        .iter()
        .rev()
        .filter_map(|s| s.structured.as_ref()?.get("diff_stat"))
    {
        match serde_json::from_value(value.clone()) {
            Ok(diff) => return Some(diff),
            Err(error) => tracing::debug!(%error, "ignoring unstructured reported diff_stat"),
        }
    }
    None
}

pub(super) fn stage_charge(
    store: &Store,
    stage: &StageRun,
    estimates: bool,
) -> Result<f64, ArchonError> {
    let (direct, delegated): (Option<f64>, f64) = store
        .conn
        .query_row(
            "SELECT SUM(CASE WHEN w.root IS NOT NULL
                         THEN x.cost_usd + CASE WHEN ?2 THEN x.estimated_cost_usd ELSE 0 END END),
                    COALESCE(SUM(CASE WHEN w.root IS NULL
                         THEN x.cost_usd + CASE WHEN ?2 THEN x.estimated_cost_usd ELSE 0 END END), 0)
             FROM stage_spend x
             LEFT JOIN stage_spend_windows w ON w.root = x.agent AND w.stage = x.stage
             WHERE x.stage = ?1",
            rusqlite::params![stage.id.as_str(), estimates],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ctx("read outcome stage charge")?;
    assert!(delegated.is_finite() && delegated >= 0.0);
    let total = direct.unwrap_or(stage.cost_usd) + delegated;
    assert!(total.is_finite() && total >= 0.0);
    Ok(total)
}

fn historical_corrections(
    store: &Store,
    run: &PipelineRun,
) -> Result<Vec<archon_core::outcome::Correction>, ArchonError> {
    use archon_core::outcome::Correction;
    use archon_core::run::GateDecision;
    let mut corrections = Vec::new();
    if store.gates_for_run(&run.id)?.iter().any(|g| {
        matches!(
            g.decision,
            Some(GateDecision::Reject { .. } | GateDecision::Revise { .. })
        )
    }) {
        corrections.push(Correction::GateRejected);
    }
    if matches!(run.state, RunState::Succeeded { .. })
        && store
            .runs_for_item(&run.item)?
            .iter()
            .any(|r| r.started_at > run.started_at)
    {
        corrections.push(Correction::RetryAfterDone);
    }
    assert!(corrections.len() <= 2);
    assert!(run.state.is_terminal());
    Ok(corrections)
}
