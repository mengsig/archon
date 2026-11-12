//! The read-only GitHub sync (ARCHITECTURE §f, vNext D2): it refreshes the
//! cached copy of every linked issue and pull request through `gh api` `GET`s
//! conditional on their ETags, and never writes to GitHub. A pull request is
//! watched until it is merged or closed, however long that takes. An open
//! one also has its review decision and checks read (vNext D3).

mod fetch;
pub mod import;
mod pr_state;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::protocol::{GithubKind, GithubObjectState, GithubRef};
use archon_core::repo::{GH_HOST, Repo};
use archon_queue::store::{CachedObject, SyncTarget};
use tokio::time::Instant;

use crate::context::DaemonContext;

/// How often one object is asked about, at most (ARCHITECTURE §f).
pub const INTERVAL: Duration = Duration::from_secs(300);

/// The shortest interval a sync may be given.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// A closed issue rarely reopens: it is asked about once a day.
const CLOSED_INTERVAL: Duration = Duration::from_secs(86_400);

/// The most `GET`s one pass makes; the rest wait for the next pass.
const MAX_REQUESTS_PER_PASS: usize = 50;

/// How many linked objects one store read returns; a pass pages through all.
const TARGET_PAGE: usize = 1_000;

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    /// `GET`s sent.
    pub requested: usize,
    /// 200s, each written to the cache.
    pub changed: usize,
    /// 304s, which write nothing.
    pub unchanged: usize,
    /// 404s and 410s.
    pub missing: usize,
    pub failed: usize,
    /// Objects in no registered repository, which no login is chosen for.
    pub unregistered: usize,
    /// Set when GitHub's rate-limit headers stopped the pass.
    pub backoff: Option<Duration>,
    /// Open pull requests whose review and checks were read, and those
    /// whose read failed, which keep what was cached.
    pub pr_states: usize,
    pub pr_states_failed: usize,
}

type Key = (String, String, u64);

fn key_of(target: &SyncTarget) -> Key {
    (target.host.clone(), target.repo.clone(), target.number)
}

/// The sync's own schedule: when each object was last asked about, and how
/// long GitHub asked it to wait. Kept in memory, so a 304 writes nothing.
#[derive(Debug)]
pub struct GithubSync {
    interval: Duration,
    asked: HashMap<Key, Instant>,
    paused_until: Option<Instant>,
    /// The registered repositories whose `origin` the last pass could not read.
    unreadable: Vec<String>,
}

impl GithubSync {
    pub fn new(interval: Duration) -> Self {
        assert!(interval >= MIN_INTERVAL, "at least {MIN_INTERVAL:?}");
        Self {
            interval,
            asked: HashMap::new(),
            paused_until: None,
            unreadable: Vec::new(),
        }
    }

    /// Whether `target` is due: never asked, or asked longer ago than its interval.
    fn due(&self, target: &SyncTarget, now: Instant) -> bool {
        let key = key_of(target);
        let every = match target.state {
            Some(GithubObjectState::Closed) => CLOSED_INTERVAL,
            _ => self.interval,
        };
        self.asked
            .get(&key)
            .is_none_or(|at| now.duration_since(*at) >= every)
    }

    /// The due `targets`, least recently asked first: those this process never
    /// asked, then by when, ties to the stalest copy. Forgets unwatched objects.
    fn due_in_order(&mut self, targets: Vec<SyncTarget>, now: Instant) -> Vec<SyncTarget> {
        let watched: HashSet<Key> = targets.iter().map(key_of).collect();
        self.asked.retain(|key, _| watched.contains(key));
        let mut due: Vec<SyncTarget> = targets.into_iter().filter(|t| self.due(t, now)).collect();
        due.sort_by_cached_key(|t| (self.asked.get(&key_of(t)).copied(), t.fetched_at));
        assert!(
            self.asked.len() <= watched.len(),
            "only watched objects kept"
        );
        assert!(due.len() <= watched.len(), "each target due at most once");
        due
    }

    /// Ask GitHub about each due linked object once, as the login of the
    /// registered repository it belongs to. Only a store failure is an error.
    pub async fn pass(&mut self, ctx: &DaemonContext) -> Result<Pass, ArchonError> {
        let mut pass = Pass::default();
        let now = Instant::now();
        if self.paused_until.is_some_and(|until| now < until) {
            return Ok(pass);
        }
        let targets = watched_targets(ctx).await?;
        let registry = registered(ctx).await?;
        self.note_unreadable(registry.unreadable);
        for target in self.due_in_order(targets, now) {
            let Some(repo) = registry
                .by_origin
                .get(&target.repo)
                .filter(|_| target.host == GH_HOST)
            else {
                pass.unregistered += 1;
                continue;
            };
            if pass.requested == MAX_REQUESTS_PER_PASS {
                break;
            }
            pass.requested += 1;
            // When this request starts, not the pass: a slow pass must not
            // bring its last objects due early.
            self.asked.insert(key_of(&target), Instant::now());
            if let Some(wait) = refresh(ctx, repo, &target, &mut pass).await? {
                self.paused_until = Some(Instant::now() + wait);
                pass.backoff = Some(wait);
                break;
            }
        }
        assert!(pass.requested <= MAX_REQUESTS_PER_PASS, "bounded per pass");
        assert!(
            pass.changed + pass.unchanged + pass.missing + pass.failed == pass.requested,
            "each request has one outcome"
        );
        Ok(pass)
    }

