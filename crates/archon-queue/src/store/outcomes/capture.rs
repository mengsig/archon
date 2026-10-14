//! Bounded historical import and finish-time capture from the existing ledger.

use super::super::Store;
use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::outcome::{DiffStat, FailureClass, Outcome, OutcomeResult, OutcomeStage};
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, Verdict};
use rusqlite::OptionalExtension;
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
        let (result, failure) = result(&run.state)?;
        assert!(run.state.is_terminal());
        assert_eq!(item.id, run.item);
        let observations = stage_observations(self, &stages, estimates)?;
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

fn result(state: &RunState) -> Result<(OutcomeResult, Option<FailureClass>), ArchonError> {
    match state {
        RunState::Succeeded { .. } => Ok((OutcomeResult::Succeeded, None)),
        RunState::Cancelled { .. } => Ok((OutcomeResult::Cancelled, None)),
        RunState::Failed { stage_id, error } => {
            Ok((OutcomeResult::Failed, Some(failure_class(stage_id, error))))
        }
        _ => Err(ArchonError::Invalid("a live run has no outcome".into())),
    }
}

/// What a failed run's error says of the failure. A stage's own word that
/// the task is in the wrong pipeline or needs the operator comes first.
fn failure_class(stage_id: &str, error: &str) -> FailureClass {
    if error.starts_with(StageOutcome::ESCALATE_PREFIX) {
        return FailureClass::Escalated;
    }
    if error.starts_with(StageOutcome::BLOCKED_PREFIX) {
        return FailureClass::Blocked;
    }
    let text = error.to_lowercase();
    if text.contains("budget") {
        FailureClass::Budget
    } else if text.contains("timed out") || text.contains("timeout") {
        FailureClass::Timeout
    } else if text.contains("aether") || text.contains("disk") {
        FailureClass::Infrastructure
    } else if stage_id.contains("review") {
        FailureClass::Review
    } else if stage_id.contains("test") {
        FailureClass::Verification
    } else if stage_id.contains("ship") {
        FailureClass::Ship
    } else {
        FailureClass::Other
    }
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
    let Some(agent) = &stage.aether_agent else {
        return Ok(stage.cost_usd);
    };
    let direct:Option<f64>=store.conn.query_row("SELECT cost_usd + CASE WHEN ?2 THEN estimated_cost_usd ELSE 0 END FROM spend WHERE agent=?1",
        rusqlite::params![agent.as_str(),estimates],|r| r.get(0)).optional().ctx("read outcome stage cost")?;
    let descendants: f64 = store
        .conn
        .query_row(
            "WITH RECURSIVE descendants(agent) AS (
            SELECT agent FROM delegated_agents WHERE parent=?1
            UNION
            SELECT d.agent FROM delegated_agents d JOIN descendants p ON d.parent=p.agent
        ) SELECT COALESCE(SUM(s.cost_usd + CASE WHEN ?2 THEN s.estimated_cost_usd ELSE 0 END),0)
          FROM descendants d JOIN spend s ON s.agent=d.agent WHERE d.agent<>?1",
            rusqlite::params![agent.as_str(), estimates],
            |r| r.get(0),
        )
        .ctx("read outcome descendant cost")?;
    assert!(descendants.is_finite() && descendants >= 0.0);
    let total = direct.unwrap_or(stage.cost_usd) + descendants;
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
