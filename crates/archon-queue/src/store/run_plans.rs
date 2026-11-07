//! The plan a run finished with, as `run_plan` serves it. Kept in `kv` under
//! [`PREFIX`]`<run>` in the transaction that finishes the run and pruned with
//! the run's history, so a run that finished before plans were kept has none.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::RunPlan;
use rusqlite::Connection;

use super::{Store, settings};

/// `kv` key prefix: + run id → the run's finished plan as JSON.
pub(super) const PREFIX: &str = "run_plan:";

/// Within `finish`'s transaction: keep `plan` as `run`'s finished plan,
/// replacing any earlier one, or forget an earlier one when it has none.
pub(super) fn keep(
    tx: &Connection,
    run: &PipelineRunId,
    plan: Option<&RunPlan>,
) -> Result<(), ArchonError> {
    let key = format!("{PREFIX}{run}");
    let Some(plan) = plan else {
        return settings::kv_delete(tx, &key);
    };
    assert!(plan.finished, "only a finished run's plan is kept");
    assert_eq!(&plan.run, run, "a plan is kept under its run");
    let value = serde_json::to_string(plan).expect("a run plan encodes");
    settings::kv_set(tx, &key, &value)
}

impl Store {
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
