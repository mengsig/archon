//! Stage observations and finished-run evidence for inspectable model defaults.

use std::collections::{BTreeMap, HashMap, HashSet};

use archon_core::Timestamp;
use archon_core::cost::DayRange;
use archon_core::error::ArchonError;
use archon_core::feedback::feedback_window;
use archon_core::id::{FeedbackId, PipelineRunId, RepoId};
use archon_core::model::{Effort, ModelChoice};
use archon_core::model_evidence::{
    MAX_ROWS, ModelEvidenceQuery, ModelEvidenceReport, ModelEvidenceRow, ProviderEvidenceRow,
    TierEvidenceRow,
};
use archon_core::model_tier::{ModelCatalog, Tier};
use archon_core::outcome::{
    CorrectionCause, DiffSize, FailureClass, Outcome, OutcomeResult, RouteContext,
};
use archon_core::queue::QueueItem;
use archon_core::run::{StageRun, StageState, Verdict};
use rusqlite::{Connection, params};

use super::codec::{SqlContext, from_json};
use super::delegated::BENEATH;
use super::operator_feedback as feedback;

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
        let attempt = attempt(row)?;
        // A failed engine login says nothing of the model: it is no attempt.
        if !attempt.stage.failed_on_engine_login() {
            attempts.push(attempt);
        }
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

    /// Aggregate the captured rows with the caller's stage classification,
    /// by model and by tier: the tier an attempt recorded, else `catalog`'s.
    pub fn report(
        self,
        catalog: &ModelCatalog,
        mut is_verdict_stage: impl FnMut(&QueueItem, &StageRun) -> Result<bool, ArchonError>,
    ) -> Result<ModelEvidenceReport, ArchonError> {
        let mut groups: HashMap<(String, ModelChoice, archon_core::config::HostContext), Samples> =
            HashMap::new();
        let mut tiers: HashMap<(String, Tier, Effort), Samples> = HashMap::new();
        let mut providers: BTreeMap<String, ProviderEvidenceRow> = BTreeMap::new();
        for attempt in self.attempts {
            let stage = &attempt.stage;
            let choice = stage.current_choice();
            let sample = sample(&attempt, self.estimates, &mut is_verdict_stage)?;
            add_provider(&mut providers, &choice.model.provider, sample);
            let key = (
                stage.stage_id.clone(),
                choice.without_tier(),
                stage.host_context,
            );
            groups.entry(key).or_default().add(sample);
            let tier = match &choice.tier {
                Some(tier) => Some(tier.resolved),
                None => catalog.tier_of(&choice.model),
            };
            if let Some(tier) = tier {
                let key = (stage.stage_id.clone(), tier, choice.effort);
                tiers.entry(key).or_default().add(sample);
            }
        }
        let mut report = finish(groups, tiers, self.range, self.estimates);
        report.providers = provider_rows(providers);
        Ok(report)
    }
}

fn add_provider(rows: &mut BTreeMap<String, ProviderEvidenceRow>, provider: &str, sample: Sample) {
    assert!(!provider.is_empty(), "a model names its provider");
    let row = rows
        .entry(provider.to_owned())
        .or_insert_with(|| ProviderEvidenceRow {
            provider: provider.to_owned(),
            attempts: 0,
            completed: 0,
            cost_usd: 0.0,
        });
    row.attempts += 1;
    if let Some(done) = sample.completed {
        row.completed += 1;
        row.cost_usd += done.cost;
    }
    debug_assert!(row.completed <= row.attempts && row.cost_usd.is_finite());
}

/// Most attempts first, then by name.
fn provider_rows(rows: BTreeMap<String, ProviderEvidenceRow>) -> Vec<ProviderEvidenceRow> {
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| {
        b.attempts
            .cmp(&a.attempts)
            .then_with(|| a.provider.cmp(&b.provider))
    });
    debug_assert!(rows.windows(2).all(|w| w[0].attempts >= w[1].attempts));
    rows
}

/// What one attempt adds to its groups; only a completed one is measured.
#[derive(Clone, Copy)]
struct Sample {
    completed: Option<Completed>,
}

#[derive(Clone, Copy)]
struct Completed {
    cost: f64,
    minutes: Option<f64>,
    verdict: Option<bool>,
}

impl Samples {
    fn add(&mut self, sample: Sample) {
        self.attempts += 1;
        let Some(done) = sample.completed else {
            return;
        };
        self.costs.push(done.cost);
        self.minutes.extend(done.minutes);
        if let Some(passed) = done.verdict {
            self.verdicts += 1;
            self.passed += usize::from(passed);
        }
    }