    /// Warn when the set of unreadable repositories changes, not every pass.
    fn note_unreadable(&mut self, unreadable: Vec<String>) {
        if unreadable == self.unreadable {
            return;
        }
        if unreadable.is_empty() {
            tracing::info!("every registered repository's origin reads again");
        } else {
            tracing::warn!(
                ?unreadable,
                "the GitHub sync skips registered repositories whose origin cannot be read"
            );
        }
        self.unreadable = unreadable;
    }
}

/// Every linked object still watched, read a page at a time.
async fn watched_targets(ctx: &DaemonContext) -> Result<Vec<SyncTarget>, ArchonError> {
    let store = ctx.read_store().await?;
    let mut targets: Vec<SyncTarget> = Vec::new();
    loop {
        let after = targets
            .last()
            .map(|t| (t.host.as_str(), t.repo.as_str(), t.number));
        let page = store.github_sync_targets(after, TARGET_PAGE)?;
        let last_page = page.len() < TARGET_PAGE;
        targets.extend(page);
        if last_page {
            break;
        }
    }
    debug_assert_eq!(
        targets.iter().map(key_of).collect::<HashSet<_>>().len(),
        targets.len(),
        "no object read twice"
    );
    Ok(targets)
}

/// One `GET` of `target`, its outcome counted in `pass`: only a 200 writes.
/// Returns the backoff GitHub's headers asked for.
async fn refresh(
    ctx: &DaemonContext,
    repo: &Repo,
    target: &SyncTarget,
    pass: &mut Pass,
) -> Result<Option<Duration>, ArchonError> {
    let reference = GithubRef {
        host: target.host.clone(),
        repo: target.repo.clone(),
        number: target.number,
        kind: GithubKind::Issue,
    };
    let binary = &ctx.config.github.binary;
    let read = match fetch::read(binary, repo, &reference, target.etag.as_deref()).await {
        Ok(read) => read,
        Err(error) => {
            tracing::debug!(%error, repo = %target.repo, number = target.number, "GitHub read failed");
            pass.failed += 1;
            return Ok(None);
        }
    };
    match read.fetched {
        Ok(fetch::Fetched::Changed { cached, .. }) => {
            let cached = with_pr_state(ctx, repo, *cached, pass).await?;
            ctx.store.lock().await.cache_github_object(&cached)?;
            pass.changed += 1;
        }
        Ok(fetch::Fetched::NotModified) => {
            pass.unchanged += 1;
            let open_pr = target.kind == Some(GithubKind::PullRequest)
                && target.state == Some(GithubObjectState::Open);
            let reference = GithubRef {
                kind: GithubKind::PullRequest,
                ..reference
            };
            // Checks change without the issue's ETag: they are read anyway.
            if open_pr && let Some(state) = read_pr_state(ctx, repo, &reference, pass).await {
                let checks = Some(&state.checks);
                let mut store = ctx.store.lock().await;
                store.set_pull_request_state(&reference, state.review_decision, checks)?;
            }
        }
        Ok(fetch::Fetched::Missing) => pass.missing += 1,
        Err(error) => {
            tracing::debug!(%error, repo = %target.repo, number = target.number, "GitHub read refused");
            pass.failed += 1;
        }
    }
    Ok(read.backoff)
}

/// `cached` with a pull request's review decision and checks: read afresh
/// while it is open, else (or when that read fails) kept from the cache.
async fn with_pr_state(
    ctx: &DaemonContext,
    repo: &Repo,
    mut cached: CachedObject,
    pass: &mut Pass,
) -> Result<CachedObject, ArchonError> {
    let reference = cached.object.reference.clone();
    if reference.kind != GithubKind::PullRequest {
        return Ok(cached);
    }
    let fresh = match cached.object.state {
        GithubObjectState::Open => read_pr_state(ctx, repo, &reference, pass).await,
        GithubObjectState::Merged | GithubObjectState::Closed => None,
    };
    let (review_decision, checks) = match fresh {
        Some(state) => (state.review_decision, Some(state.checks)),
        None => match ctx.read_store().await?.github_object(&reference)? {
            Some(previous) => (previous.object.review_decision, previous.object.checks),
            None => (None, None),
        },
    };
    cached.object.review_decision = review_decision;
    cached.object.checks = checks;
    assert_eq!(cached.object.reference, reference, "the same pull request");
    Ok(cached)
}

