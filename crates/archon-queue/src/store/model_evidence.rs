//! Stage observations grouped on demand; no persisted assessments or routing.

use std::collections::HashMap;

use archon_core::cost::DayRange;
use archon_core::error::ArchonError;
use archon_core::model::ModelChoice;
use archon_core::model_evidence::{
    MAX_ROWS, ModelEvidenceQuery, ModelEvidenceReport, ModelEvidenceRow,
};
use archon_core::queue::QueueItem;
use archon_core::run::{StageRun, StageState, Verdict};
use rusqlite::{Connection, params};

use super::codec::{SqlContext, from_json};
use super::delegated::BENEATH;

#[derive(Default)]
struct Samples {
    attempts: usize,
    costs: Vec<f64>,
    minutes: Vec<f64>,
    verdicts: usize,
    passed: usize,
}

/// Owned observations, so classification can read files without a store lock.
#[derive(Debug)]
pub struct ModelEvidenceSnapshot {
    attempts: Vec<Attempt>,
    range: DayRange,
    estimates: bool,
}

#[derive(Debug)]
struct Attempt {
    stage: StageRun,
    item: QueueItem,
    pipeline: String,
    billed: Option<f64>,
    estimated: Option<f64>,
    /// What the agents the stage agent delegated to were charged.
    delegated_billed: f64,
    delegated_estimated: f64,
}

pub(super) fn read(
    conn: &Connection,
    query: &ModelEvidenceQuery,
    range: &DayRange,
    estimates: bool,
) -> Result<ModelEvidenceSnapshot, ArchonError> {
    if range.since > range.until {
        return Err(ArchonError::Invalid(
            "evidence range starts after it ends".into(),
        ));
    }
    let repo = query
        .repo
        .as_ref()
        .map(|r| super::repos::find(conn, r))
        .transpose()?;
    // A stage cost is complete: its agent's spend and that of every agent
    // beneath it, so a lead that delegates is not compared as a cheap one.
    let sql = format!(
        "WITH RECURSIVE {BENEATH},
         delegated(root, cost_usd, estimated_cost_usd) AS (
             SELECT b.root, SUM(s.cost_usd), SUM(s.estimated_cost_usd)
             FROM beneath b JOIN spend s ON s.agent = b.agent GROUP BY b.root
         )
         SELECT st.data, i.data, s.cost_usd, s.estimated_cost_usd, r.pipeline,
                COALESCE(d.cost_usd, 0), COALESCE(d.estimated_cost_usd, 0)
         FROM stage_runs st JOIN runs r ON r.id = st.run JOIN items i ON i.id = r.item
         LEFT JOIN spend s ON s.agent = st.aether_agent
         LEFT JOIN delegated d ON d.root = st.aether_agent
         WHERE (?1 IS NULL OR i.repo = ?1) AND (?2 IS NULL OR r.pipeline = ?2)
           AND (?3 IS NULL OR st.stage_id = ?3)
           AND date(st.started_at, 'localtime') BETWEEN ?4 AND ?5"
    );
    let mut statement = conn.prepare(&sql).ctx("prepare model evidence")?;
    let mut rows = statement
        .query(params![
            repo.as_ref().map(|r| r.id.as_str()),
            query.pipeline,
            query.stage,
            range.since.to_string(),
            range.until.to_string(),
        ])
        .ctx("query model evidence")?;
    let mut attempts = Vec::new();
    while let Some(row) = rows.next().ctx("read model evidence")? {
        attempts.push(attempt(row)?);
    }
    Ok(ModelEvidenceSnapshot {
        attempts,
        range: *range,
        estimates,
    })
}

/// One row of `read`: a stage run, its item, and the charges on both sides.
fn attempt(row: &rusqlite::Row<'_>) -> Result<Attempt, ArchonError> {
    let stage = row.get::<_, String>(0).ctx("read evidence stage")?;
    let item = row.get::<_, String>(1).ctx("read evidence item")?;
    Ok(Attempt {
        stage: from_json(&stage, "evidence stage")?,
        item: from_json(&item, "evidence item")?,
        pipeline: row.get(4).ctx("read evidence pipeline")?,
        billed: row.get(2).ctx("read evidence cost")?,
        estimated: row.get(3).ctx("read evidence estimate")?,
        delegated_billed: row.get(5).ctx("read delegated evidence cost")?,
        delegated_estimated: row.get(6).ctx("read delegated evidence estimate")?,
    })
}

