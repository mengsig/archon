//! GitHub links, the object cache and issue import (CONTRACTS.md §1.3,
//! owners D1 and D2). The link requests are served from the link store, the
//! cache from what the read-only sync (`crate::github_sync`) last fetched.

use archon_core::error::ArchonError;
use archon_core::github::normalized_ref;
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::{LinkSource, MAX_GITHUB_OBJECTS};
use archon_core::protocol::{
    EnqueueRequest, GithubKind, GithubLink, GithubObjectState, GithubRef, LinkRelation,
    ManualLinkRelation, Reply, feature,
};
use archon_core::queue::{Priority, QueueItem};
use archon_core::repo::GH_HOST;

use crate::context::DaemonContext;
use crate::github_sync::{self, import};

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[feature::GITHUB_LINKS, feature::GITHUB_IMPORT];

/// `item`'s links, with the cached objects they name.
pub(super) async fn item_links(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<Reply, ArchonError> {
    let store = ctx.read_store().await?;
    let links = store.item_links(item)?;
    let objects = store.linked_github_objects(item)?;
    assert!(
        links.iter().all(|l| &l.item == item),
        "only the item's links"
    );
    assert!(
        objects.len() <= links.len(),
        "an object per linked number at most"
    );
    Ok(Reply::ItemLinks { links, objects })
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

/// One repository's cached objects, `limit` (default and cap
/// [`MAX_GITHUB_OBJECTS`]) newest first.
pub(super) async fn list_objects(
    ctx: &DaemonContext,
    repo: &str,
    kind: Option<GithubKind>,
    state: Option<GithubObjectState>,
    limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    let limit = limit.unwrap_or(MAX_GITHUB_OBJECTS);
    if limit == 0 || limit > MAX_GITHUB_OBJECTS {
        return Err(ArchonError::Invalid(format!(
            "list_github_objects takes a limit of 1 to {MAX_GITHUB_OBJECTS}, not {limit}"
        )));
    }
    let limit = limit as usize;
    let (objects, synced_at) = ctx
        .read_store()
        .await?
        .github_objects(repo, kind, state, limit)?;
    assert!(objects.len() <= limit, "at most the limit");
    assert!(
        objects
            .iter()
            .all(|o| kind.is_none_or(|k| o.reference.kind == k)
                && state.is_none_or(|s| o.state == s)),
        "only the asked kind and state"
    );
    Ok(Reply::GithubObjects { objects, synced_at })
}

/// Enqueue the issue `reference` names in the registered repository whose
/// `origin` it is, once: a repeat returns the first item, asking GitHub nothing.
pub(super) async fn import_issue(
    ctx: &DaemonContext,
    reference: &GithubRef,
    pipeline: Option<String>,
    priority: Option<Priority>,
) -> Result<Reply, ArchonError> {
    let reference = normalized_ref(reference)?;
    if reference.kind != GithubKind::Issue || reference.host != GH_HOST {
        return Err(ArchonError::Invalid(format!(
            "import_issue imports a {GH_HOST} issue, not {:?}",
            reference
        )));
    }
    let key = import::key(&reference);
    // Its own statement: without reader connections the read holds the writer,
    // which `imported` locks again.
    let first = ctx.read_store().await?.imported_item(&key)?;
    if let Some(item) = first {
        return imported(ctx, &reference, None, item).await;
    }
    let mut registry = github_sync::registered(ctx).await?;
    let repo = registry.by_origin.remove(&reference.repo).ok_or_else(|| {
        let unreadable = match registry.unreadable.as_slice() {
            [] => String::new(),
            some => format!("; these origins could not be read: {}", some.join("; ")),
        };
        ArchonError::NotFound(format!(
            "a registered repository whose origin is github.com/{}{unreadable}",
            reference.repo
        ))
    })?;
    let issue = import::read_issue(ctx, &repo, &reference).await?;
    let title = import::title(&reference, &issue.cached.object.title);
    let request = EnqueueRequest {
        repo: Some(crate::resolve::repo_ref(&repo.id)),
        pipeline,
        brief: import::brief(&reference, &issue.cached.object.title, &issue.body),
        title,
        priority: priority.unwrap_or_default(),
        depends_on: Vec::new(),
        onto: None,
        tags: Vec::new(),
        model_plan: Default::default(),
        model_reasons: None,
        budget_usd: None,
        stage_timeouts: Default::default(),
        idempotency_key: Some(key.clone()),
        autonomy: Default::default(),
        origin: None,
    };
    let item = match super::items::enqueue(ctx, request).await {
        Ok(Reply::Item(item) | Reply::ItemBudgetsOff(item)) => item,
        Ok(other) => unreachable!("an enqueue replies with its item, not {other:?}"),
        // A concurrent import of the same issue won the key: return its item.
        Err(error) => match ctx.read_store().await?.imported_item(&key)? {
            Some(first) => first,
            None => return Err(error),
        },
    };
    imported(ctx, &reference, Some(&issue.cached), item).await
}

/// `item`, which imported `reference`: linked (again, which changes
/// nothing), with the issue cached as just read.
async fn imported(
    ctx: &DaemonContext,
    reference: &GithubRef,
    read: Option<&archon_queue::store::CachedObject>,
    item: QueueItem,
) -> Result<Reply, ArchonError> {
    let link = GithubLink {
        item: item.id.clone(),
        reference: reference.clone(),
        relation: LinkRelation::Imported,
        source: LinkSource::Import,
        created_at: item.created_at,
    };
    let mut store = ctx.store.lock().await;
    if let Some(cached) = read {
        assert_eq!(&cached.object.reference, reference, "the issue asked for");
        store.cache_github_object(cached)?;
    }
    store.link_github(&link)?;
    Ok(Reply::Item(item))
}
