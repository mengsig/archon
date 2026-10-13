//! `steer_item`: change a queued or running item without cancelling it.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::Reply;
use archon_core::steer::Steer;

use super::not_implemented;
use crate::context::DaemonContext;

/// Replies `Reply::Item` with the item as steered.
pub(super) async fn steer(
    _ctx: &DaemonContext,
    _item: &QueueItemId,
    _steer: Steer,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("steer_item"))
}