    /// Sorted, with the summary every evidence row shows.
    fn summary(mut self) -> Summary {
        self.costs.sort_by(f64::total_cmp);
        self.minutes.sort_by(f64::total_cmp);
        Summary {
            attempts: self.attempts,
            completed: self.costs.len(),
            median_cost_usd: median(&self.costs),
            // Nearest-rank p90, including the maximum for fewer than ten samples.
            p90_cost_usd: self
                .costs
                .get((self.costs.len() * 9).div_ceil(10).saturating_sub(1))
                .copied(),
            median_minutes: median(&self.minutes),
            verdicts: self.verdicts,
            passed: self.passed,
        }
    }
}

struct Summary {
    attempts: usize,
    completed: usize,
    median_cost_usd: Option<f64>,
    p90_cost_usd: Option<f64>,
    median_minutes: Option<f64>,
    verdicts: usize,
    passed: usize,
}

fn sample(
    attempt: &Attempt,
    estimates: bool,
    is_verdict_stage: &mut impl FnMut(&QueueItem, &StageRun) -> Result<bool, ArchonError>,
) -> Result<Sample, ArchonError> {
    let stage = &attempt.stage;
    if stage.state != StageState::Completed {
        return Ok(Sample { completed: None });
    }
    let cost = attempt.billed.unwrap_or(stage.cost_usd)
        + attempt.delegated_billed
        + if estimates {
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
    let minutes = match stage.finished_at {
        Some(end) => {
            let elapsed = end.signed_duration_since(stage.started_at);
            if elapsed < chrono::Duration::zero() {
                return Err(ArchonError::Store(format!(
                    "stage {} finishes before it starts",
                    stage.id
                )));
            }
            Some(elapsed.num_milliseconds() as f64 / 60_000.0)
        }
        None => None,
    };
    let verdict = match is_verdict_stage(&attempt.item, stage)? {
        true => stage.verdict.map(|v| v == Verdict::Pass),
        false => None,
    };
    Ok(Sample {
        completed: Some(Completed {
            cost: cost - boundary,
            minutes,
            verdict,
        }),
    })
}

fn finish(
    groups: HashMap<(String, ModelChoice, archon_core::config::HostContext), Samples>,
    tiers: HashMap<(String, Tier, Effort), Samples>,
    range: DayRange,
    estimates: bool,
) -> ModelEvidenceReport {
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|((stage, choice, host_context), samples)| {
            let s = samples.summary();
            ModelEvidenceRow {
                stage,
                choice,
                host_context,
                attempts: s.attempts,
                completed: s.completed,
                median_cost_usd: s.median_cost_usd,
                p90_cost_usd: s.p90_cost_usd,
                median_minutes: s.median_minutes,
                verdicts: s.verdicts,
                passed: s.passed,
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
            .then_with(|| a.host_context.cmp(&b.host_context))
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
    let mut tiers: Vec<_> = tiers
        .into_iter()
        .map(|((stage, tier, effort), samples)| {
            let s = samples.summary();
            TierEvidenceRow {
                stage,
                tier,
                effort,
                attempts: s.attempts,
                completed: s.completed,
                median_cost_usd: s.median_cost_usd,
                p90_cost_usd: s.p90_cost_usd,
                median_minutes: s.median_minutes,
                verdicts: s.verdicts,
                passed: s.passed,
            }
        })
        .collect();
    tiers.sort_by(|a, b| (&a.stage, a.tier, a.effort).cmp(&(&b.stage, b.tier, b.effort)));
    tiers.truncate(MAX_ROWS);
    ModelEvidenceReport {
        range,
        estimates_counted: estimates,
        rows,
        omitted,
        tiers,
        providers: Vec::new(),
        pipelines: Vec::new(),
        repo_names: Default::default(),
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

/// One stage's routing cohort: whose finished runs a learned default reads.
#[derive(Debug, Clone, Copy)]
pub struct RouteQuery<'a> {
    pub repo: Option<&'a RepoId>,
    pub pipeline: &'a str,
    pub stage: &'a str,
    pub size: Option<DiffSize>,
    pub context: Option<&'a RouteContext>,
}

/// A cohort's routing samples and the operator corrections behind them.
#[derive(Debug, Clone)]
pub struct CohortEvidence {
    pub routes: Vec<RouteEvidence>,
    /// Corrections penalizing a sample this repository cohort reads now.
    pub penalties: Vec<FeedbackId>,
    /// A correction of this repository cohort penalizes now, its sample
    /// read or not: the cohort explores no other model.
    pub corrected: bool,
    /// The cohort has had a credited quality correction: it reads only runs
    /// of the last 90 days, and never pools global evidence.
    pub feedback_history: bool,
}

/// Whether a finished run is a sample of model quality at all.
pub(super) fn counts_for_routing(outcome: &Outcome) -> bool {
    let excluded = matches!(
        outcome.failure,
        Some(
            FailureClass::Infrastructure
                | FailureClass::Budget
                | FailureClass::Timeout
                | FailureClass::Blocked
                | FailureClass::Escalated
                | FailureClass::Ship
        )
    );
    outcome.result != OutcomeResult::Cancelled && !excluded
}

impl super::Store {
    pub fn route_evidence(
        &self,
        repo: Option<&RepoId>,
        pipeline: &str,
        stage: &str,
        size: Option<DiffSize>,
    ) -> Result<Vec<RouteEvidence>, ArchonError> {
        self.contextual_route_evidence(repo, pipeline, stage, size, None)
    }

    pub fn contextual_route_evidence(
        &self,
        repo: Option<&RepoId>,
        pipeline: &str,
        stage: &str,
        size: Option<DiffSize>,
        context: Option<&RouteContext>,
    ) -> Result<Vec<RouteEvidence>, ArchonError> {
        let query = RouteQuery {
            repo,
            pipeline,
            stage,
            size,
            context,
        };
        Ok(self.cohort_evidence(&query, archon_core::now())?.routes)
    }

    /// The cohort's evidence `as_of`. In a repository cohort each active
    /// quality correction turns the one sample it names unsuccessful; the
    /// corrections are read by indexed columns, only for the samples read.
    pub fn cohort_evidence(
        &self,
        query: &RouteQuery<'_>,
        as_of: Timestamp,
    ) -> Result<CohortEvidence, ArchonError> {
        if query.pipeline.is_empty() || query.stage.is_empty() {
            return Err(ArchonError::Invalid(
                "route needs pipeline and stage".into(),
            ));
        }
        let history = feedback::has_cohort_feedback(&self.conn, query, as_of)?;
        let window = history.then(|| (as_of - feedback_window(), as_of));
        let outcomes = self.route_outcomes(query, window)?;
        let masks = if history {
            let runs: Vec<&PipelineRunId> = outcomes.iter().map(|o| &o.run).collect();
            feedback::active_masks(&self.conn, &runs, query.stage, as_of)?
        } else {
            Vec::new()
        };
        let masked: HashSet<(&PipelineRunId, &ModelChoice)> =
            masks.iter().map(|m| (&m.run, &m.choice)).collect();
        let groups = route_samples(&outcomes, query, &masked);
        assert!(history || masked.is_empty(), "no mask without feedback");
        let corrected =
            !masks.is_empty() || (history && feedback::cohort_corrected(&self.conn, query, as_of)?);
        assert!(history || !corrected, "a correction is feedback history");
        Ok(CohortEvidence {
            routes: finish_routes(groups, query)?,
            penalties: masks.into_iter().map(|m| m.feedback).collect(),
            corrected,
            feedback_history: history,
        })
    }
}

/// Costs and successes per (model, effort); a legacy quality correction
/// masks the whole stage, a typed one the one choice it credits.
fn route_samples(
    outcomes: &[Outcome],
    query: &RouteQuery<'_>,
    masks: &HashSet<(&PipelineRunId, &ModelChoice)>,
) -> BTreeMap<(String, String), (Vec<f64>, u32)> {
    let mut groups = BTreeMap::new();
    for outcome in outcomes {
        let other_size = query.size.is_some_and(|s| outcome.size() != Some(s));
        if outcome.pipeline != query.pipeline || other_size || !counts_for_routing(outcome) {
            continue;
        }
        let corrected = outcome
            .feedback
            .iter()
            .any(|e| e.stage == query.stage && e.cause == CorrectionCause::Quality);
        for observed in outcome
            .stages
            .iter()
            .filter(|s| s.stage == query.stage && s.passed.is_some())
        {
            let (costs, wins) = groups
                .entry((
                    observed.choice.model.to_string(),
                    observed.choice.effort.to_string(),
                ))
                .or_insert((Vec::new(), 0u32));
            costs.push(observed.cost_usd);
            let masked = corrected || masks.contains(&(&outcome.run, &observed.choice));
            *wins += u32::from(observed.passed == Some(true) && !masked);
        }
    }
    let bounded = groups
        .values()
        .all(|(costs, wins)| *wins as usize <= costs.len());
    assert!(bounded, "a choice wins at most once per observation");
    groups
}

fn finish_routes(
    groups: BTreeMap<(String, String), (Vec<f64>, u32)>,
    query: &RouteQuery<'_>,
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
                repo: query.repo.cloned(),
                pipeline: query.pipeline.into(),
                stage: query.stage.into(),
                size: query.size,
                choice: ModelChoice {
                    model: archon_core::model::ModelRef::from_str(&model)?,
                    effort: archon_core::model::Effort::from_str(&effort)?,
                    tier: None,
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
