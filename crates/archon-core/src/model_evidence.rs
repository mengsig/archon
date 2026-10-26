//! On-demand observations of recorded stage attempts, never model recommendations.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::ArchonError;
use crate::cost::{CostQuery, DayBound, DayRange};
use crate::id::RepoId;
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

/// One provider's attempts and completed cost: the split provider weights
/// achieved over the range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderEvidenceRow {
    pub provider: String,
    pub attempts: usize,
    pub completed: usize,
    pub cost_usd: f64,
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
    /// By provider, most attempts first.
    #[serde(default)]
    pub providers: Vec<ProviderEvidenceRow>,
    #[serde(default)]
    pub pipelines: Vec<crate::lesson::PipelineEvidence>,
    /// Names of the repositories `pipelines` cite, as `cost` names them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repo_names: BTreeMap<RepoId, String>,
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
        self.provider_text(&mut text);
        self.pipeline_text(&mut text);
        text.trim_end().to_owned()
    }

    fn provider_text(&self, text: &mut String) {
        let attempts: usize = self.providers.iter().map(|p| p.attempts).sum();
        let cost: f64 = self.providers.iter().map(|p| p.cost_usd).sum();
        let share = |part: f64, whole: f64| match whole > 0.0 {
            true => format!("{:.0}%", 100.0 * part / whole),
            false => "-".to_owned(),
        };
        for p in &self.providers {
            let _ = writeln!(
                text,
                "Provider {} n={} ({} of attempts) completed={} cost=${:.4} ({} of cost)",
                p.provider,
                p.attempts,
                share(p.attempts as f64, attempts as f64),
                p.completed,
                p.cost_usd,
                share(p.cost_usd, cost)
            );
        }
    }
    fn pipeline_text(&self, text: &mut String) {
        for p in &self.pipelines {
            let repo = p.repo.as_ref().map_or("global", |repo| {
                self.repo_names
                    .get(repo)
                    .map_or(repo.as_str(), String::as_str)
            });
            let _ = writeln!(
                text,
                "Pipeline {} / {} n={} pass={:.0}% median=${:.3} wall={:.0}s corrected={:.0}%",
                repo,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lesson::PipelineEvidence;

    #[test]
    fn pipeline_evidence_names_the_repository_and_falls_back_to_its_id() {
        let named = RepoId::new();
        let gone = RepoId::new();
        let evidence = |repo: Option<RepoId>| PipelineEvidence {
            repo,
            pipeline: "adaptive".into(),
            runs: 4,
            pass_rate: 0.5,
            median_cost_usd: 1.0,
            median_wall_secs: 60.0,
            correction_rate: 0.0,
            estimated_saving_usd: None,
        };
        let report: ModelEvidenceReport = serde_json::from_value(serde_json::json!({
            "range": {"since": "2026-10-01", "until": "2026-10-26"},
            "estimates_counted": true, "rows": [], "omitted": 0
        }))
        .expect("a report without the optional fields");
        let report = ModelEvidenceReport {
            pipelines: vec![
                evidence(Some(named.clone())),
                evidence(Some(gone.clone())),
                evidence(None),
            ],
            repo_names: BTreeMap::from([(named, "web".to_owned())]),
            ..report
        };
        let text = report.text();
        assert!(text.contains("\nPipeline web / adaptive n=4"), "{text}");
        assert!(
            text.contains(&format!("\nPipeline {gone} / adaptive")),
            "{text}"
        );
        assert!(text.contains("\nPipeline global / adaptive"), "{text}");
    }
}
