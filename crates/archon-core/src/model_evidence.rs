//! On-demand observations of recorded stage attempts, never model recommendations.

use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::ArchonError;
use crate::cost::{CostQuery, DayBound, DayRange};
use crate::model::{Effort, ModelChoice};
use crate::model_tier::Tier;
use crate::protocol::RepoRef;

pub const DEFAULT_DAYS: u32 = 30;
pub const MAX_ROWS: usize = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEvidenceQuery {
    pub repo: Option<RepoRef>,
    pub pipeline: Option<String>,
    pub stage: Option<String>,
    pub since: Option<DayBound>,
}

impl ModelEvidenceQuery {
    pub fn range(&self, today: NaiveDate) -> Result<DayRange, ArchonError> {
        CostQuery {
            since: Some(self.since.unwrap_or(DayBound::Days(DEFAULT_DAYS))),
            ..CostQuery::default()
        }
        .range(today)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEvidenceRow {
    pub stage: String,
    pub choice: ModelChoice,
    #[serde(default)]
    pub host_context: crate::config::HostContext,
    /// Each stage run (including each fan-out slot) is one attempt.
    pub attempts: usize,
    pub completed: usize,
    pub median_cost_usd: Option<f64>,
    pub p90_cost_usd: Option<f64>,
    pub median_minutes: Option<f64>,
    /// Completed review/test attempts with a recorded verdict; Unknown does not pass.
    pub verdicts: usize,
    pub passed: usize,
}

/// The same observations pooled by the tier of the model that ran, so they
/// outlive a model replaced within its tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierEvidenceRow {
    pub stage: String,
    pub tier: Tier,
    pub effort: Effort,
    pub attempts: usize,
    pub completed: usize,
    pub median_cost_usd: Option<f64>,
    pub p90_cost_usd: Option<f64>,
    pub median_minutes: Option<f64>,
    pub verdicts: usize,
    pub passed: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEvidenceReport {
    pub range: DayRange,
    pub estimates_counted: bool,
    pub rows: Vec<ModelEvidenceRow>,
    pub omitted: usize,
    /// By (stage, tier, effort); attempts on a model Aether places in no tier are left out.
    #[serde(default)]
    pub tiers: Vec<TierEvidenceRow>,
    #[serde(default)]
    pub pipelines: Vec<crate::lesson::PipelineEvidence>,
}

/// `completed=… median=… p90=… median_min=…`, then any pass share and a
/// small-sample warning.
fn measures(
    completed: usize,
    (median, p90, minutes): (Option<f64>, Option<f64>, Option<f64>),
    (passed, verdicts): (usize, usize),
) -> String {
    let cost = |value: Option<f64>| value.map_or("-".into(), |v| format!("${v:.4}"));
    let minutes = minutes.map_or("-".into(), |v| format!("{v:.2}"));
    let pass = if verdicts == 0 {
        String::new()
    } else {
        format!(
            " pass={passed}/{verdicts} ({:.0}%)",
            100.0 * passed as f64 / verdicts as f64
        )
    };
    let small = if completed < 5 {
        " [small sample: completed n < 5]"
    } else {
        ""
    };
    format!(
        "completed={completed} median={} p90={} median_min={minutes}{pass}{small}",
        cost(median),
        cost(p90)
    )
}

impl ModelEvidenceReport {
    /// Shared compact text for MCP and CLI. Missing observations stay missing.
    pub fn text(&self) -> String {
        let mut text = format!(
            "Model evidence {}..{} (by attempt start, local days; completed costs {} estimates).\n",
            self.range.since,
            self.range.until,
            if self.estimates_counted {
                "include"
            } else {
                "exclude"
            }
        );
        for row in &self.rows {
            let measures = measures(
                row.completed,
                (row.median_cost_usd, row.p90_cost_usd, row.median_minutes),
                (row.passed, row.verdicts),
            );
            let _ = writeln!(
                text,
                "{} {} {} host_context={} n={} {measures}",
                row.stage,
                row.choice.model,
                row.choice.effort,
                row.host_context.as_str(),
                row.attempts,
            );
        }
        if self.rows.is_empty() {
            text.push_str("No stage attempts in this range.\n");
        }
        if self.omitted > 0 {
            let _ = writeln!(text, "Truncated: {} more groups omitted.", self.omitted);
        }
        for row in &self.tiers {
            let measures = measures(
                row.completed,
                (row.median_cost_usd, row.p90_cost_usd, row.median_minutes),
                (row.passed, row.verdicts),
            );
            let _ = writeln!(
                text,
                "Tier {} {} {} n={} {measures}",
                row.stage, row.tier, row.effort, row.attempts,
            );
        }
        self.pipeline_text(&mut text);
        text.trim_end().to_owned()
    }
    fn pipeline_text(&self, text: &mut String) {
        for p in &self.pipelines {
            let _ = writeln!(
                text,
                "Pipeline {} / {} n={} pass={:.0}% median=${:.3} wall={:.0}s corrected={:.0}%",
                p.repo.as_ref().map_or("global", |repo| repo.as_str()),
                p.pipeline,
                p.runs,
                p.pass_rate * 100.0,
                p.median_cost_usd,
                p.median_wall_secs,
                p.correction_rate * 100.0
            );
        }
    }
}
