//! The change stream and item history (CONTRACTS.md §1.3, owner AR-C):
//! `subscribe_changes`, `changes_page`, `item_history` and the `as_of` of
//! `list_item_summaries`. Until served, each answers `unavailable`.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{ItemQuery, Reply};

use crate::context::DaemonContext;

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[];

fn not_yet(command: &str) -> ArchonError {
    ArchonError::Unavailable(format!(
        "{command} is not implemented yet: this archond does not serve the `changes` feature"
    ))
}

pub(super) async fn subscribe(
    _ctx: &DaemonContext,
    _after: Option<u64>,
) -> Result<Reply, ArchonError> {
    Err(not_yet("subscribe_changes"))
}

pub(super) async fn page(
    _ctx: &DaemonContext,
    _after: u64,
    _limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    Err(not_yet("changes_page"))
}

pub(super) async fn item_history(
    _ctx: &DaemonContext,
    _item: &QueueItemId,
    _limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    Err(not_yet("item_history"))
}

/// `list_item_summaries` with `with_as_of`: the summaries and their `as_of`.
pub(super) async fn summaries_at(
    _ctx: &DaemonContext,
    query: &ItemQuery,
) -> Result<Reply, ArchonError> {
    assert!(query.with_as_of, "routed only for with_as_of");
    Err(not_yet("list_item_summaries with_as_of"))
}