/// One read of an open pull request's review and checks, counted in `pass`.
async fn read_pr_state(
    ctx: &DaemonContext,
    repo: &Repo,
    reference: &GithubRef,
    pass: &mut Pass,
) -> Option<pr_state::PrState> {
    assert_eq!(
        reference.kind,
        GithubKind::PullRequest,
        "only pull requests have checks"
    );
    match pr_state::read(&ctx.config.github.binary, repo, reference).await {
        Ok(state) => {
            pass.pr_states += 1;
            Some(state)
        }
        Err(error) => {
            tracing::debug!(%error, repo = %reference.repo, number = reference.number, "pull request state read failed");
            pass.pr_states_failed += 1;
            None
        }
    }
}

/// The registered repositories by the github.com `owner/name` their `origin`
/// names, and those whose `origin` could not be read, each with its error.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    pub by_origin: HashMap<String, Repo>,
    pub unreadable: Vec<String>,
}

/// The [`Registry`] now. One unreadable checkout is reported and skipped,
/// so it stops neither the sync nor an import of another repository.
pub(crate) async fn registered(ctx: &DaemonContext) -> Result<Registry, ArchonError> {
    let repos = ctx.read_store().await?.repos()?;
    let count = repos.len();
    let registry = tokio::task::spawn_blocking(move || {
        let mut registry = Registry::default();
        for repo in repos {
            match crate::worktree::stack::origin_url(&repo.path) {
                Ok(origin) => {
                    if let Some(github) = origin.as_deref().and_then(GithubRepo::from_remote_url) {
                        registry
                            .by_origin
                            .entry(github.as_str().to_owned())
                            .or_insert(repo);
                    }
                }
                Err(error) => registry.unreadable.push(format!(
                    "{} ({}): {error}",
                    repo.name,
                    repo.path.display()
                )),
            }
        }
        registry
    })
    .await
    .map_err(|e| ArchonError::io("reading repository origins", std::io::Error::other(e)))?;
    assert!(
        registry.by_origin.len() + registry.unreadable.len() <= count,
        "each repository counted once"
    );
    debug_assert!(
        registry
            .by_origin
            .keys()
            .all(|k| k == &k.to_ascii_lowercase()),
        "keys are lowercased"
    );
    Ok(registry)
}

/// The sync worker: a pass every [`MIN_INTERVAL`], each asking only about
/// the objects due. Only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut sync = GithubSync::new(INTERVAL);
    loop {
        tokio::time::sleep(MIN_INTERVAL).await;
        match sync.pass(&ctx).await {
            Ok(pass) if pass.backoff.is_some() => {
                tracing::info!(?pass, "GitHub asked the sync to wait");
            }
            Ok(pass) if pass.failed > 0 => {
                tracing::warn!(
                    failed = pass.failed,
                    ?pass,
                    "some GitHub objects were not refreshed"
                );
            }
            Ok(pass) => tracing::debug!(?pass, "GitHub sync pass"),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(number: u64) -> SyncTarget {
        SyncTarget {
            host: GH_HOST.into(),
            repo: "o/r".into(),
            number,
            kind: Some(GithubKind::Issue),
            state: Some(GithubObjectState::Open),
            etag: Some("W/\"e\"".into()),
            fetched_at: Some(archon_core::now()),
        }
    }

    /// 600 unchanged objects at 50 a pass: each is asked once a round, in turn,
    /// rather than the first 250 every interval and the rest never.
    #[test]
    fn every_watched_object_is_asked_in_turn_however_many_there_are() {
        let mut sync = GithubSync::new(INTERVAL);
        let targets: Vec<SyncTarget> = (1..=600).map(target).collect();
        let start = Instant::now();
        let mut asks: HashMap<u64, u32> = HashMap::new();
        for minute in 0..60 {
            let now = start + MIN_INTERVAL * minute;
            let due = sync.due_in_order(targets.clone(), now);
            for t in due.into_iter().take(MAX_REQUESTS_PER_PASS) {
                sync.asked.insert(key_of(&t), now);
                *asks.entry(t.number).or_default() += 1;
            }
        }
        assert_eq!(asks.len(), targets.len(), "every object is asked");
        assert!(asks.values().all(|&n| n == 5), "{asks:?}");
        sync.due_in_order(targets[..10].to_vec(), start);
        assert_eq!(sync.asked.len(), 10, "unwatched objects are forgotten");
    }

    #[test]
    fn never_fetched_objects_come_before_the_stalest_copies() {
        let mut sync = GithubSync::new(INTERVAL);
        let mut fresh = target(1);
        fresh.fetched_at = Some(archon_core::now());
        let mut stale = target(2);
        stale.fetched_at = Some(archon_core::now() - chrono::Duration::days(1));
        let mut new = target(3);
        new.fetched_at = None;
        let order = sync.due_in_order(vec![fresh, stale, new], Instant::now());
        let numbers: Vec<u64> = order.iter().map(|t| t.number).collect();
        assert_eq!(numbers, [3, 2, 1]);
    }
}
