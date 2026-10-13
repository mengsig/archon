//! Initiatives: several items built against one shared contract.

use archon_core::error::ArchonError;
use archon_core::id::InitiativeId;
use archon_core::initiative::EnqueueInitiative;
use archon_core::protocol::Reply;

use super::not_implemented;
use crate::context::DaemonContext;

/// Replies `Reply::Initiative` with the initiative and the items it made.
pub(super) async fn enqueue(
    _ctx: &DaemonContext,
    _request: EnqueueInitiative,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("enqueue_initiative"))
}

pub(super) async fn get(
    _ctx: &DaemonContext,
    _initiative: &InitiativeId,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("get_initiative"))
}

/// Replies `Reply::Initiative` once live members were told of the addition.
pub(super) async fn update(
    _ctx: &DaemonContext,
    _initiative: &InitiativeId,
    _contract_append: &str,
) -> Result<Reply, ArchonError> {
    Err(not_implemented("update_initiative"))
}
