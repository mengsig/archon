//! The plan a run finished with, as `run_plan` serves it. Kept in `kv` under
//! [`PREFIX`]`<run>` as the run finishes and pruned with the run's history,
//! so a run that finished before plans were kept simply has none.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::RunPlan;

use super::codec::{SqlContext, write_tx};
use super::{Store, runs, settings};

/// `kv` key prefix: + run id → the run's finished plan as JSON.
pub(super) const PREFIX: &str = "run_plan:";

impl Store {
    /// Keep `plan` as its run's finished plan, replacing an earlier one.
    pub fn keep_run_plan(&mut self, plan: &RunPlan) -> Result<(), ArchonError> {
        assert!(plan.finished, "only a finished run's plan is kept");
        assert!(!plan.stages.is_empty(), "a run keeps a stage");
        let value = serde_json::to_string(plan).expect("a run plan encodes");
        let tx = write_tx(&mut self.conn)?;
        runs::load(&tx, &plan.run)?;
        settings::kv_set(&tx, &format!("{PREFIX}{}", plan.run), &value)?;
        tx.commit().ctx("commit a run's finished plan")
    }

    /// The plan `run` finished with; `None` when none was kept.
    pub fn kept_run_plan(&self, run: &PipelineRunId) -> Result<Option<RunPlan>, ArchonError> {
        let Some(raw) = settings::kv_get(&self.conn, &format!("{PREFIX}{run}"))? else {
            return Ok(None);
        };
        let plan: RunPlan = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("run {run} keeps a malformed plan: {e}")))?;
        if &plan.run != run || !plan.finished {
            return Err(ArchonError::Store(format!(
                "run {run} keeps the plan of run {} (finished: {})",
                plan.run, plan.finished
            )));
        }
        Ok(Some(plan))
    }
}
