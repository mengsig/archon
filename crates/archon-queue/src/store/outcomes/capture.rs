//! Bounded historical import and finish-time capture from the existing ledger.

use super::super::Store;
use super::super::codec::{SqlContext, from_json};
use archon_core::error::ArchonError;
use archon_core::outcome::{DiffStat, FailureClass, Outcome, OutcomeResult, OutcomeStage};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState, Verdict};
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
        if self
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
            diff: diff.or(reported_diff(&stages)?),
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
            merged: None,
            cost_usd: total,
            wall_secs: elapsed(run.started_at, finished)?,
            finished_at: finished,
        };
        self.record_outcome(&outcome)
    }

    /// Keyset scan: at most `limit` existing runs per background turn, never in migrations.
    pub fn backfill_outcomes(&self, limit: usize, estimates: bool) -> Result<bool, ArchonError> {
        if !self.learning_config()?.enabled || self.kv_get("learning/backfill/done")?.is_some() {
            return Ok(true);
        }
        if !(1..=100).contains(&limit) {
            return Err(ArchonError::Invalid("backfill batch must be 1..100".into()));
        }
        let cursor = self.kv_get("learning/backfill/cursor")?.unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare("SELECT id,data FROM runs WHERE id>?1 ORDER BY id LIMIT ?2")
            .ctx("prepare outcome backfill")?;
        let rows = stmt
            .query_map(rusqlite::params![cursor, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ctx("query outcome backfill")?;
        let mut count = 0;
        for row in rows {
            let (id, raw) = row.ctx("read backfill run")?;
            let run: PipelineRun = from_json(&raw, "backfill run")?;
            assert_eq!(run.id.as_str(), id);
            if run.state.is_terminal() && self.outcome(&run.id)?.is_none() {
                self.capture_outcome(&run, None, estimates)?;
            }
            super::super::settings::kv_set(&self.conn, "learning/backfill/cursor", &id)?;
            count += 1;
        }
        assert!(count <= limit);
        if count < limit {
            super::super::settings::kv_set(&self.conn, "learning/backfill/done", "1")?;
        }
        Ok(count < limit)
    }
}

fn stage_observations(
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
        let key = (
            stage.stage_id.clone(),
            stage.choice.model.to_string(),
            stage.choice.effort.to_string(),
        );
        let (observation, last_attempt) = grouped.entry(key).or_insert_with(|| {
            (
                OutcomeStage {
                    stage: stage.stage_id.clone(),
                    choice: stage.choice.clone(),
                    cost_usd: 0.0,
                    wall_secs: 0,
                    attempts: 0,
                    passed: Some(true),
                },
                0,
            )
        });
        observation.cost_usd += cost;
        observation.wall_secs += stage
            .finished_at
            .map(|end| elapsed(stage.started_at, end))
            .transpose()?
            .unwrap_or(0);
        observation.attempts += 1;
        if stage.attempt > *last_attempt {
            observation.passed = Some(true);
            *last_attempt = stage.attempt;
        }
        if stage.attempt == *last_attempt {
            observation.passed = Some(
                observation.passed == Some(true)
                    && stage.state == StageState::Completed
                    && stage.verdict != Some(Verdict::Fail),
            );
        }
    }
    assert!(grouped.len() <= stages.len());
    Ok(grouped.into_values().map(|(stage, _)| stage).collect())
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
            let text = error.to_lowercase();
            let class = if text.contains("budget") {
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
            };
            Ok((OutcomeResult::Failed, Some(class)))
        }
        _ => Err(ArchonError::Invalid("a live run has no outcome".into())),
    }
}

fn reported_diff(stages: &[StageRun]) -> Result<Option<DiffStat>, ArchonError> {
    let value = stages
        .iter()
        .rev()
        .find_map(|s| s.structured.as_ref()?.get("diff_stat"));
    value
        .map(|v| {
            serde_json::from_value(v.clone())
                .map_err(|e| ArchonError::Invalid(format!("reported diff_stat: {e}")))
        })
        .transpose()
}

fn stage_charge(store: &Store, stage: &StageRun, estimates: bool) -> Result<f64, ArchonError> {
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
