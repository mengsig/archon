//! The repository watches' share of a sync pass: each due watch's list is
//! walked whole, a conditional `GET` a page, and stored only when completed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::id::RepoId;
use archon_core::protocol::github_links::MIN_WATCH_MINUTES;
use archon_core::repo::Repo;
use archon_queue::Store;
use tokio::time::Instant;

use super::watch::{self, Counts, PageMark, Pull, Row, Walk, WatchRecord, WatchedChange};
use super::watch_read::{self, Counted, Got, MAX_PAGES};
use super::{GithubSync, MAX_REQUESTS_PER_PASS, Pass, Registry};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// Most watch `GET`s in one pass: the linked objects keep the rest.
pub(super) const MAX_WATCH_REQUESTS: usize = MAX_REQUESTS_PER_PASS / 2;

/// The soonest a walk cut short is taken up again, and a watch is asked after a restart.
const FLOOR: Duration = Duration::from_secs(MIN_WATCH_MINUTES as u64 * 60);

/// What the watches did in one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchPass {
    /// `GET`s sent (pages and counts), also counted in [`Pass::requested`].
    pub requested: usize,
    /// Watches GitHub sent a page of (a 200), unchanged (all 304s), or failed.
    pub changed: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Triggers raised: completed walks with news in them.
    pub triggers: usize,
    /// Watches of a registered repository with no readable GitHub `origin`.
    pub no_origin: usize,
    /// Watches dropped because their repository is no longer registered.
    pub dropped: usize,
    /// Walks cut short by the budget or a backoff: nothing stored, resumed first.
    pub incomplete: usize,
    /// Walks stopped because the watch or registration changed meanwhile.
    pub revoked: usize,
}

/// What the sync remembers of a watch between passes; never stored.
#[derive(Debug)]
pub(super) struct Asked {
    at: Instant,
    /// The last walk was cut short: what it had counted, for its resumption.
    resume: Option<Resume>,
}

/// The rows a cut walk counted, of one setup of the watch and one `origin`.
#[derive(Debug)]
struct Resume {
    generation: String,
    repository: String,
    rows: BTreeMap<u64, Row>,
}

/// A due watch and the registration it reads as.
struct Due {
    id: RepoId,
    /// The record as the pass read it, re-pointed at `repository`.
    record: WatchRecord,
    /// The `owner/name` the stored record was of.
    stored: String,
    repository: String,
    repo: Repo,
}

/// The list as one walk read it, before its pull requests are counted.
struct Listed {
    started_at: Timestamp,
    pages: Vec<PageMark>,
    /// The pull requests of the pages GitHub sent, each once.
    fresh: BTreeMap<u64, Pull>,
}

/// How far a walk of the list's pages got.
enum Listing {
    /// Every page was a 304: the stored snapshot stands. When the walk began.
    Unchanged(Timestamp),
    /// The budget or a backoff left a page unasked.
    Cut,
    Changed(Listed),
}

/// How one watch's walk went, for the pass.
#[derive(Default)]
struct Outcome {
    backoff: Option<Duration>,
    /// Set when the walk was cut short: the rows it had counted.
    resume: Option<BTreeMap<u64, Row>>,
}

