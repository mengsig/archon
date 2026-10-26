use super::super::Store;
use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::lesson::{LessonStats, PipelineEvidence};
use archon_core::outcome::{Outcome, OutcomeResult};
use std::collections::BTreeMap;

impl Store {
    pub fn learning_stats(&self, repo: Option<&RepoId>) -> Result<LessonStats, ArchonError> {
        let outcomes = self.outcomes(repo)?;
        let mut stats = LessonStats {
            outcomes: outcomes.len() as u64,
            repo_names: self.repos()?.into_iter().map(|r| (r.id, r.name)).collect(),
            ..Default::default()
        };
        let mut groups = BTreeMap::new();
        for outcome in &outcomes {
            stats.corrected += u64::from(!outcome.corrections.is_empty());
            stats.merged += u64::from(outcome.merged == Some(true));
            groups
                .entry((outcome.repo.clone(), outcome.pipeline.clone()))
                .or_insert_with(Vec::new)
                .push(outcome);
        }
        for ((repo, pipeline), values) in groups {
            stats
                .pipelines
                .push(pipeline_evidence(repo, pipeline, &values));
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT state,COUNT(*) FROM lessons WHERE (?1 IS NULL OR repo=?1) GROUP BY state",
            )
            .ctx("prepare lesson counts")?;
        let rows = stmt
            .query_map([repo.map(RepoId::as_str)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
            })
            .ctx("query lesson counts")?;
        for row in rows {
            let (state, count) = row.ctx("read lesson count")?;
            let count = u64::from(count);
            match state.as_str() {
                "active" => stats.active = count,
                "proposed" => stats.proposed = count,
                "retracted" => stats.retracted = count,
                _ => return Err(ArchonError::Store("unknown lesson state".into())),
            }
        }
        assert!(stats.corrected <= stats.outcomes);
        Ok(stats)
    }
}

/// One pipeline's runs in one repository. Its pass rate is of the runs that
/// judged the work: a cancelled, capped or timed-out run is neither pass nor fail.
fn pipeline_evidence(
    repo: Option<RepoId>,
    pipeline: String,
    values: &[&Outcome],
) -> PipelineEvidence {
    assert!(!values.is_empty());
    assert!(
        values
            .iter()
            .all(|o| o.repo == repo && o.pipeline == pipeline)
    );
    let mut costs: Vec<_> = values.iter().map(|o| o.cost_usd).collect();
    let mut walls: Vec<_> = values.iter().map(|o| o.wall_secs as f64).collect();
    let scored: Vec<_> = values.iter().filter(|o| o.is_scored()).collect();
    let passed = scored
        .iter()
        .filter(|o| o.result == OutcomeResult::Succeeded)
        .count();
    PipelineEvidence {
        repo,
        pipeline,
        runs: values.len() as u64,
        scored: scored.len() as u64,
        pass_rate: if scored.is_empty() {
            0.0
        } else {
            passed as f64 / scored.len() as f64
        },
        correction_rate: values.iter().filter(|o| !o.corrections.is_empty()).count() as f64
            / values.len() as f64,
        median_cost_usd: median(&mut costs),
        median_wall_secs: median(&mut walls),
        estimated_saving_usd: None,
    }
}

pub(crate) fn median(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty());
    assert!(values.iter().all(|v| v.is_finite() && *v >= 0.0));
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        values[mid - 1] / 2.0 + values[mid] / 2.0
    } else {
        values[mid]
    }
}
