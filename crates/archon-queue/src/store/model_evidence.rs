//! Stage observations and finished-run evidence for inspectable model defaults.

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
                .entry((stage.stage_id.clone(), stage.current_choice().clone()))
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
            let boundary = stage
                .model_segments
                .last()
                .map_or(0.0, |s| s.cost_before_usd.min(cost));
            samples.costs.push(cost - boundary);
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
        pipelines: Vec::new(),
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

/// One model's independent run samples, including all its attempts and descendants.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RouteEvidence {
    pub route: archon_core::outcome::LearnedRoute,
    pub median_cost_usd: f64,
}

impl RouteEvidence {
    pub fn quality_interval(&self) -> (f64, f64) {
        assert!(self.route.passed <= self.route.runs);
        assert!(self.median_cost_usd.is_finite() && self.median_cost_usd >= 0.0);
        if self.route.runs == 0 {
            return (0.0, 1.0);
        }
        let n = f64::from(self.route.runs);
        let p = f64::from(self.route.passed) / n;
        let z2 = 1.96_f64.powi(2);
        let center = p + z2 / (2.0 * n);
        let radius = 1.96 * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt();
        (
            (center - radius).max(0.0) / (1.0 + z2 / n),
            (center + radius) / (1.0 + z2 / n),
        )
    }

    fn quality_cost_score(&self) -> f64 {
        self.quality_interval().0 / (1.0 + 0.05 * self.median_cost_usd.ln_1p())
    }
}

pub fn preferred_route(evidence: &[RouteEvidence]) -> Option<&RouteEvidence> {
    evidence
        .iter()
        .filter(|e| e.route.runs >= 5 && e.quality_interval().1 >= 0.3)
        .max_by(|a, b| {
            a.quality_cost_score()
                .total_cmp(&b.quality_cost_score())
                .then_with(|| b.median_cost_usd.total_cmp(&a.median_cost_usd))
        })
}

impl super::Store {
    pub fn route_evidence(
        &self,
        repo: Option<&archon_core::id::RepoId>,
        pipeline: &str,
        stage: &str,
        size: Option<archon_core::outcome::DiffSize>,
    ) -> Result<Vec<RouteEvidence>, ArchonError> {
        self.contextual_route_evidence(repo, pipeline, stage, size, None)
    }

    pub fn contextual_route_evidence(
        &self,
        repo: Option<&archon_core::id::RepoId>,
        pipeline: &str,
        stage: &str,
        size: Option<archon_core::outcome::DiffSize>,
        context: Option<&archon_core::outcome::RouteContext>,
    ) -> Result<Vec<RouteEvidence>, ArchonError> {
        if pipeline.is_empty() || stage.is_empty() {
            return Err(ArchonError::Invalid(
                "route needs pipeline and stage".into(),
            ));
        }
        let mut groups = std::collections::BTreeMap::new();
        for outcome in self.route_outcomes(repo, pipeline, stage, size, context)? {
            if outcome.pipeline != pipeline || size.is_some_and(|s| outcome.size() != Some(s)) {
                continue;
            }
            use archon_core::outcome::{CorrectionCause, FailureClass, OutcomeResult};
            if outcome.result == OutcomeResult::Cancelled
                || matches!(
                    outcome.failure,
                    Some(
                        FailureClass::Infrastructure
                            | FailureClass::Budget
                            | FailureClass::Timeout
                            | FailureClass::Blocked
                            | FailureClass::Escalated
                            | FailureClass::Ship
                    )
                )
            {
                continue;
            }
            for observed in outcome
                .stages
                .iter()
                .filter(|s| s.stage == stage && s.passed.is_some())
            {
                let (costs, wins) = groups
                    .entry((
                        observed.choice.model.to_string(),
                        observed.choice.effort.to_string(),
                    ))
                    .or_insert((Vec::new(), 0u32));
                costs.push(observed.cost_usd);
                let corrected = outcome
                    .feedback
                    .iter()
                    .any(|e| e.stage == observed.stage && e.cause == CorrectionCause::Quality);
                *wins += u32::from(observed.passed == Some(true) && !corrected);
            }
        }
        finish_routes(groups, repo, pipeline, stage, size)
    }
}

fn finish_routes(
    groups: std::collections::BTreeMap<(String, String), (Vec<f64>, u32)>,
    repo: Option<&archon_core::id::RepoId>,
    pipeline: &str,
    stage: &str,
    size: Option<archon_core::outcome::DiffSize>,
) -> Result<Vec<RouteEvidence>, ArchonError> {
    use archon_core::outcome::LearnedRoute;
    use std::str::FromStr;
    let mut evidence = Vec::new();
    for ((model, effort), (mut costs, passed)) in groups {
        assert!(passed as usize <= costs.len());
        let runs = u32::try_from(costs.len())
            .map_err(|_| ArchonError::Store("too many route samples".into()))?;
        let median_cost_usd = super::outcomes::stats::median(&mut costs);
        evidence.push(RouteEvidence {
            median_cost_usd,
            route: LearnedRoute {
                repo: repo.cloned(),
                pipeline: pipeline.into(),
                stage: stage.into(),
                size,
                choice: ModelChoice {
                    model: archon_core::model::ModelRef::from_str(&model)?,
                    effort: archon_core::model::Effort::from_str(&effort)?,
                },
                runs,
                passed,
            },
        });
    }
    evidence.sort_by(|a, b| a.median_cost_usd.total_cmp(&b.median_cost_usd));
    assert!(
        evidence
            .windows(2)
            .all(|p| p[0].median_cost_usd <= p[1].median_cost_usd)
    );
    Ok(evidence)
}

impl super::Store {
    /// The static fallback used by plan filling, for a measured counterfactual.
    pub fn record_static_plan(
        &self,
        repo: Option<&archon_core::id::RepoId>,
        pipeline: &str,
        plan: &archon_core::model::ModelPlan,
    ) -> Result<(), ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        if pipeline.is_empty() || plan.0.values().any(Vec::is_empty) {
            return Err(ArchonError::Invalid(
                "a static baseline names a pipeline and nonempty choices".into(),
            ));
        }
        self.save_learning_value(
            &format!(
                "learning/static/{}/{pipeline}",
                repo.map_or("global", |r| r.as_str())
            ),
            &super::codec::to_json(plan, "static model baseline")?,
        )
    }

    pub fn estimated_model_saving(
        &self,
        repo: Option<&archon_core::id::RepoId>,
        pipeline: &str,
        policy: &archon_core::policy::ModelPolicy,
    ) -> Result<Option<f64>, ArchonError> {
        let key = format!(
            "learning/static/{}/{pipeline}",
            repo.map_or("global", |r| r.as_str())
        );
        let Some(raw) = self.kv_get(&key)? else {
            return Ok(None);
        };
        let plan: archon_core::model::ModelPlan = from_json(&raw, "static model baseline")?;
        let mut saving = 0.0;
        let mut measured = false;
        for (stage, choices) in plan.0 {
            let mut evidence = self.route_evidence(repo, pipeline, &stage, None)?;
            evidence.retain(|e| policy.check(&e.route.choice).is_ok());
            let Some(selected) = preferred_route(&evidence) else {
                continue;
            };
            for choice in choices {
                let Some(baseline) = evidence
                    .iter()
                    .find(|e| e.route.runs >= 5 && e.route.choice == choice)
                else {
                    return Ok(None);
                };
                saving += baseline.median_cost_usd - selected.median_cost_usd;
                measured = true;
            }
        }
        assert!(saving.is_finite());
        assert!(!measured || !raw.is_empty());
        Ok(measured.then_some(saving))
    }
}
