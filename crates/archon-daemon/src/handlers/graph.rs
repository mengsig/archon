//! The task graph, run plans and the `list_items` filters (CONTRACTS.md
//! §1.3, owner AR-G). Until served, each answers `unavailable`.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::{ItemQuery, Reply, TaskFilter};

use crate::context::DaemonContext;

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[];

pub(super) async fn task_graph(
    _ctx: &DaemonContext,
    _filter: TaskFilter,
    _limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    Err(ArchonError::Unavailable(
        "task_graph is not implemented yet: this archond does not serve the `task_graph` feature"
            .into(),
    ))
}

pub(super) async fn run_plan(
    _ctx: &DaemonContext,
    _run: &PipelineRunId,
) -> Result<Reply, ArchonError> {
    Err(ArchonError::Unavailable(
        "run_plan is not implemented yet: this archond does not serve the `run_plan` feature"
            .into(),
    ))
}

/// Refuse a query whose protocol 24 filters this archond cannot apply yet,
/// rather than answer as if they were not there.
pub(super) fn check_item_filters(query: &ItemQuery) -> Result<(), ArchonError> {
    if !query.has_item_filters() {
        return Ok(());
    }
    Err(ArchonError::Unavailable(
        "the updated_since, before and text filters are not implemented yet: \
         this archond does not serve the `item_filters` feature"
            .into(),
    ))
}
