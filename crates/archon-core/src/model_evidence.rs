//! On-demand observations of recorded stage attempts, never model recommendations.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::ArchonError;
use crate::cost::{CostQuery, DayBound, DayRange};
use crate::effort_sizing::RiskClass;
use crate::id::RepoId;
use crate::model::{Effort, ModelChoice};
use crate::model_tier::Tier;
use crate::plan_origin::PlanSource;
use crate::protocol::RepoRef;

pub const DEFAULT_DAYS: u32 = 30;
pub const MAX_ROWS: usize = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Completed attempts whose pass their own findings dispute: a format clash
    /// kept out of `verdicts`, so it is no fail of the model.
    #[serde(default)]
    pub disputed: usize,
    #[serde(default)]
    /// First attempts that completed successfully; review/test stages use recorded verdicts.
    pub first_attempt_passed: usize,
    #[serde(default)]
    pub first_attempt_verdicts: usize,
    #[serde(default)]
    /// Recorded findings on review attempts; absent when no findings array was recorded.
    pub review_findings: Option<usize>,
    #[serde(default)]
    /// Later same-PR/branch items or dependencies queued after a failed review; absent if unlinked.
    pub fix_up_items: Option<usize>,
    #[serde(default)]
    /// Linked later items titled Revert or tagged revert; absent if unlinked.
    pub revert_items: Option<usize>,
}

/// The same observations pooled by the tier of the model that ran, so they
/// outlive a model replaced within its tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    #[serde(default)]
    pub disputed: usize,
}

/// The attempts whose effort the sizer decided or advised, pooled by the
/// item's risk class and where the effort came from. For display only:
/// no route and no later sizing reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SizingEvidenceRow {
    pub risk_class: RiskClass,
    pub effort_source: PlanSource,
    pub attempts: usize,
    pub completed: usize,
    pub median_cost_usd: Option<f64>,
    pub p90_cost_usd: Option<f64>,
    pub median_minutes: Option<f64>,
    pub verdicts: usize,
    pub passed: usize,
    pub disputed: usize,
}

/// One provider's attempts and completed cost: the split provider weights
/// achieved over the range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderEvidenceRow {
    pub provider: String,
    pub attempts: usize,
    pub completed: usize,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutingEvidenceRow {
    pub repo: Option<RepoId>,
    pub pipeline: String,
    /// Observed escalations followed by a switch to a different pipeline.
    /// None means no classified routing decisions are available.
    pub misses: Option<u64>,
    /// Failed runs whose routing decision is unknown.
    pub unclassified: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// By (risk class, effort source), lowest class first; attempts with no
    /// sizing record are left out. Advisory display only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizing: Vec<SizingEvidenceRow>,
    #[serde(default)]
    pub pipelines: Vec<crate::lesson::PipelineEvidence>,
    /// By repository and effective pipeline, using the failed run's finish day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<RoutingEvidenceRow>,
    /// Names of the repositories the pipeline and routing rows cite.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repo_names: BTreeMap<RepoId, String>,
}

/// `completed=… median=… p90=… median_min=…`, then any pass share, any
/// disputed passes and a small-sample warning.
fn measures(
    completed: usize,
    (median, p90, minutes): (Option<f64>, Option<f64>, Option<f64>),
    (passed, verdicts, disputed): (usize, usize, usize),
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
    let disputed = match disputed {
        0 => String::new(),
        n => format!(" disputed={n}"),
    };
    let small = if completed < 5 {
        " [small sample: completed n < 5]"
    } else {
        ""
    };
    format!(
        "completed={completed} median={} p90={} median_min={minutes}{pass}{disputed}{small}",
        cost(median),
        cost(p90)
    )
}

