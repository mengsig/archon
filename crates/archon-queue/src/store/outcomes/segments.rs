use super::super::{
    Store,
    codec::{SqlContext, to_json},
};
use archon_core::{ArchonError, Timestamp, id::StageRunId, model::ModelChoice, run::ModelSegment};

impl Store {
    pub fn record_model_segment(
        &self,
        id: &StageRunId,
        delivery: &str,
        choice: &ModelChoice,
        at: Timestamp,
        estimates: bool,
    ) -> Result<(), ArchonError> {
        let mut stage = self.stage_run(id)?;
        if stage.model_segments.iter().any(|s| s.delivery == delivery) {
            return Ok(());
        }
        if stage.state.is_terminal() || delivery.is_empty() || at < stage.started_at {
            return Err(ArchonError::Invalid(
                "model segment requires a live stage, delivery key and valid start".into(),
            ));
        }
        if stage.current_choice() == choice {
            return Ok(());
        }
        let cost = super::capture::stage_charge(self, &stage, estimates)?;
        assert!(cost.is_finite() && cost >= 0.0);
        stage.model_segments.push(ModelSegment {
            delivery: delivery.into(),
            choice: choice.clone(),
            started_at: at,
            cost_before_usd: cost,
        });
        let changed = self
            .conn
            .execute(
                "UPDATE stage_runs SET data=?2 WHERE id=?1",
                rusqlite::params![id.as_str(), to_json(&stage, "model segment")?],
            )
            .ctx("record model segment")?;
        assert_eq!(changed, 1);
        Ok(())
    }
}

pub(super) fn observations(
    stage: &archon_core::run::StageRun,
    total: f64,
) -> Result<Vec<archon_core::outcome::OutcomeStage>, ArchonError> {
    assert!(total.is_finite() && total >= 0.0);
    assert!(stage.attempt > 0);
    let mut start = stage.started_at;
    let mut choice = &stage.choice;
    let mut charged = 0.0_f64;
    let mut values = Vec::new();
    let end = stage.finished_at.unwrap_or(start);
    for next in &stage.model_segments {
        let boundary = next.cost_before_usd.min(total).max(charged);
        values.push(observation(
            stage,
            choice,
            boundary - charged,
            start,
            next.started_at,
            None,
        )?);
        charged = boundary;
        choice = &next.choice;
        start = next.started_at;
    }
    let passed = match stage.state {
        _ if stage.infrastructure_failure => None,
        archon_core::run::StageState::Cancelled | archon_core::run::StageState::TimedOut => None,
        // A pass whose review Archon refused failed the stage's own gate.
        archon_core::run::StageState::Completed => Some(!stage.review_refused()),
        _ => Some(false),
    };
    values.push(observation(
        stage,
        choice,
        total - charged,
        start,
        end.max(start),
        passed,
    )?);
    Ok(values)
}

fn observation(
    stage: &archon_core::run::StageRun,
    choice: &ModelChoice,
    cost: f64,
    start: Timestamp,
    end: Timestamp,
    passed: Option<bool>,
) -> Result<archon_core::outcome::OutcomeStage, ArchonError> {
    let wall_secs = u64::try_from((end - start).num_seconds())
        .map_err(|_| ArchonError::Store("model segment ends before it starts".into()))?;
    assert!(cost >= 0.0 && cost.is_finite());
    assert!(stage.attempt > 0);
    Ok(archon_core::outcome::OutcomeStage {
        stage: stage.stage_id.clone(),
        choice: choice.clone(),
        cost_usd: cost,
        wall_secs,
        attempts: 1,
        passed,
    })
}