impl GithubSync {
    /// Walk each due watch once, cut walks first, counted in `pass`.
    pub(super) async fn read_watches(
        &mut self,
        ctx: &DaemonContext,
        registry: &Registry,
        pass: &mut Pass,
    ) -> Result<(), ArchonError> {
        let due = self.due_watches(ctx, registry, pass).await?;
        for due in due {
            if !within_budget(pass) {
                break;
            }
            let resumed = self.take_resume(&due);
            let mut walked = walk_one(ctx, &due, resumed, pass).await;
            let cut = walked
                .as_mut()
                .ok()
                .and_then(|outcome| outcome.resume.take());
            let resume = cut.map(|rows| Resume {
                generation: due.record.generation.clone(),
                repository: due.repository.clone(),
                rows,
            });
            // Asked, even by a walk that failed: it waits its interval like any other.
            let at = Instant::now();
            self.watch_asked
                .insert(due.id.clone(), Asked { at, resume });
            if let Some(wait) = walked?.backoff {
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

    /// The rows `due`'s last walk counted before it was cut, if it is the same watch.
    fn take_resume(&mut self, due: &Due) -> BTreeMap<u64, Row> {
        let resume = self
            .watch_asked
            .get_mut(&due.id)
            .and_then(|asked| asked.resume.take());
        match resume {
            Some(resume)
                if resume.generation == due.record.generation
                    && resume.repository == due.repository =>
            {
                resume.rows
            }
            _ => BTreeMap::new(),
        }
    }

    /// The watches due now, cut walks first; drops those whose repository is gone.
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
            if !self.is_due(&id, &record, now) {
                continue;
            }
            let stored = record.repository.clone();
            due.push(Due {
                id,
                record: record.rebased(repository),
                stored,
                repository: repository.clone(),
                repo: repo.clone(),
            });
        }
        due.sort_by_key(|d| {
            let asked = self.watch_asked.get(&d.id);
            let cut = asked.is_some_and(|a| a.resume.is_some());
            (!cut, asked.map(|a| a.at))
        });
        assert!(due.len() <= live.len(), "each watch due at most once");
        Ok(due)
    }

    /// Whether `record`'s interval passed since it was asked. A cut walk waits only
    /// the floor; so does a watch not asked since the sync's start (a restart).
    fn is_due(&self, id: &RepoId, record: &WatchRecord, now: Instant) -> bool {
        let Some(asked) = self.watch_asked.get(id) else {
            return now.duration_since(self.started) >= FLOOR;
        };
        let minutes = record.interval_minutes.max(MIN_WATCH_MINUTES);
        let every = match asked.resume {
            Some(_) => FLOOR,
            None => Duration::from_secs(u64::from(minutes) * 60),
        };
        assert!(every >= FLOOR, "never sooner than the floor");
        now.duration_since(asked.at) >= every
    }
}

/// Whether the pass may send one more watch `GET`.
fn within_budget(pass: &Pass) -> bool {
    pass.watch.requested < MAX_WATCH_REQUESTS && pass.requested < MAX_REQUESTS_PER_PASS
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

/// Sends a due watch's `GET`s within the pass's budget, until the first
/// backoff. Before each, the watch and its registration are checked again.
struct Asker<'a> {
    ctx: &'a DaemonContext,
    due: &'a Due,
    backoff: Option<Duration>,
    /// The watch was removed or set up again, or the registration changed.
    revoked: bool,
}

impl Asker<'_> {
    fn can_ask(&self, pass: &Pass) -> bool {
        self.backoff.is_none() && !self.revoked && within_budget(pass)
    }

    /// `None` when the budget, a backoff or a revocation allows no more.
    async fn get(
        &mut self,
        pass: &mut Pass,
        endpoint: &str,
        etag: Option<&str>,
    ) -> Option<Result<Got, ArchonError>> {
        if !self.can_ask(pass) {
            return None;
        }
        let current = match self.ctx.read_store().await {
            Ok(store) => still_current(&store, self.due),
            Err(error) => Err(error),
        };
        match current {
            Ok(true) => {}
            Ok(false) => {
                self.revoked = true;
                return None;
            }
            Err(error) => return Some(Err(error)),
        }
        pass.requested += 1;
        pass.watch.requested += 1;
        let binary = &self.ctx.config.github.binary;
        let answer = match watch_read::get(binary, &self.due.repo, endpoint, etag).await {
            Ok(answer) => answer,
            Err(error) => return Some(Err(error)),
        };
        self.backoff = self.backoff.or(answer.backoff);
        Some(answer.got)
    }
}

