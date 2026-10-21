//! Discarding a settled item's experiments directory (ARCHITECTURE §13.5)
//! once nothing Archon started for the item can still write there.

use std::collections::BTreeSet;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_queue::Store;

use super::{FollowUp, descendants, run_node};
use crate::context::DaemonContext;
use crate::experiments;

/// Whether `item` settled and nothing of it may run: no live or `in_flight` stage, node to
/// retire or owed descendant stop. All but `in_flight` is stored, so gc asks it too.
pub(crate) fn item_stopped(
    store: &Store,
    in_flight: &BTreeSet<StageRunId>,
    item: &QueueItemId,
) -> Result<bool, ArchonError> {
    if !store.item(item)?.state.is_terminal() {
        return Ok(false);
    }
    let runs = store.runs_for_item(item)?;
    let ids: BTreeSet<&PipelineRunId> = runs.iter().map(|run| &run.id).collect();
    for stage in store.stage_runs_for_item(item)? {
        assert!(ids.contains(&stage.run), "an item's stages are its runs'");
        if !stage.state.is_terminal() || in_flight.contains(&stage.id) {
            return Ok(false);
        }
    }
    for run in &runs {
        assert_eq!(&run.item, item, "the store returns the item's runs");
        let live = !run.state.is_terminal();
        if live || run_node::has_node(store, &run.id)? || descendants::holds(store, &run.id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Set aside and delete the experiments directory of `run`'s item once the
/// item has stopped; until then a later stop, settle or gc pass tries again.
pub(super) async fn discard(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let aside = {
        let table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        let finished = store.run(run)?;
        assert_eq!(&finished.id, run, "the store returns the run asked for");
        match item_stopped(&store, &table.in_flight, &finished.item) {
            Ok(true) => {}
            Ok(false) => return Ok(FollowUp::default()),
            Err(error) if !error.is_store() => {
                tracing::warn!(%run, %error, "experiments kept: the item's records are unreadable");
                return Ok(FollowUp::default());
            }
            Err(error) => return Err(error),
        }
        assert!(
            !table.drivers.contains_key(run),
            "a finished run has no driver"
        );
        experiments::set_aside(&ctx.paths, &finished.item)
    };
    if let Some(aside) = aside {
        experiments::delete(&ctx.paths, aside).await;
    }
    Ok(FollowUp::default())
}