impl ModelEvidenceSnapshot {
    /// Each attempt's item, executed pipeline name and stage run.
    pub fn attempts(&self) -> impl Iterator<Item = (&QueueItem, &str, &StageRun)> {
        self.attempts
            .iter()
            .map(|a| (&a.item, a.pipeline.as_str(), &a.stage))
    }

    /// Aggregate the captured rows with the caller's stage classification.
    pub fn report(
        self,
        mut is_verdict_stage: impl FnMut(&QueueItem, &StageRun) -> Result<bool, ArchonError>,
    ) -> Result<ModelEvidenceReport, ArchonError> {
        let mut groups: HashMap<(String, ModelChoice), Samples> = HashMap::new();
        for attempt in self.attempts {
            let stage = attempt.stage;
            let samples = groups
                .entry((stage.stage_id.clone(), stage.choice.clone()))
                .or_default();
            samples.attempts += 1;
            if stage.state != StageState::Completed {
                continue;
            }
            let cost = attempt.billed.unwrap_or(stage.cost_usd)
                + attempt.delegated_billed
                + if self.estimates {
                    attempt.estimated.unwrap_or(0.0) + attempt.delegated_estimated
                } else {
                    0.0
                };
            if !cost.is_finite() || cost < 0.0 {
                return Err(ArchonError::Store(format!(
                    "stage {} has invalid evidence cost {cost}",
                    stage.id
                )));
            }
            samples.costs.push(cost);
            if let Some(end) = stage.finished_at {
                let elapsed = end.signed_duration_since(stage.started_at);
                if elapsed < chrono::Duration::zero() {
                    return Err(ArchonError::Store(format!(
                        "stage {} finishes before it starts",
                        stage.id
                    )));
                }
                samples
                    .minutes
                    .push(elapsed.num_milliseconds() as f64 / 60_000.0);
            }
            if is_verdict_stage(&attempt.item, &stage)?
                && let Some(verdict) = stage.verdict
            {
                samples.verdicts += 1;
                samples.passed += usize::from(verdict == Verdict::Pass);
            }
        }
        Ok(finish(groups, self.range, self.estimates))
    }
}

fn finish(
    groups: HashMap<(String, ModelChoice), Samples>,
    range: DayRange,
    estimates: bool,
) -> ModelEvidenceReport {
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|((stage, choice), mut samples)| {
            samples.costs.sort_by(f64::total_cmp);
            samples.minutes.sort_by(f64::total_cmp);
            ModelEvidenceRow {
                stage,
                choice,
                attempts: samples.attempts,
                completed: samples.costs.len(),
                median_cost_usd: median(&samples.costs),
                // Nearest-rank p90, including the maximum for fewer than ten samples.
                p90_cost_usd: samples
                    .costs
                    .get((samples.costs.len() * 9).div_ceil(10).saturating_sub(1))
                    .copied(),
                median_minutes: median(&samples.minutes),
                verdicts: samples.verdicts,
                passed: samples.passed,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then_with(|| {
                a.median_cost_usd
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.median_cost_usd.unwrap_or(f64::INFINITY))
            })
            .then_with(|| a.choice.model.to_string().cmp(&b.choice.model.to_string()))
            .then_with(|| {
                a.choice
                    .effort
                    .to_string()
                    .cmp(&b.choice.effort.to_string())
            })
    });
    let omitted = rows.len().saturating_sub(MAX_ROWS);
    rows.truncate(MAX_ROWS);
    ModelEvidenceReport {
        range,
        estimates_counted: estimates,
        rows,
        omitted,
    }
}

fn median(sorted: &[f64]) -> Option<f64> {
    let middle = sorted.len() / 2;
    let upper = *sorted.get(middle)?;
    Some(if sorted.len().is_multiple_of(2) {
        sorted[middle - 1] / 2.0 + upper / 2.0
    } else {
        upper
    })
}