/// Walk `due`'s list, count what changed, and store the walk if it completed.
async fn walk_one(
    ctx: &DaemonContext,
    due: &Due,
    resumed: BTreeMap<u64, Row>,
    pass: &mut Pass,
) -> Result<Outcome, ArchonError> {
    let mut asker = Asker {
        ctx,
        due,
        backoff: None,
        revoked: false,
    };
    let walked = walk(&mut asker, resumed, pass).await;
    let mut outcome = Outcome {
        backoff: asker.backoff,
        resume: None,
    };
    if asker.revoked {
        tracing::info!(repository = %due.repository, "watch or registration changed during a walk; stopped");
        pass.watch.revoked += 1;
        return Ok(outcome);
    }
    match walked {
        Ok(Walked::Unchanged(started_at)) => {
            pass.watch.unchanged += 1;
            let walked = watch::after_unchanged_walk(&due.record, started_at)?;
            record_walk(ctx, due, walked, Vec::new()).await?;
        }
        Ok(Walked::Cut(rows)) => {
            pass.watch.incomplete += 1;
            outcome.resume = Some(rows);
        }
        Ok(Walked::Whole(walk)) => {
            let (walked, changes) = watch::after_walk(&due.record, walk)?;
            if record_walk(ctx, due, walked, changes).await? {
                pass.watch.triggers += 1;
            }
        }
        Err(error) => {
            tracing::debug!(%error, repository = %due.repository, "repository watch walk failed");
            pass.watch.failed += 1;
        }
    }
    Ok(outcome)
}

/// How a walk ended.
enum Walked {
    /// Completed on 304s alone, begun at this time.
    Unchanged(Timestamp),
    /// Cut short, with the rows counted so far, this walk's and `resumed`.
    Cut(BTreeMap<u64, Row>),
    Whole(Walk),
}

/// The list's pages, then the counts of each pull request that moved.
async fn walk(
    asker: &mut Asker<'_>,
    resumed: BTreeMap<u64, Row>,
    pass: &mut Pass,
) -> Result<Walked, ArchonError> {
    let listed = match list(asker, pass).await? {
        Listing::Unchanged(started_at) => return Ok(Walked::Unchanged(started_at)),
        Listing::Cut => return Ok(Walked::Cut(resumed)),
        Listing::Changed(listed) => listed,
    };
    pass.watch.changed += 1;
    let (mut rows, complete) = count_rows(asker, pass, &listed.fresh, resumed).await?;
    if !complete {
        return Ok(Walked::Cut(rows));
    }
    assert_eq!(rows.len(), listed.fresh.len(), "a row each");
    let (mut fresh, mut stale) = (BTreeMap::new(), BTreeSet::new());
    for (number, pull) in listed.fresh {
        let row = rows.remove(&number).expect("counted above");
        if row.recount {
            stale.insert(number);
        }
        fresh.insert(number, (pull, row));
    }
    let mut pages = listed.pages;
    // A row whose counts are older than its pull request is counted again by the
    // next walk: its page loses its ETag, so that walk is sent the page.
    for page in &mut pages {
        if page.numbers.iter().any(|number| stale.contains(number)) {
            page.etag = None;
        }
    }
    Ok(Walked::Whole(Walk {
        started_at: listed.started_at,
        pages,
        fresh,
    }))
}

