//! One automatic repair retry for a run that failed only because the gate
//! refused the review or ship record its finished work owes, or the delivery
//! refused what the record stage can fix in its own worktree. It does what
//! the orchestrator does by hand with `retry_item` and a note: same worktree,
//! same branch, the refusal quoted in the brief, and asked only for what was
//! refused. Coverage, verdict and identity rules are untouched; the retried
//! agent still has to produce the record.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use archon_core::steer::RunEdit;
use archon_pipeline::ship::SHIP_STEP;
use archon_pipeline::{record_refusal, refuses_ship_record};
use archon_queue::Store;

use super::{RunTable, ship};
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

/// What was refused, which decides what the repair asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    /// The ship record alone: the committed work stands.
    Record,
    /// The review on record.
    Review,
    /// The delivery, for something the record stage can fix in its worktree.
    Delivery,
}

pub(super) struct Repair {
    pub item: QueueItemId,
    pub refusal: String,
    pub refused: Refused,
    /// Whether the record stage owes a review, which the retry must show again.
    pub owes_review: bool,
}

impl Repair {
    pub(super) fn note(&self) -> String {
        let refusal = &self.refusal;
        assert!(!refusal.trim().is_empty(), "a repair quotes its refusal");
        let review = match (self.refused, self.owes_review) {
            (Refused::Review, _) | (_, false) => "",
            (Refused::Record, true) => {
                " Archon counts only a review by an agent this run delegates, so delegate one \
                 reviewer over the whole change from the merge base as the branch holds it, copy \
                 its agent id exactly, and hand in its review object beside the record."
            }
            (Refused::Delivery, true) => {
                " Your fix changes the commits, so delegate one reviewer over the whole change \
                 from the merge base, copy its agent id exactly, and hand in its review object \
                 beside the record."
            }
        };
        match self.refused {
            Refused::Review => format!(
                "{NOTE_OPENING} The gate refused your finished work: {refusal}\n\nRepair the \
                 record only. Do not change code unless a reviewer finds a defect. Delegate one \
                 reviewer over the whole change from the merge base, copy its agent id exactly, \
                 and return the review object and ship record the gate requires."
            ),
            Refused::Record => format!(
                "{NOTE_OPENING} The gate refused only the ship record your finished work handed \
                 in: {refusal}\n\nYour committed work stands: change no code and redo nothing. \
                 Read `git rev-parse HEAD` and `git rev-parse HEAD^{{tree}}` after your last \
                 commit and hand in the ship record with those full hashes.{review}"
            ),
            Refused::Delivery => format!(
                "{NOTE_OPENING} Archon refused to deliver your finished work: {refusal}\n\nFix \
                 only what that names, in this worktree and on this branch, and commit the fix \
                 under the repository's own git identity, never one you set. Then hand in a new \
                 ship record read after your last commit.{review}"
            ),
        }
    }

    /// The feed line that makes the repair visible.
    pub(super) fn feed_text(&self) -> String {
        let (who, what) = match self.refused {
            Refused::Record | Refused::Review => ("review gate", "the record"),
            Refused::Delivery => ("delivery", "it"),
        };
        format!(
            "{who} refused {}: {}; retrying once to repair {what}",
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

/// The refusal a failed run's `state` reports that a repair may answer, and
/// its kind; `None` for every other end.
fn refusal_of(state: &RunState) -> Option<(Refused, &str)> {
    let RunState::Failed { stage_id, error } = state else {
        return None;
    };
    if stage_id == SHIP_STEP {
        return ship::repairable_refusal(error).then_some((Refused::Delivery, error.trim()));
    }
    let refusal = record_refusal(error)?;
    let refused = match refuses_ship_record(refusal) {
        true => Refused::Record,
        false => Refused::Review,
    };
    Some((refused, refusal))
}

/// Whether the stage `state` blames, or for a delivery the record stage,
/// owes a review, as `run`'s driver has it; without one, assume it does.
fn owes_review(table: &RunTable, run: &PipelineRun, state: &RunState) -> bool {
    let RunState::Failed { stage_id, .. } = state else {
        panic!("only a failed run is repaired")
    };
    let Some(driver) = table.drivers.get(&run.id) else {
        return true;
    };
    let stage = match stage_id.as_str() {
        SHIP_STEP => driver.spec().ship_record_stage().map(|s| s.id.as_str()),
        stage => Some(stage),
    };
    stage.is_none_or(|stage| driver.reviewers_owed(stage) > 0)
}

/// Whether `run`, about to finish in `state`, gets its one repair retry.
pub(super) async fn decide(
    ctx: &DaemonContext,
    table: &RunTable,
    run: &PipelineRun,
    state: &RunState,
) -> Result<Decision, ArchonError> {
    let Some((refused, refusal)) = refusal_of(state) else {
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
        refused,
        owes_review: owes_review(table, run, state),
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
        .filter_map(|earlier| refusal_of(&earlier.state).map(|(_, refusal)| refusal))
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
