//! The repository watches' share of a sync pass. An unchanged list costs one
//! conditional `GET`; a changed one reads further pages only while they hold
//! updates at or after the watermark, and a comment or review check only for
//! a pull request that moved with nothing else to show, or whose last mark
//! was too recent to trust. Every `GET` counts against [`MAX_WATCH_REQUESTS`]
//! and the pass's [`MAX_REQUESTS_PER_PASS`].

use std::collections::HashSet;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::id::RepoId;
use archon_core::protocol::github_links::MIN_WATCH_MINUTES;
use archon_core::repo::Repo;
use tokio::time::Instant;

use super::watch::{self, Followed, Pull, PullMark, Seen, WatchRecord, WatchedChange};
use super::watch_read::{self, Got, MAX_PAGES, PAGE};
use super::{GithubSync, MAX_REQUESTS_PER_PASS, Pass, Registry};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// Most watch `GET`s in one pass: the linked objects keep the rest.
pub(super) const MAX_WATCH_REQUESTS: usize = MAX_REQUESTS_PER_PASS / 2;

/// What the watches did in one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchPass {
    /// `GET`s sent (pages and follow-ups), also counted in [`Pass::requested`].
    pub requested: usize,
    /// Watches whose list GitHub sent (a 200), unchanged (a 304), or failed.
    pub changed: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Triggers raised: changed lists with news in them.
    pub triggers: usize,
    /// Watches of a registered repository with no readable GitHub `origin`.
    pub no_origin: usize,
    /// Watches dropped because their repository is no longer registered.
    pub dropped: usize,
    /// Reads cut short by the budget or a failed follow-up: asked again.
    pub incomplete: usize,
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

/// A changed list: what was read, and whether all of what was needed.
struct Walked {
    etag: Option<String>,
    pulls: Vec<Pull>,
    /// Every page was read, down to a short one: absent pull requests are closed.
    whole: bool,
    /// No page was left unread for want of budget.
    complete: bool,
    /// GitHub's clock at the first page, which marks are settled against.
    read_at: Timestamp,
}

/// What the read of a changed list adds to the record.
#[derive(Default)]
struct Told {
    changes: Vec<WatchedChange>,
    followed: Followed,
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
            if !within_budget(pass) {
                break;
            }
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
            let stored = record.repository.clone();
            due.push(Due {
                id,
                record: record.rebased(repository),
                stored,
                repository: repository.clone(),
                repo: repo.clone(),
            });
        }
        due.sort_by_key(|d| self.watch_asked.get(&d.id).copied());
        assert!(due.len() <= live.len(), "each watch due at most once");
        Ok(due)
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

/// Sends a due watch's `GET`s within the pass's budget, and stops at the
/// first backoff GitHub asks for.
struct Asker<'a> {
    ctx: &'a DaemonContext,
    due: &'a Due,
    backoff: Option<Duration>,
}