/// Every page of the list, each conditional on its stored ETag; a full page
/// asks the next, as a 304 cannot say one was added behind it.
async fn list(asker: &mut Asker<'_>, pass: &mut Pass) -> Result<Listing, ArchonError> {
    let due = asker.due;
    let (mut pages, mut fresh) = (Vec::new(), BTreeMap::<u64, Pull>::new());
    let (mut started_at, mut sent) = (None, false);
    for page in 1..=MAX_PAGES {
        let stored = due.record.pages.get(page as usize - 1);
        let etag = stored.and_then(|p| p.etag.as_deref());
        let endpoint = watch_read::list_endpoint(&due.repository, page);
        let Some(got) = asker.get(pass, &endpoint, etag).await else {
            return Ok(Listing::Cut);
        };
        let (mark, date) = match got? {
            Got::NotModified { date } => (unmodified(stored, &endpoint)?, date),
            Got::Changed {
                etag, body, date, ..
            } => {
                sent = true;
                let read = watch::parse_pulls(&due.repository, &body)?;
                let numbers = read.iter().map(|p| p.number).collect();
                for pull in read {
                    keep_latest(&mut fresh, pull);
                }
                (PageMark { etag, numbers }, date)
            }
        };
        started_at = started_at.or(Some(date.unwrap_or_else(archon_core::now)));
        let full = mark.is_full();
        pages.push(mark);
        if !full {
            break;
        }
    }
    let started_at = started_at.expect("a walk not cut asked a page");
    if !sent {
        assert_eq!(pages, due.record.pages, "304s tell the stored pages again");
        return Ok(Listing::Unchanged(started_at));
    }
    Ok(Listing::Changed(Listed {
        started_at,
        pages,
        fresh,
    }))
}

/// The stored mark a 304 says stands; a 304 to a page asked without one is refused.
fn unmodified(stored: Option<&PageMark>, endpoint: &str) -> Result<PageMark, ArchonError> {
    match stored {
        Some(mark) if mark.etag.is_some() => Ok(mark.clone()),
        _ => Err(ArchonError::Protocol(format!(
            "GitHub answered 304 to the unconditional {endpoint}"
        ))),
    }
}

/// A pull request a shift put on two pages is kept as last updated.
fn keep_latest(fresh: &mut BTreeMap<u64, Pull>, pull: Pull) {
    let stale = fresh
        .get(&pull.number)
        .is_some_and(|seen| seen.updated_at > pull.updated_at);
    if !stale {
        fresh.insert(pull.number, pull);
    }
}

/// A row for each of `fresh` as listed now: with the stored counts if it has not moved, else
/// a cut walk's (to recount if it moved again), else counted now. `false` when cut short.
async fn count_rows(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    fresh: &BTreeMap<u64, Pull>,
    mut resumed: BTreeMap<u64, Row>,
) -> Result<(BTreeMap<u64, Row>, bool), ArchonError> {
    let due = asker.due;
    let stored = &due.record.pulls;
    resumed.retain(|number, _| fresh.contains_key(number));
    let mut rows = BTreeMap::new();
    for (number, pull) in fresh {
        let current = stored.get(number).filter(|row| row.is_of(pull));
        let row = match (current, resumed.get(number)) {
            (Some(row), _) => pull.row(row.counts()),
            (None, Some(row)) => Row {
                recount: !row.is_of(pull),
                ..pull.row(row.counts())
            },
            (None, None) => match counts(asker, pass, *number).await? {
                Some(counts) => pull.row(counts),
                None => {
                    resumed.extend(rows);
                    return Ok((resumed, false));
                }
            },
        };
        rows.insert(*number, row);
    }
    assert_eq!(rows.len(), fresh.len(), "a row each");
    assert!(rows.values().all(|row| row.missed.is_none()), "listed rows");
    Ok((rows, true))
}

/// #`number`'s comment and review counts; `None` when a `GET` was not allowed.
async fn counts(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    number: u64,
) -> Result<Option<Counts>, ArchonError> {
    let Some(comments) = count(asker, pass, number, Counted::Comments).await? else {
        return Ok(None);
    };
    let Some(reviews) = count(asker, pass, number, Counted::Reviews).await? else {
        return Ok(None);
    };
    Ok(Some(Counts { comments, reviews }))
}

