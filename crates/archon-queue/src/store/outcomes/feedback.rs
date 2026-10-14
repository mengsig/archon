use super::super::{
    Store,
    codec::{SqlContext, from_json, to_json},
    settings,
};
use super::{correct, load, write};
use archon_core::{ArchonError, id::PipelineRunId, outcome::Correction};

impl Store {
    pub fn record_feedback(
        &self,
        run: &PipelineRunId,
        event: archon_core::outcome::CorrectionEvent,
    ) -> Result<(), ArchonError> {
        use archon_core::outcome::{CorrectionCause, CorrectionEvent};
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let stored = self.run(run)?;
        if stored.item != event.item || event.reason.trim().is_empty() || event.reason.len() > 1000
        {
            return Err(ArchonError::Invalid(
                "correction needs its run's item and a reason of 1..1000 bytes".into(),
            ));
        }
        if !self
            .stage_runs(run)?
            .iter()
            .any(|s| s.stage_id == event.stage)
        {
            return Err(ArchonError::Invalid(format!(
                "correction stage {:?} is absent from run {run}",
                event.stage
            )));
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin attributed correction")?;
        let key = format!("learning/feedback/{run}");
        let mut feedback: Vec<CorrectionEvent> = settings::kv_get(&tx, &key)?
            .map(|raw| from_json(&raw, "attributed corrections"))
            .transpose()?
            .unwrap_or_default();
        if feedback.contains(&event) {
            return Ok(());
        }
        if feedback.len() >= 100 {
            return Err(ArchonError::Invalid(
                "correction limit reached for this run".into(),
            ));
        }
        assert_eq!(stored.item, event.item);
        if event.cause == CorrectionCause::Quality {
            correct(&tx, run, Correction::OperatorContradiction)?;
        }
        feedback.push(event);
        settings::kv_set(&tx, &key, &to_json(&feedback, "attributed corrections")?)?;
        if let Some(mut outcome) = load(&tx, run)? {
            outcome.feedback = feedback;
            assert_eq!(outcome.item, stored.item);
            write(&tx, &outcome)?;
        }
        tx.commit().ctx("commit attributed correction")
    }
}
