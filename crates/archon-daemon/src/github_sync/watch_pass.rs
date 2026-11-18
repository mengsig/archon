//! The repository watches' share of a sync pass: each due watch costs one
//! conditional `GET`, bounded by [`MAX_WATCH_REQUESTS`] and the pass's
//! [`MAX_REQUESTS_PER_PASS`]; only a changed list writes, and only news wakes.

use std::collections::HashSet;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::protocol::github_links::MIN_WATCH_MINUTES;
use archon_core::repo::Repo;
use tokio::time::Instant;

use super::watch::{self, Listed, Pull, WatchRecord};
use super::{GithubSync, MAX_REQUESTS_PER_PASS, Pass, Registry};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// Most watch reads in one pass: the linked objects keep the rest.
pub(super) const MAX_WATCH_REQUESTS: usize = MAX_REQUESTS_PER_PASS / 2;

/// What the watches did in one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchPass {
    /// `GET`s sent, also counted in [`Pass::requested`].
    pub requested: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Triggers raised: changed lists with news in them.
    pub triggers: usize,
    /// Watches of a registered repository with no readable GitHub `origin`.
    pub no_origin: usize,
    /// Watches dropped because their repository is no longer registered.
    pub dropped: usize,
}

/// A due watch and the registration it reads as.
struct Due {
    id: RepoId,
    record: WatchRecord,
    repository: String,
    repo: Repo,
}

impl GithubSync {
    /// Read each due watch once, least recently asked first, counted in `pass`.
    pub(super) async fn read_watches(
        &mut self,
        ctx: &DaemonContext,
        registry: &Registry,
        pass: &mut Pass,
    ) -> Result<(), ArchonError> {
        let due = self.due_watches(ctx, registry, pass).await?;
        for due in due {
            if pass.watch.requested == MAX_WATCH_REQUESTS || pass.requested == MAX_REQUESTS_PER_PASS
            {
                break;
            }
            pass.requested += 1;
            pass.watch.requested += 1;
            self.watch_asked.insert(due.id.clone(), Instant::now());
            if let Some(wait) = read_one(ctx, &due, pass).await? {
                self.paused_until = Some(Instant::now() + wait);
                pass.backoff = Some(wait);
                break;
            }
        }
        assert!(
            pass.watch.requested <= MAX_WATCH_REQUESTS,
            "bounded per pass"
        );
        assert!(
            pass.requested <= MAX_REQUESTS_PER_PASS,
            "within the pass's budget"
        );
        Ok(())
    }

    /// The watches due now; drops those whose repository is gone.
    async fn due_watches(
        &mut self,
        ctx: &DaemonContext,
        registry: &Registry,
        pass: &mut Pass,
    ) -> Result<Vec<Due>, ArchonError> {
        let now = Instant::now();
        let watches = watch::all(&*ctx.read_store().await?)?;
        let live: HashSet<RepoId> = watches.iter().map(|(id, _)| id.clone()).collect();
        self.watch_asked.retain(|id, _| live.contains(id));
        let mut due = Vec::new();
        for (id, record) in watches {
            if !registry.ids.contains(&id) {
                if drop_unregistered(ctx, &id).await? {
                    pass.watch.dropped += 1;
                }
                continue;
            }
            let Some((repository, repo)) = registry.by_id.get(&id) else {
                pass.watch.no_origin += 1;
                continue;
            };
            let minutes = record.interval_minutes.max(MIN_WATCH_MINUTES);
            let every = Duration::from_secs(u64::from(minutes) * 60);
            let asked = self.watch_asked.get(&id).copied();
            if asked.is_some_and(|at| now.duration_since(at) < every) {
                continue;
            }
            let (repository, repo) = (repository.clone(), repo.clone());
            due.push(Due {
                id,
                record,
                repository,
                repo,
            });
        }
        due.sort_by_key(|d| self.watch_asked.get(&d.id).copied());
        assert!(due.len() <= live.len(), "each watch due at most once");
        Ok(due)
    }
}

/// Forget `id`'s watch if its repository is still unregistered under the
/// store lock: one registered since the pass's snapshot keeps its watch.
async fn drop_unregistered(ctx: &DaemonContext, id: &RepoId) -> Result<bool, ArchonError> {
    let mut store = ctx.store.lock().await;
    if store.repos()?.iter().any(|repo| &repo.id == id) {
        return Ok(false);
    }
    watch::forget(&mut store, id)?;
    tracing::info!(repo = %id, "dropped the watch of a repository no longer registered");
    Ok(true)
}

/// One conditional `GET` of `due`'s list. Returns the backoff GitHub asked for.
async fn read_one(
    ctx: &DaemonContext,
    due: &Due,
    pass: &mut Pass,
) -> Result<Option<Duration>, ArchonError> {
    let binary = &ctx.config.github.binary;
    let etag = due.record.etag.as_deref();
    let read = match watch::read(binary, &due.repo, &due.repository, etag).await {
        Ok(read) => read,
        Err(error) => {
            tracing::debug!(%error, repository = %due.repository, "repository watch read failed");
            pass.watch.failed += 1;
            return Ok(None);
        }
    };
    match read.listed {
        Ok(Listed::NotModified) => pass.watch.unchanged += 1,
        Ok(Listed::Changed { etag, pulls }) => {
            pass.watch.changed += 1;
            if record_list(ctx, due, etag, &pulls).await? {
                pass.watch.triggers += 1;
            }
        }
        Err(error) => {
            tracing::debug!(%error, repository = %due.repository, "repository watch read refused");
            pass.watch.failed += 1;
        }
    }
    Ok(read.backoff)
}

/// Save a changed list and hold the news it carries, under one store lock.
/// The first list is the baseline and raises nothing; a watch removed during
/// the read is not written back. Whether a trigger was raised.
async fn record_list(
    ctx: &DaemonContext,
    due: &Due,
    etag: Option<String>,
    pulls: &[Pull],
) -> Result<bool, ArchonError> {
    let mut store = ctx.store.lock().await;
    let Some(mut record) = watch::get(&store, &due.id)? else {
        return Ok(false);
    };
    let changes = match &record.pulls {
        Some(before) => watch::diff(before, pulls),
        None => Vec::new(),
    };
    let raised = !changes.is_empty();
    if raised {
        // Held before the list is saved: a crash between repeats the news
        // rather than losing it.
        triggers::hold(&mut store, watch_event(due, changes))?;
    }
    record.etag = etag;
    record.fetched_at = Some(archon_core::now());
    record.pulls = Some(watch::marks(pulls));
    watch::put(&mut store, &due.id, &record)?;
    drop(store);
    if raised {
        triggers::wake(ctx);
    }
    Ok(raised)
}

/// The trigger for `changes`: news with no operator standing.
fn watch_event(due: &Due, mut changes: Vec<watch::WatchedChange>) -> TriggerEvent {
    assert!(!changes.is_empty(), "only news raises a trigger");
    let more = changes.len().saturating_sub(watch::MAX_CHANGES);
    changes.truncate(watch::MAX_CHANGES);
    TriggerEvent::GithubWatch {
        repo: due.repo.name.clone(),
        repository: due.repository.clone(),
        changes,
        more: u32::try_from(more).unwrap_or(u32::MAX),
        operator: false,
        trust: watch::WATCH_TRUST.to_owned(),
    }
}