async fn count(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    number: u64,
    counted: Counted,
) -> Result<Option<u64>, ArchonError> {
    let endpoint = watch_read::count_endpoint(&asker.due.repository, number, counted);
    let Some(got) = asker.get(pass, &endpoint, None).await else {
        return Ok(None);
    };
    match got? {
        Got::Changed {
            body, last_page, ..
        } => watch_read::count(&body, last_page).map(Some),
        Got::NotModified { .. } => Err(ArchonError::Protocol(format!(
            "GitHub answered 304 to the unconditional {endpoint}"
        ))),
    }
}

/// Save a completed walk as `walked` and hold `changes` under one store lock, unless the
/// watch, registration or `origin` changed meanwhile. Whether a trigger was raised.
async fn record_walk(
    ctx: &DaemonContext,
    due: &Due,
    mut walked: WatchRecord,
    changes: Vec<WatchedChange>,
) -> Result<bool, ArchonError> {
    assert!(walked.walks > due.record.walks, "one more walk");
    if !origin_unmoved(due).await {
        tracing::info!(repository = %due.repository, "origin moved during a watch walk; discarded");
        return Ok(false);
    }
    let mut store = ctx.store.lock().await;
    if !still_current(&store, due)? {
        return Ok(false);
    }
    let Some(current) = watch::get(&store, &due.id)? else {
        unreachable!("a current watch is stored");
    };
    assert_eq!(current.generation, due.record.generation, "the same watch");
    walked.interval_minutes = current.interval_minutes;
    let raised = !changes.is_empty();
    if raised {
        // Held before the snapshot is saved: a crash between repeats the news
        // rather than losing it.
        triggers::hold_watch(&mut store, watch_event(due, changes))?;
    }
    watch::put(&mut store, &due.id, &walked)?;
    drop(store);
    if raised {
        triggers::wake(ctx);
    }
    Ok(raised)
}

/// Whether `due`'s watch is the one the pass read, and its repository is
/// registered still at the same checkout with the same `gh` login.
fn still_current(store: &Store, due: &Due) -> Result<bool, ArchonError> {
    let Some(current) = watch::get(store, &due.id)? else {
        return Ok(false);
    };
    if current.generation != due.record.generation || current.repository != due.stored {
        return Ok(false);
    }
    let registered = store.repos()?.into_iter().find(|r| r.id == due.id);
    Ok(registered.is_some_and(|r| r.path == due.repo.path && r.gh_user == due.repo.gh_user))
}

/// Whether `due`'s checkout still names the `origin` its walk was of. An
/// unreadable one counts as moved: the next pass resolves it again.
async fn origin_unmoved(due: &Due) -> bool {
    let path = due.repo.path.clone();
    let read = tokio::task::spawn_blocking(move || crate::worktree::stack::origin_url(&path)).await;
    match read {
        Ok(Ok(origin)) => origin
            .as_deref()
            .and_then(GithubRepo::from_remote_url)
            .is_some_and(|github| github.as_str() == due.repository),
        Ok(Err(error)) => {
            tracing::debug!(%error, repository = %due.repository, "origin unreadable after a watch walk");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "origin read after a watch walk did not finish");
            false
        }
    }
}

/// The trigger for `changes`: news with no operator standing. The first are
/// told in full, and every one past them is named by kind and number.
fn watch_event(due: &Due, mut changes: Vec<WatchedChange>) -> TriggerEvent {
    assert!(!changes.is_empty(), "only news raises a trigger");
    let past = changes.split_off(changes.len().min(watch::MAX_CHANGES));
    let (changes, rest, truncated) = super::watch_fold::capped(changes, watch::by_kind(&past));
    assert!(!changes.is_empty(), "the first are told in full");
    TriggerEvent::GithubWatch {
        repo: due.repo.name.clone(),
        repository: due.repository.clone(),
        changes,
        rest,
        truncated,
        note: truncated.then(|| super::watch_fold::TRUNCATED_NOTE.to_owned()),
        operator: false,
        trust: watch::WATCH_TRUST.to_owned(),
    }
}
