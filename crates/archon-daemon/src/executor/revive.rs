//! Reviving a failed item on its own branch (ARCHITECTURE §6.6). A stage
//! that escalates, or fails in any other way, ends its run. A steer that
//! inserts a stage or switches the pipeline says how the item goes on, and
//! the item is queued again without a fresh start: its next run works in the
//! worktree and on the branch of the failed one, with that run's commits.

use archon_core::error::ArchonError;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use archon_core::steer::RunEdit;

use super::clip;

/// How much of the failed stage's error the next run's brief repeats.
const ERROR_CHARS: usize = 600;

/// How `edit` lets a failed run's item go on, for its brief.
fn goes_on(edit: &RunEdit) -> Result<String, ArchonError> {
    match edit {
        RunEdit::InsertStage { spec, .. } => {
            let stage = spec.get("id").and_then(|id| id.as_str()).unwrap_or("new");
            Ok(format!("with stage `{stage}` added to its pipeline"))
        }
        RunEdit::SwitchPipeline { pipeline, .. } => {
            Ok(format!("with the stages of pipeline `{pipeline}`"))
        }
        RunEdit::AbortStage { .. } => Err(ArchonError::Invalid(
            "a failed run has no stage left to abort; revive its item with insert_stage or \
             switch_pipeline"
                .into(),
        )),
        RunEdit::AssignRoles { .. } => Err(ArchonError::Invalid(
            "a swarm plan's roles are assigned by the daemon, not by a steer".into(),
        )),
    }
}

/// `item`, which failed with run `failed`, as its next run takes it after
/// shape edit `edit`: its brief names the failure and the branch to go on.
pub(crate) fn revived(
    failed: &PipelineRun,
    edit: &RunEdit,
    mut item: QueueItem,
) -> Result<QueueItem, ArchonError> {
    assert_eq!(failed.item, item.id, "an item is revived from its own run");
    let latest =
        matches!(&item.state, QueueState::Failed { run: Some(run), .. } if *run == failed.id);
    let RunState::Failed { stage_id, error } = &failed.state else {
        return Err(ArchonError::InvalidTransition(format!(
            "run {} did not fail, so there is nothing to revive",
            failed.id
        )));
    };
    if !latest {
        return Err(ArchonError::InvalidTransition(format!(
            "item {} is {}, not failed with run {}",
            item.id,
            item.state.kind(),
            failed.id
        )));
    }
    let continuation = format!(
        "This item continues {} after its run failed at stage `{stage_id}`: {}\nThe work that \
         run committed is on branch `{}` in this worktree. Continue from it; do not start over.",
        goes_on(edit)?,
        clip(error.trim(), ERROR_CHARS),
        failed.branch
    );
    let fresh_retries = item.fresh_retries;
    item.amend_brief(&continuation, archon_core::now())?;
    assert_eq!(
        item.fresh_retries, fresh_retries,
        "the item stays on its branch"
    );
    Ok(item)
}
