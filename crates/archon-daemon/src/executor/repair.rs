//! One automatic repair retry for a run that failed only because the gate
//! refused the review or ship record its finished work owes. It does what the
//! orchestrator does by hand with `retry_item` and a note: same worktree, same
//! branch, the refusal quoted in the brief. Coverage and verdict rules are
//! untouched; the retried agent still has to produce the record.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use archon_core::steer::RunEdit;
use archon_pipeline::record_refusal;
use archon_queue::Store;

use crate::context::DaemonContext;

/// Opens every repair note, and so marks an item whose one repair is spent.
const NOTE_OPENING: &str = "Automatic repair (archond retries a refused record once).";

/// What to do with a run that is about to finish.
pub(super) enum Decision {
    /// Finish it as usual.
    Finish,
    /// Queue the item again with this repair.
    Repair(Repair),
    /// Its one repair was spent: finish it, naming the refusal that came first.
    Spent { first: String },
}

pub(super) struct Repair {
    pub item: QueueItemId,
    pub refusal: String,
}

impl Repair {
    pub(super) fn note(&self) -> String {
        format!(
            "{NOTE_OPENING} The gate refused your finished work: {}\n\nRepair the record only. \
             Do not change code unless a reviewer finds a defect. Delegate one reviewer over the \
             whole change from the merge base, copy its agent id exactly, and return the review \
             object and ship record the gate requires.",
            self.refusal
        )
    }

    /// The feed line that makes the repair visible.
    pub(super) fn feed_text(&self) -> String {
        format!(
            "review gate refused {}: {}; retrying once to repair the record",
            self.item, self.refusal
        )
    }

    /// Amend the brief and queue the failed item again, like `retry_item`.
    pub(super) fn requeue(&self, store: &mut Store) -> Result<QueueItem, ArchonError> {
        let now = archon_core::now();
        let mut item = store.item(&self.item)?;
        assert!(
            matches!(item.state, QueueState::Failed { .. }),
            "only the failed item is requeued"
        );
        item.amend_brief(&self.note(), now)?;
        store.update_item(&item)?;
        store.transition_item(&item.id, QueueState::Queued, now)
    }
}

/// Whether `run`, about to finish in `state`, gets its one repair retry.
pub(super) async fn decide(
    ctx: &DaemonContext,
    run: &PipelineRun,
    state: &RunState,
) -> Result<Decision, ArchonError> {
    let RunState::Failed { error, .. } = state else {
        return Ok(Decision::Finish);
    };
    let Some(refusal) = record_refusal(error) else {
        return Ok(Decision::Finish);
    };
    let store = ctx.store.lock().await;
    let item = store.item(&run.item)?;
    if !matches!(&item.state, QueueState::Running { run: live } if live == &run.id) {
        return Ok(Decision::Finish);
    }
    if item.brief.contains(NOTE_OPENING) {
        return spent(&store, run);
    }
    let eligible = ctx.config.review.auto_repair
        && !steered(&store, run)?
        && !store.has_pending_stack_fix(&item.id)?;
    if !eligible {
        return Ok(Decision::Finish);
    }
    Ok(Decision::Repair(Repair {
        item: item.id,
        refusal: refusal.to_owned(),
    }))
}

/// A run edit by a person (a steer) changed the run's shape; the daemon's
/// own role assignment did not.
fn steered(store: &Store, run: &PipelineRun) -> Result<bool, ArchonError> {
    let edits = store.run_edits(&run.id)?;
    Ok(edits
        .iter()
        .any(|e| !matches!(e.edit, RunEdit::AssignRoles { .. })))
}

/// The refusal of the run before `run` on its item, which the repair answered.
fn spent(store: &Store, run: &PipelineRun) -> Result<Decision, ArchonError> {
    let runs = store.runs_for_item(&run.item)?;
    let first = runs
        .iter()
        .take_while(|earlier| earlier.id != run.id)
        .filter_map(|earlier| match &earlier.state {
            RunState::Failed { error, .. } => record_refusal(error),
            _ => None,
        })
        .last();
    Ok(match first {
        Some(first) => Decision::Spent {
            first: first.to_owned(),
        },
        None => Decision::Finish,
    })
}

/// `state` with the first refusal named after the second one.
pub(super) fn with_first_refusal(state: RunState, first: &str) -> RunState {
    let RunState::Failed { stage_id, error } = state else {
        panic!("only a failed run has a refusal to extend")
    };
    let error = format!(
        "{error}\n\nThe automatic repair retry was refused too. The first refusal was: {first}"
    );
    RunState::Failed { stage_id, error }
}
