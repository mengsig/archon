//! The review archond verified for a run at its review gate: the branch head
//! its counted passes were held to and that head's tree. Kept in `kv` under
//! [`PREFIX`]`<run>`, so a run from before it was recorded simply has none.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use serde::{Deserialize, Serialize};

use super::codec::{SqlContext, write_tx};
use super::{Store, runs, settings};

/// `kv` key prefix: + run id → the run's verified review as JSON.
const PREFIX: &str = "run_review:";

/// The commit and tree a run's verified review passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReview {
    pub head: String,
    pub tree: String,
}

impl RunReview {
    fn is_valid(&self) -> bool {
        is_full_id(&self.head) && is_full_id(&self.tree)
    }
}

/// Whether `id` is a full lowercase SHA-1 or SHA-256 object id.
fn is_full_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Store {
    /// Record that archond verified `run`'s review at `review`, replacing an
    /// earlier one of the run: a later gate pass is what the run last held.
    pub fn record_run_review(
        &mut self,
        run: &PipelineRunId,
        review: &RunReview,
    ) -> Result<(), ArchonError> {
        if !review.is_valid() {
            return Err(ArchonError::Invalid(format!(
                "a verified review names full lowercase ids, not {review:?}"
            )));
        }
        let value = serde_json::to_string(review).expect("two strings serialize");
        let tx = write_tx(&mut self.conn)?;
        runs::load(&tx, run)?;
        settings::kv_set(&tx, &format!("{PREFIX}{run}"), &value)?;
        tx.commit().ctx("commit a run's verified review")
    }

    /// The review archond verified for `run`; `None` when it verified none or
    /// the run predates the record.
    pub fn run_review(&self, run: &PipelineRunId) -> Result<Option<RunReview>, ArchonError> {
        let Some(raw) = settings::kv_get(&self.conn, &format!("{PREFIX}{run}"))? else {
            return Ok(None);
        };
        let review: RunReview = serde_json::from_str(&raw).map_err(|e| {
            ArchonError::Store(format!(
                "run {run} records a malformed verified review: {e}"
            ))
        })?;
        if !review.is_valid() {
            return Err(ArchonError::Store(format!(
                "run {run} records a verified review with malformed ids: {review:?}"
            )));
        }
        Ok(Some(review))
    }
}
