//! `watch_repo` and `repo_watches` (A18): a watch reads only a registered
//! repository with a github.com `origin`, as that registration's login, so it
//! inherits the registration's trust. Setting one never registers anything.

use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::protocol::github_links::{
    MAX_REPO_WATCHES, MAX_WATCH_MINUTES, MIN_WATCH_MINUTES, RepoWatchView,
};
use archon_core::protocol::{Reply, RepoRef};
use archon_core::repo::Repo;

use crate::context::DaemonContext;
use crate::github_sync::{self, watch};

/// Watch `reference`, change its interval, or (`remove`) stop watching it.
pub(super) async fn set(
    ctx: &DaemonContext,
    reference: &RepoRef,
    interval_minutes: Option<u32>,
    remove: bool,
) -> Result<Reply, ArchonError> {
    if remove && interval_minutes.is_some() {
        return Err(ArchonError::Invalid(
            "interval_minutes goes with a watch, not with remove".into(),
        ));
    }
    let interval = checked_interval(interval_minutes)?;
    let repo = registered(&*ctx.read_store().await?, reference)?;
    if remove {
        let mut store = ctx.store.lock().await;
        watch::forget(&mut store, &repo.id)?;
        drop(store);
        tracing::info!(repo = %repo.name, "repository watch removed");
        return list(ctx).await;
    }
    let repository = github_origin(&repo).await?;
    let mut store = ctx.store.lock().await;
    // Again under the lock: a repository removed meanwhile leaves no watch.
    if registered(&store, reference)?.id != repo.id {
        return Err(not_registered(reference));
    }
    let record = match watch::get(&store, &repo.id)? {
        Some(record) => watch::WatchRecord {
            interval_minutes: interval.unwrap_or(record.interval_minutes),
            ..record
        },
        None if watch::all(&store)?.len() >= MAX_REPO_WATCHES => {
            return Err(ArchonError::Invalid(format!(
                "at most {MAX_REPO_WATCHES} repositories are watched at once; remove one first"
            )));
        }
        None => watch::WatchRecord::new(
            interval.unwrap_or(watch::DEFAULT_MINUTES),
            repository.to_string(),
        ),
    };
    watch::put(&mut store, &repo.id, &record)?;
    drop(store);
    tracing::info!(repo = %repo.name, minutes = record.interval_minutes, "repository watched");
    list(ctx).await
}

/// Every watch, with the `owner/name` its repository's `origin` names now.
pub(super) async fn list(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    let registry = github_sync::registered(ctx).await?;
    let store = ctx.read_store().await?;
    let repos = store.repos()?;
    let mut views = Vec::new();
    for (id, record) in watch::all(&store)? {
        let Some(repo) = repos.iter().find(|r| r.id == id) else {
            continue;
        };
        views.push(RepoWatchView {
            repo: repo.name.clone(),
            repository: registry.by_id.get(&id).map(|(name, _)| name.clone()),
            interval_minutes: record.interval_minutes,
            since: record.since,
            fetched_at: record.walked_at,
            tracked_pull_requests: record
                .walked_at
                .map(|_| u32::try_from(record.pulls.len()).unwrap_or(u32::MAX)),
            truncated: record.is_truncated(),
        });
    }
    assert!(
        views.len() <= repos.len(),
        "a watch per registered repository"
    );
    Ok(Reply::RepoWatches(views))
}

fn checked_interval(minutes: Option<u32>) -> Result<Option<u32>, ArchonError> {
    match minutes {
        Some(m) if !(MIN_WATCH_MINUTES..=MAX_WATCH_MINUTES).contains(&m) => {
            Err(ArchonError::Invalid(format!(
                "interval_minutes must be {MIN_WATCH_MINUTES} to {MAX_WATCH_MINUTES}, not {m}"
            )))
        }
        minutes => Ok(minutes),
    }
}

/// The registered repository `reference` names; an unknown one is refused:
/// a watch never registers one.
fn registered(store: &archon_queue::Store, reference: &RepoRef) -> Result<Repo, ArchonError> {
    match store.repo(reference) {
        Ok(repo) => Ok(repo),
        Err(ArchonError::NotFound(_)) => Err(not_registered(reference)),
        Err(error) => Err(error),
    }
}

fn not_registered(reference: &RepoRef) -> ArchonError {
    ArchonError::NotFound(format!(
        "{} is not a registered repository: a watch reads only a registered one \
         (`archon repo add`), and watch_repo never registers one",
        reference.0
    ))
}

/// `repo`'s github.com `owner/name`; a repository without one is refused.
async fn github_origin(repo: &Repo) -> Result<GithubRepo, ArchonError> {
    let path = repo.path.clone();
    let origin = tokio::task::spawn_blocking(move || crate::worktree::stack::origin_url(&path))
        .await
        .map_err(|e| ArchonError::io("reading a repository origin", std::io::Error::other(e)))??;
    origin
        .as_deref()
        .and_then(GithubRepo::from_remote_url)
        .ok_or_else(|| {
            ArchonError::Invalid(format!(
                "repository {} has no github.com origin, so there is nothing to watch",
                repo.name
            ))
        })
}
