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
    /// Keep `plan` as `run`'s finished plan, replacing an earlier one; with
    /// no plan, forget an earlier one, so none outlives the finish it was for.
    pub fn keep_run_plan(
        &mut self,
        run: &PipelineRunId,
        plan: Option<&RunPlan>,
    ) -> Result<(), ArchonError> {
        let key = format!("{PREFIX}{run}");
        let tx = write_tx(&mut self.conn)?;
        runs::load(&tx, run)?;
        match plan {
            Some(plan) => {
                assert!(plan.finished, "only a finished run's plan is kept");
                assert_eq!(&plan.run, run, "a plan is kept under its run");
                let value = serde_json::to_string(plan).expect("a run plan encodes");
                settings::kv_set(&tx, &key, &value)?;
            }
            None => settings::kv_delete(&tx, &key)?,
        }
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
