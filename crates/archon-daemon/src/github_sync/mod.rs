//! The read-only GitHub sync (ARCHITECTURE §f, vNext D2): it refreshes the
//! cached copy of every linked issue and pull request through `gh api` `GET`s
//! conditional on their ETags, and never writes to GitHub. A pull request is
//! watched until it is merged or closed, however long that takes.

mod fetch;
pub mod import;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::protocol::{GithubKind, GithubObjectState, GithubRef};
use archon_core::repo::{GH_HOST, Repo};
use archon_queue::store::SyncTarget;
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

/// How many linked objects one pass considers, stalest first.
const MAX_TARGETS_PER_PASS: usize = 2_000;

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
}

type Key = (String, String, u64);

/// The sync's own schedule: when each object was last asked about, and how
/// long GitHub asked it to wait. Kept in memory, so a 304 writes nothing.
#[derive(Debug)]
pub struct GithubSync {
    interval: Duration,
    asked: HashMap<Key, Instant>,
    paused_until: Option<Instant>,
}

impl GithubSync {
    pub fn new(interval: Duration) -> Self {
        assert!(interval >= MIN_INTERVAL, "at least {MIN_INTERVAL:?}");
        Self {
            interval,
            asked: HashMap::new(),
            paused_until: None,
        }
    }

    /// Whether `target` is due: never asked, or asked longer ago than its interval.
    fn due(&self, target: &SyncTarget, now: Instant) -> bool {
        let key = (target.host.clone(), target.repo.clone(), target.number);
        let every = match target.state {
            Some(GithubObjectState::Closed) => CLOSED_INTERVAL,
            _ => self.interval,
        };
        self.asked
            .get(&key)
            .is_none_or(|at| now.duration_since(*at) >= every)
    }

    /// Ask GitHub about each due linked object once, as the login of the
    /// registered repository it belongs to. Only a store failure is an error.
    pub async fn pass(&mut self, ctx: &DaemonContext) -> Result<Pass, ArchonError> {
        let mut pass = Pass::default();
        let now = Instant::now();
        if self.paused_until.is_some_and(|until| now < until) {
            return Ok(pass);
        }
        let targets = ctx
            .read_store()
            .await?
            .github_sync_targets(MAX_TARGETS_PER_PASS)?;
        let repos = registered(ctx).await?;
        let due: Vec<&SyncTarget> = targets.iter().filter(|t| self.due(t, now)).collect();
        for target in due {
            let Some(repo) = repos.get(&target.repo).filter(|_| target.host == GH_HOST) else {
                pass.unregistered += 1;
                continue;
            };
            if pass.requested == MAX_REQUESTS_PER_PASS {
                break;
            }
            pass.requested += 1;
            self.asked.insert(
                (target.host.clone(), target.repo.clone(), target.number),
                now,
            );
            if let Some(wait) = refresh(ctx, repo, target, &mut pass).await? {
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
            ctx.store.lock().await.cache_github_object(&cached)?;
            pass.changed += 1;
        }
        Ok(fetch::Fetched::NotModified) => pass.unchanged += 1,
        Ok(fetch::Fetched::Missing) => pass.missing += 1,
        Err(error) => {
            tracing::debug!(%error, repo = %target.repo, number = target.number, "GitHub read refused");
            pass.failed += 1;
        }
    }
    Ok(read.backoff)
}

/// The registered repositories by the github.com `owner/name` their `origin`
/// names; a repository without one is left out.
pub(crate) async fn registered(ctx: &DaemonContext) -> Result<HashMap<String, Repo>, ArchonError> {
    let repos = ctx.read_store().await?.repos()?;
    let found = tokio::task::spawn_blocking(move || {
        let mut by_origin = HashMap::new();
        for repo in repos {
            let origin = crate::worktree::stack::origin_url(&repo.path)?;
            if let Some(github) = origin.as_deref().and_then(GithubRepo::from_remote_url) {
                by_origin.entry(github.as_str().to_owned()).or_insert(repo);
            }
        }
        Ok::<_, ArchonError>(by_origin)
    })
    .await
    .map_err(|e| ArchonError::io("reading repository origins", std::io::Error::other(e)))??;
    debug_assert!(
        found.keys().all(|k| k == &k.to_ascii_lowercase()),
        "keys are lowercased"
    );
    Ok(found)
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