impl Asker<'_> {
    fn can_ask(&self, pass: &Pass) -> bool {
        self.backoff.is_none() && within_budget(pass)
    }

    /// `None` when the budget or a backoff allows no more.
    async fn get(
        &mut self,
        pass: &mut Pass,
        endpoint: &str,
        etag: Option<&str>,
    ) -> Option<Result<Got, ArchonError>> {
        if !self.can_ask(pass) {
            return None;
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

/// Read `due`'s list and what it needs. Returns the backoff GitHub asked for.
async fn read_one(
    ctx: &DaemonContext,
    due: &Due,
    pass: &mut Pass,
) -> Result<Option<Duration>, ArchonError> {
    let mut asker = Asker {
        ctx,
        due,
        backoff: None,
    };
    let walked = match walk(&mut asker, pass).await {
        Ok(Some(walked)) => walked,
        Ok(None) => {
            pass.watch.unchanged += 1;
            return Ok(asker.backoff);
        }
        Err(error) => {
            tracing::debug!(%error, repository = %due.repository, "repository watch read failed");
            pass.watch.failed += 1;
            return Ok(asker.backoff);
        }
    };
    pass.watch.changed += 1;
    let told = match &due.record.pulls {
        Some(_) => follow_up(&mut asker, pass, &walked.pulls).await,
        None => Told::default(),
    };
    if !walked.complete || !told.followed.kept.is_empty() {
        pass.watch.incomplete += 1;
    }
    if record_list(ctx, due, walked, told).await? {
        pass.watch.triggers += 1;
    }
    Ok(asker.backoff)
}

/// The list's pages, newest update first: `None` for a 304 of the first. A
/// baseline reads every page; a later read stops at the watermark.
async fn walk(asker: &mut Asker<'_>, pass: &mut Pass) -> Result<Option<Walked>, ArchonError> {
    let due = asker.due;
    let first = watch_read::list_endpoint(&due.repository, 1);
    let etag = due.record.etag.as_deref();
    let (etag, body, date) = match asker.get(pass, &first, etag).await {
        None => unreachable!("a watch is read only within the budget"),
        Some(Ok(Got::NotModified)) => return Ok(None),
        Some(Ok(Got::Changed {
            etag, body, date, ..
        })) => (etag, body, date),
        Some(Err(error)) => return Err(error),
    };
    let read_at = date.unwrap_or_else(archon_core::now);
    let mut pulls = watch::parse_pulls(&due.repository, &body)?;
    let mut last_page = (pulls.len(), pulls.last().map(|p| p.updated_at));
    let (mut page, mut whole, mut complete) = (1, true, true);
    while last_page.0 >= PAGE {
        let newer = last_page.1.is_some_and(|oldest| {
            // Ties too: GitHub dates to the second, and more may share it.
            due.record.pulls.is_none() || due.record.watermark.is_none_or(|w| oldest >= w)
        });
        if !newer || page == MAX_PAGES {
            whole = false;
            break;
        }
        page += 1;
        let endpoint = watch_read::list_endpoint(&due.repository, page);
        let read = match asker.get(pass, &endpoint, None).await {
            Some(Ok(Got::Changed { body, .. })) => watch::parse_pulls(&due.repository, &body)?,
            Some(Ok(Got::NotModified)) => Vec::new(),
            Some(Err(error)) => return Err(error),
            None => {
                (whole, complete) = (false, false);
                break;
            }
        };
        last_page = (read.len(), read.last().map(|p| p.updated_at));
        let seen: HashSet<u64> = pulls.iter().map(|p| p.number).collect();
        pulls.extend(read.into_iter().filter(|p| !seen.contains(&p.number)));
    }
    assert!(pulls.len() <= PAGE * MAX_PAGES as usize, "bounded pages");
    Ok(Some(Walked {
        etag,
        pulls,
        whole,
        complete,
        read_at,
    }))
}

/// What `pulls` tells against `due`'s marks, with a comment and review check
/// for each pull request that moved with nothing else to show.
async fn follow_up(asker: &mut Asker<'_>, pass: &mut Pass, pulls: &[Pull]) -> Told {
    let diffed = watch::diff(&asker.due.record, pulls);
    let mut told = Told {
        changes: diffed.changes,
        followed: Followed::default(),
    };
    for follow in diffed.follow_ups {
        let pull = &pulls[follow.index];
        let mark = &follow.mark;
        let Some(comments) = newest(asker, pass, Feed::Comments, pull.number, mark).await else {
            told.followed.kept.insert(pull.number);
            continue;
        };
        let mut seen = Seen {
            comment_id: comments.max_id.max(mark.comment_id),
            review_id: mark.review_id,
        };
        if comments.news {
            told.changes.push(pull.told("new_comment", Vec::new()));
        } else {
            let Some(reviews) = newest(asker, pass, Feed::Reviews, pull.number, mark).await else {
                told.followed.kept.insert(pull.number);
                continue;
            };
            seen.review_id = reviews.max_id.max(mark.review_id);
            if reviews.news {
                told.changes.push(pull.told("new_review", Vec::new()));
            }
        }
        told.followed.seen.insert(pull.number, seen);
    }
    assert!(
        told.changes.len() <= pulls.len(),
        "one change per pull request"
    );
    told
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    Comments,
    Reviews,
}

/// What a feed holds beyond a mark.
struct Newest {
    /// A comment or review made at or after the mark, with an id above it.
    news: bool,
    max_id: u64,
}

/// Read `feed`'s first page of #`number`, and its last when there are more:
/// ids only grow, so the newest are there. `None` when either went unread.
async fn newest(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    feed: Feed,
    number: u64,
    mark: &PullMark,
) -> Option<Newest> {
    let repository = &asker.due.repository;
    // `since` filters by update; a second's overlap keeps the boundary in.
    let since = mark.updated_at - chrono::TimeDelta::seconds(1);
    let endpoint = |page| match feed {
        Feed::Comments => watch_read::comments_endpoint(repository, number, since, page),
        Feed::Reviews => watch_read::reviews_endpoint(repository, number, page),
    };
    let (mut items, last) = read_feed(asker, pass, &endpoint(1), feed).await?;
    if let Some(last) = last {
        items.extend(read_feed(asker, pass, &endpoint(last), feed).await?.0);
    }
    let known = match feed {
        Feed::Comments => mark.comment_id,
        Feed::Reviews => mark.review_id,
    };
    let news = items
        .iter()
        .any(|a| a.id > known && a.at.is_some_and(|at| at >= mark.updated_at));
    let max_id = items.iter().map(|a| a.id).max().unwrap_or(0);
    assert!(!news || max_id > known, "news has a new id");
    Some(Newest { news, max_id })
}

/// One page of `feed` and the last page its `Link` names; `None` when it
/// went unread, for want of budget or by a failure.
async fn read_feed(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    endpoint: &str,
    feed: Feed,
) -> Option<(Vec<watch_read::Activity>, Option<u32>)> {
    let what = match feed {
        Feed::Comments => "comments",
        Feed::Reviews => "reviews",
    };
    let failed = match asker.get(pass, endpoint, None).await? {
        Ok(Got::Changed {
            body, last_page, ..
        }) => match watch_read::activity(&body, what) {
            Ok(items) => return Some((items, last_page)),
            Err(error) => error,
        },
        Ok(Got::NotModified) => return Some((Vec::new(), None)),
        Err(error) => error,
    };
    tracing::debug!(error = %failed, endpoint, "repository watch follow-up failed");
    None
}

/// Save a changed list and hold the news it carries, under one store lock.
/// A watch removed, set up again or re-pointed during the read, or whose
/// checkout's `origin` moved meanwhile, is not written back, and an
/// incomplete baseline is not saved. Whether a trigger was raised.
async fn record_list(
    ctx: &DaemonContext,
    due: &Due,
    walked: Walked,
    told: Told,
) -> Result<bool, ArchonError> {
    if !origin_unmoved(due).await {
        tracing::info!(repository = %due.repository, "origin moved during a watch read; discarded");
        return Ok(false);
    }
    let mut store = ctx.store.lock().await;
    let Some(current) = watch::get(&store, &due.id)? else {
        return Ok(false);
    };
    if current.generation != due.record.generation || current.repository != due.stored {
        return Ok(false);
    }
    let registered = store.repos()?.into_iter().find(|r| r.id == due.id);
    if registered.is_none_or(|r| r.path != due.repo.path) {
        return Ok(false);
    }
    let baseline = due.record.pulls.is_none();
    if baseline && !walked.complete {
        return Ok(false);
    }
    let complete = walked.complete && told.followed.kept.is_empty();
    let raised = !told.changes.is_empty();
    if raised {
        // Held before the marks are saved: a crash between repeats the news
        // rather than losing it.
        triggers::hold(&mut store, watch_event(due, told.changes))?;
    }
    let record = updated(
        current.rebased(&due.repository),
        &due.record,
        &walked,
        &told.followed,
        complete,
    );
    watch::put(&mut store, &due.id, &record)?;
    drop(store);
    if raised {
        triggers::wake(ctx);
    }
    Ok(raised)
}

/// `current` after `walked` was read against `read` (the record the pass
/// compared with). An incomplete read, or one with a mark not yet settled,
/// keeps no ETag, so the next asks again.
fn updated(
    mut current: WatchRecord,
    read: &WatchRecord,
    walked: &Walked,
    followed: &Followed,
    complete: bool,
) -> WatchRecord {
    assert_eq!(current.generation, read.generation, "the same watch");
    let newest = walked.pulls.iter().map(|p| p.updated_at).max();
    let highest = walked.pulls.iter().map(|p| p.number).max().unwrap_or(0);
    let marks = watch::merged_marks(
        read.pulls.as_ref(),
        &walked.pulls,
        walked.whole,
        followed,
        walked.read_at,
    );
    let settled = walked
        .pulls
        .iter()
        .all(|p| marks.get(&p.number).is_none_or(|m| m.settled));
    current.etag = walked.etag.clone().filter(|_| complete && settled);
    if complete {
        current.watermark = current.watermark.max(newest);
    }
    current.max_number = current.max_number.max(highest);
    current.fetched_at = Some(archon_core::now());
    current.pulls = Some(marks);
    assert!(
        (complete && settled) || current.etag.is_none(),
        "an incomplete or unsettled read is asked again"
    );
    current
}

/// Whether `due`'s checkout still names the `origin` its read was of. An
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
            tracing::debug!(%error, repository = %due.repository, "origin unreadable after a watch read");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "origin read after a watch read did not finish");
            false
        }
    }
}

/// The trigger for `changes`: news with no operator standing.
fn watch_event(due: &Due, mut changes: Vec<WatchedChange>) -> TriggerEvent {
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
