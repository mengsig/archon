//! GitHub links, the object cache and issue import (CONTRACTS.md §1.3,
//! owners D1 and D2). The link requests are served from the link store; the
//! cache and import answer `unavailable` until D2 serves them.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{
    GithubKind, GithubLink, GithubObjectState, GithubRef, ManualLinkRelation, Reply, feature,
};
use archon_core::queue::Priority;

use crate::context::DaemonContext;

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[feature::GITHUB_LINKS];

fn not_yet(command: &str, feature: &str) -> ArchonError {
    ArchonError::Unavailable(format!(
        "{command} is not implemented yet: this archond does not serve the `{feature}` feature"
    ))
}

/// `item`'s links. Their cached objects come with the `github_import` cache.
pub(super) async fn item_links(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<Reply, ArchonError> {
    let links = ctx.read_store().await?.item_links(item)?;
    assert!(
        links.iter().all(|l| &l.item == item),
        "only the item's links"
    );
    Ok(Reply::ItemLinks {
        links,
        objects: Vec::new(),
    })
}

pub(super) async fn issue_items(
    ctx: &DaemonContext,
    reference: &GithubRef,
) -> Result<Reply, ArchonError> {
    let links = ctx.read_store().await?.issue_items(reference)?;
    assert!(
        links.iter().all(|l| l.reference.number == reference.number),
        "only links to the one number"
    );
    Ok(Reply::IssueItems { links })
}

/// `link_issue` (`linked`) or `unlink_issue`: an operator's link, `ok`
/// whether or not it was already there.
pub(super) async fn set_link(
    ctx: &DaemonContext,
    item: &QueueItemId,
    reference: &GithubRef,
    relation: ManualLinkRelation,
    linked: bool,
) -> Result<Reply, ArchonError> {
    let mut store = ctx.store.lock().await;
    if !linked {
        store.unlink_github(item, reference, relation.into())?;
        return Ok(Reply::Ok);
    }
    let link = GithubLink {
        item: item.clone(),
        reference: reference.clone(),
        relation: relation.into(),
        source: LinkSource::Operator,
        created_at: archon_core::now(),
    };
    store.link_github(&link)?;
    Ok(Reply::Ok)
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