fn quality_measures(row: &ModelEvidenceRow) -> String {
    let rate = if row.first_attempt_verdicts == 0 {
        "unknown".to_owned()
    } else {
        format!(
            "{:.0}%",
            100.0 * row.first_attempt_passed as f64 / row.first_attempt_verdicts as f64
        )
    };
    let count = |value: Option<usize>| value.map_or("unknown".to_owned(), |n| n.to_string());
    format!(
        "first_attempt_stage_pass={}/{} ({rate}) review_findings={} fix_up_items={} revert_items={}",
        row.first_attempt_passed,
        row.first_attempt_verdicts,
        count(row.review_findings),
        count(row.fix_up_items),
        count(row.revert_items)
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
        if !self.rows.is_empty() {
            text.push_str("Rates are not controlled for task difficulty.\n");
            text.push_str("Fix-ups: later same PR/branch or dependent after failed review; reverts: linked items titled Revert or tagged revert. Unlinked signals are unknown.\n");
        }
        for row in &self.rows {
            let measures = measures(
                row.completed,
                (row.median_cost_usd, row.p90_cost_usd, row.median_minutes),
                (row.passed, row.verdicts, row.disputed),
            );
            let _ = writeln!(
                text,
                "{} {} {} host_context={} n={} {measures} {}",
                row.stage,
                row.choice.model,
                row.choice.effort,
                row.host_context.as_str(),
                row.attempts,
                quality_measures(row),
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
                (row.passed, row.verdicts, row.disputed),
            );
            let _ = writeln!(
                text,
                "Tier {} {} {} n={} {measures}",
                row.stage, row.tier, row.effort, row.attempts,
            );
        }
        self.provider_text(&mut text);
        self.sizing_text(&mut text);
        self.pipeline_text(&mut text);
        self.routing_text(&mut text);
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
    fn sizing_text(&self, text: &mut String) {
        for row in &self.sizing {
            let measures = measures(
                row.completed,
                (row.median_cost_usd, row.p90_cost_usd, row.median_minutes),
                (row.passed, row.verdicts, row.disputed),
            );
            let _ = writeln!(
                text,
                "Sized {} by {} n={} {measures}",
                row.risk_class, row.effort_source, row.attempts,
            );
        }
    }

    fn pipeline_text(&self, text: &mut String) {
        for p in &self.pipelines {
            let repo = self.repo_label(p.repo.as_ref());
            let prs = p.review_prs.as_ref().map_or(String::new(), |prs| {
                let rounds = prs
                    .median_rounds_to_pass
                    .map_or("-".to_owned(), |r| format!("{r:.1}"));
                format!(
                    " prs_passed={}/{} median_rounds_to_pass={rounds}",
                    prs.passed, prs.reviewed
                )
            });
            let _ = writeln!(
                text,
                "Pipeline {} / {} n={} scored={} pass={:.0}% blocked={} escalated={} median=${:.3} wall={:.0}s corrected={:.0}%{prs}",
                repo,
                p.pipeline,
                p.runs,
                p.scored,
                p.pass_rate * 100.0,
                p.blocked,
                p.escalated,
                p.median_cost_usd,
                p.median_wall_secs,
                p.correction_rate * 100.0
            );
        }
    }

    fn routing_text(&self, text: &mut String) {
        for row in &self.routing {
            let repo = self.repo_label(row.repo.as_ref());
            let misses = row
                .misses
                .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
            let _ = writeln!(
                text,
                "Routing {repo} / {} misses={misses} unclassified={} (by run finish)",
                row.pipeline, row.unclassified,
            );
        }
    }

    fn repo_label<'a>(&'a self, repo: Option<&'a RepoId>) -> &'a str {
        repo.map_or("global", |repo| {
            self.repo_names
                .get(repo)
                .map_or(repo.as_str(), String::as_str)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lesson::PipelineEvidence;

    fn report_with_legacy_row() -> ModelEvidenceReport {
        serde_json::from_value(serde_json::json!({
            "range": {"since": "2026-10-01", "until": "2026-10-26"},
            "estimates_counted": true,
            "rows": [{
                "stage": "review", "choice": {"model": "codex/test", "effort": "high"},
                "attempts": 4, "completed": 4, "median_cost_usd": null,
                "p90_cost_usd": null, "median_minutes": null, "verdicts": 3, "passed": 2
            }],
            "omitted": 0
        }))
        .expect("legacy evidence report")
    }

    #[test]
    fn legacy_rows_default_quality_and_display_unknowns() {
        let report = report_with_legacy_row();
        let row = &report.rows[0];
        assert_eq!(
            (row.first_attempt_passed, row.first_attempt_verdicts),
            (0, 0)
        );
        assert_eq!(
            (row.review_findings, row.fix_up_items, row.revert_items),
            (None, None, None)
        );
        let text = report.text();
        assert!(text.contains("pass=2/3 (67%)"), "{text}");
        assert!(
            text.contains("first_attempt_stage_pass=0/0 (unknown)"),
            "{text}"
        );
        assert!(
            text.contains("review_findings=unknown fix_up_items=unknown revert_items=unknown"),
            "{text}"
        );
    }

    #[test]
    fn quality_counts_round_trip_and_display_rate_with_denominator() {
        let mut report = report_with_legacy_row();
        let row = &mut report.rows[0];
        row.first_attempt_passed = 2;
        row.first_attempt_verdicts = 3;
        row.review_findings = Some(0);
        row.fix_up_items = Some(4);
        row.revert_items = Some(1);
        let value = serde_json::to_value(&report).expect("serialize quality evidence");
        let decoded: ModelEvidenceReport =
            serde_json::from_value(value).expect("deserialize quality evidence");
        assert_eq!(decoded, report);
        let text = decoded.text();
        assert!(
            text.contains("first_attempt_stage_pass=2/3 (67%)"),
            "{text}"
        );
        assert!(
            text.contains("review_findings=0 fix_up_items=4 revert_items=1"),
            "{text}"
        );
        assert!(
            text.contains("Rates are not controlled for task difficulty."),
            "{text}"
        );
    }

    #[test]
    fn pipeline_evidence_names_the_repository_and_falls_back_to_its_id() {
        let named = RepoId::new();
        let gone = RepoId::new();
        let evidence = |repo: Option<RepoId>| PipelineEvidence {
            repo,
            pipeline: "adaptive".into(),
            runs: 4,
            scored: 4,
            pass_rate: 0.5,
            blocked: 0,
            escalated: 0,
            review_prs: None,
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
        assert!(
            text.contains("\nPipeline web / adaptive n=4 scored=4"),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nPipeline {gone} / adaptive")),
            "{text}"
        );
        assert!(text.contains("\nPipeline global / adaptive"), "{text}");
    }
}
