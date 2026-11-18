use super::super::Store;
use super::super::codec::{SqlContext, from_json};
use archon_core::cost::DayRange;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::lesson::{LessonStats, PipelineEvidence, ReviewPrs};
use archon_core::outcome::{FailureClass, Outcome, OutcomeResult};
use rusqlite::params;
use std::collections::BTreeMap;

impl Store {
    pub fn learning_stats(&self, repo: Option<&RepoId>) -> Result<LessonStats, ArchonError> {
        let outcomes = self.reviewed_outcomes(repo, None, None)?;
        let mut stats = LessonStats {
            outcomes: outcomes.len() as u64,
            repo_names: self.repos()?.into_iter().map(|r| (r.id, r.name)).collect(),
            ..Default::default()
        };
        for (outcome, _) in &outcomes {
            stats.corrected += u64::from(!outcome.corrections.is_empty());
            stats.merged += u64::from(outcome.merged == Some(true));
        }
        stats.pipelines = pipelines(&outcomes);
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
        stats.holdout_lift = super::super::lessons::holdout_lift(&self.conn, repo)?;
        assert!(stats.corrected <= stats.outcomes);
        Ok(stats)
    }
}

/// A stored outcome and the pull request it reviewed, if any.
type Reviewed = (Outcome, Option<String>);

impl Store {
    /// Per-pipeline lines of the runs that finished within `range` (local
    /// days), as the model evidence report shows them.
    pub fn pipeline_lines(
        &self,
        repo: Option<&RepoId>,
        pipeline: Option<&str>,
        range: &DayRange,
    ) -> Result<Vec<PipelineEvidence>, ArchonError> {
        if range.since > range.until {
            return Err(ArchonError::Invalid(
                "evidence range starts after it ends".into(),
            ));
        }
        let outcomes = self.reviewed_outcomes(repo, pipeline, Some(range))?;
        let lines = pipelines(&outcomes);
        assert!(
            lines.iter().all(|l| l.runs > 0),
            "a line has a run in range"
        );
        Ok(lines)
    }

    /// Outcomes oldest first, each with the pull request its item reviews.
    fn reviewed_outcomes(
        &self,
        repo: Option<&RepoId>,
        pipeline: Option<&str>,
        range: Option<&DayRange>,
    ) -> Result<Vec<Reviewed>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT o.data FROM outcomes o
                 WHERE (?1 IS NULL OR o.repo = ?1) AND (?2 IS NULL OR o.pipeline = ?2)
                   AND (?3 IS NULL OR date(o.finished_at, 'localtime') BETWEEN ?3 AND ?4)
                 ORDER BY o.finished_at, o.run",
            )
            .ctx("prepare pipeline outcomes")?;
        let rows = stmt
            .query_map(
                params![
                    repo.map(RepoId::as_str),
                    pipeline,
                    range.map(|r| r.since.to_string()),
                    range.map(|r| r.until.to_string()),
                ],
                |r| r.get::<_, String>(0),
            )
            .ctx("query pipeline outcomes")?;
        let mut values = Vec::new();
        for row in rows {
            let data = row.ctx("read pipeline outcome")?;
            let outcome = self.settled_outcome(from_json(&data, "outcome")?)?;
            let pr = outcome.review_pr.clone();
            values.push((outcome, pr));
        }
        assert!(
            values
                .iter()
                .all(|(o, _)| repo.is_none_or(|r| o.repo.as_ref() == Some(r)))
        );
        Ok(values)
    }
}

/// One line per (repository, pipeline), in that order.
fn pipelines(outcomes: &[Reviewed]) -> Vec<PipelineEvidence> {
    let mut groups = BTreeMap::new();
    for value in outcomes {
        let key = (value.0.repo.clone(), value.0.pipeline.clone());
        groups.entry(key).or_insert_with(Vec::new).push(value);
    }
    let lines: Vec<_> = groups
        .into_iter()
        .map(|((repo, pipeline), values)| pipeline_evidence(repo, pipeline, &values))
        .collect();
    assert_eq!(
        lines.iter().map(|l| l.runs).sum::<u64>(),
        outcomes.len() as u64
    );
    lines
}

/// Whether a run counts as a pass or a loss of its pipeline: it judged the
/// work, and no stage handed it to the operator or another pipeline.
fn line_scored(outcome: &Outcome) -> bool {
    let handed_on = matches!(
        outcome.failure,
        Some(FailureClass::Blocked | FailureClass::Escalated)
    );
    outcome.is_scored() && !handed_on
}

/// One pipeline's runs in one repository. Its pass rate is of the runs that
/// judged the work: a cancelled, capped, timed-out, blocked or escalated run
/// is neither pass nor fail.
fn pipeline_evidence(
    repo: Option<RepoId>,
    pipeline: String,
    values: &[&Reviewed],
) -> PipelineEvidence {
    assert!(!values.is_empty());
    assert!(
        values
            .iter()
            .all(|(o, _)| o.repo == repo && o.pipeline == pipeline)
    );
    let class = |wanted: FailureClass| {
        values
            .iter()
            .filter(|(o, _)| o.failure == Some(wanted))
            .count() as u64
    };
    let mut costs: Vec<_> = values.iter().map(|(o, _)| o.cost_usd).collect();
    let mut walls: Vec<_> = values.iter().map(|(o, _)| o.wall_secs as f64).collect();
    let scored: Vec<_> = values.iter().filter(|(o, _)| line_scored(o)).collect();
    let passed = scored
        .iter()
        .filter(|(o, _)| o.result == OutcomeResult::Succeeded)
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
        blocked: class(FailureClass::Blocked),
        escalated: class(FailureClass::Escalated),
        review_prs: review_prs(&scored),
        correction_rate: values
            .iter()
            .filter(|(o, _)| !o.corrections.is_empty())
            .count() as f64
            / values.len() as f64,
        median_cost_usd: median(&mut costs),
        median_wall_secs: median(&mut walls),
        estimated_saving_usd: None,
    }
}

/// Each reviewed pull request once: whether any scored run passed it, and
/// how many scored runs it took to; `None` when no run reviewed one.
fn review_prs(scored: &[&&Reviewed]) -> Option<ReviewPrs> {
    let mut prs: BTreeMap<&str, (u64, Option<u64>)> = BTreeMap::new();
    for (outcome, pr) in scored.iter().map(|v| (&v.0, v.1.as_deref())) {
        let Some(pr) = pr else { continue };
        let (rounds, passed_at) = prs.entry(pr).or_default();
        *rounds += 1;
        if outcome.result == OutcomeResult::Succeeded && passed_at.is_none() {
            *passed_at = Some(*rounds);
        }
    }
    if prs.is_empty() {
        return None;
    }
    let mut rounds: Vec<f64> = prs
        .values()
        .filter_map(|(_, at)| at.map(|n| n as f64))
        .collect();
    let passed = rounds.len() as u64;
    assert!(passed <= prs.len() as u64);
    Some(ReviewPrs {
        reviewed: prs.len() as u64,
        passed,
        median_rounds_to_pass: (!rounds.is_empty()).then(|| median(&mut rounds)),
    })
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
