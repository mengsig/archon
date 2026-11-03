//! GitHub links, the object cache and issue import (CONTRACTS.md §1.3,
//! owners D1 and D2). Until served, each answers `unavailable`.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{GithubKind, GithubObjectState, GithubRef, ManualLinkRelation, Reply};
use archon_core::queue::Priority;

use crate::context::DaemonContext;

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[];

fn not_yet(command: &str, feature: &str) -> ArchonError {
    ArchonError::Unavailable(format!(
        "{command} is not implemented yet: this archond does not serve the `{feature}` feature"
    ))
}

pub(super) async fn item_links(
    _ctx: &DaemonContext,
    _item: &QueueItemId,
) -> Result<Reply, ArchonError> {
    Err(not_yet("item_links", "github_links"))
}

pub(super) async fn issue_items(
    _ctx: &DaemonContext,
    _reference: &GithubRef,
) -> Result<Reply, ArchonError> {
    Err(not_yet("issue_items", "github_links"))
}

/// `link_issue` (`linked`) or `unlink_issue`.
pub(super) async fn set_link(
    _ctx: &DaemonContext,
    _item: &QueueItemId,
    _reference: &GithubRef,
    _relation: ManualLinkRelation,
    linked: bool,
) -> Result<Reply, ArchonError> {
    let command = if linked { "link_issue" } else { "unlink_issue" };
    Err(not_yet(command, "github_links"))
}

pub(super) async fn list_objects(
    _ctx: &DaemonContext,
    _repo: &str,
    _kind: Option<GithubKind>,
    _state: Option<GithubObjectState>,
    _limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    Err(not_yet("list_github_objects", "github_import"))
}

pub(super) async fn import_issue(
    _ctx: &DaemonContext,
    _reference: &GithubRef,
    _pipeline: Option<String>,
    _priority: Option<Priority>,
) -> Result<Reply, ArchonError> {
    Err(not_yet("import_issue", "github_import"))
}
