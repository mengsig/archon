//! The repository watches' share of a sync pass. Each page of the list is a
//! `GET` conditional on its own ETag, so an unchanged list costs one per page
//! (one for up to 100 pull requests); GitHub's order by update is not exact
//! enough to stop early. A comment or review check is asked only for a pull
//! request that moved with nothing else to show, or whose last mark was too
//! recent to trust. Every `GET` counts against [`MAX_WATCH_REQUESTS`]
//! and the pass's [`MAX_REQUESTS_PER_PASS`].

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::id::RepoId;
use archon_core::protocol::github_links::MIN_WATCH_MINUTES;
use archon_core::repo::Repo;
use archon_queue::Store;
use tokio::time::Instant;

use super::watch::{self, Followed, PageMark, Pull, PullMark, Seen, WatchRecord, WatchedChange};
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
    /// Reads stopped because the watch or registration changed meanwhile.
    pub revoked: usize,
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
    /// Each page asked, in order, with the ETag it was answered with.
    pages: Vec<PageMark>,
    /// The pull requests of the pages that changed.
    pulls: Vec<Pull>,
    /// No page was left unread for want of budget.
    complete: bool,
    /// GitHub's clock at the first page, which marks are settled against.
    read_at: Timestamp,
    /// Every pull request listed, when every page was asked.
    listed: Option<BTreeSet<u64>>,
}

/// What a scan of the list's pages found.
enum Scan {
    /// Every page asked was a 304, and none was left unasked.
    Unchanged,
    /// The budget ran out before any page changed.
    Cut,
    Changed(Walked),
}

/// How one watch's read went, for the pass.
struct Outcome {
    backoff: Option<Duration>,
    /// Pages or follow-ups were left unread: the watch is asked first next pass.
    cut: bool,
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
            if !fits(pass, &due.record) {
                continue;
            }
            self.watch_asked.insert(due.id.clone(), Instant::now());
            let read = read_one(ctx, &due, pass).await?;
            if read.cut {
                self.watch_asked.remove(&due.id);
            }
            if let Some(wait) = read.backoff {
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

/// Whether the pass has room for every page `record` last read, so an
/// unchanged list is never cut short; one without waits, first, for the next.
fn fits(pass: &Pass, record: &WatchRecord) -> bool {
    let left = MAX_WATCH_REQUESTS
        .saturating_sub(pass.watch.requested)
        .min(MAX_REQUESTS_PER_PASS.saturating_sub(pass.requested));
    assert!(record.pages.len() <= MAX_PAGES as usize, "a mark per page");
    record.pages.len().max(1) <= left
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
/// Before each, the watch and registration it reads under are checked again.
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

/// Read `due`'s list and what it needs.
async fn read_one(ctx: &DaemonContext, due: &Due, pass: &mut Pass) -> Result<Outcome, ArchonError> {
    let mut asker = Asker {
        ctx,
        due,
        backoff: None,
        revoked: false,
    };
    let scanned = walk(&mut asker, pass).await;
    let mut outcome = Outcome {
        backoff: None,
        cut: false,
    };
    if asker.revoked {
        tracing::info!(repository = %due.repository, "watch or registration changed during a read; stopped");
        pass.watch.revoked += 1;
        outcome.backoff = asker.backoff;
        return Ok(outcome);
    }
    let walked = match scanned {
        Ok(Scan::Changed(walked)) => walked,
        Ok(Scan::Unchanged) => {
            pass.watch.unchanged += 1;
            outcome.backoff = asker.backoff;
            return Ok(outcome);
        }
        Ok(Scan::Cut) => {
            pass.watch.incomplete += 1;
            outcome.backoff = asker.backoff;
            outcome.cut = true;
            return Ok(outcome);
        }
        Err(error) => {
            tracing::debug!(%error, repository = %due.repository, "repository watch read failed");
            pass.watch.failed += 1;
            outcome.backoff = asker.backoff;
            return Ok(outcome);
        }
    };
    pass.watch.changed += 1;
    let told = match &due.record.pulls {
        Some(_) => follow_up(&mut asker, pass, &walked.pulls).await,
        None => Told::default(),
    };
    outcome.backoff = asker.backoff;
    if asker.revoked {
        pass.watch.revoked += 1;
        return Ok(outcome);
    }
    outcome.cut = !walked.complete || !told.followed.kept.is_empty();
    if outcome.cut {
        pass.watch.incomplete += 1;
    }
    if record_list(ctx, due, walked, told).await? {
        pass.watch.triggers += 1;
    }
    Ok(outcome)
}

/// Every page of the list, each conditional on its last ETag; a full page
/// asks the next, as a 304 cannot say one was added. Ends short of a full
/// page, or at the cap. One `GET` a page: one for up to 100 pull requests.
async fn walk(asker: &mut Asker<'_>, pass: &mut Pass) -> Result<Scan, ArchonError> {
    let due = asker.due;
    let (mut pages, mut pulls) = (Vec::new(), Vec::<Pull>::new());
    let (mut read_at, mut changed, mut complete) = (None, false, true);
    for page in 1..=MAX_PAGES {
        let stored = due.record.pages.get(page as usize - 1);
        let etag = stored.and_then(|p| p.etag.as_deref());
        let endpoint = watch_read::list_endpoint(&due.repository, page);
        let (mark, date) = match asker.get(pass, &endpoint, etag).await {
            None => {
                complete = false;
                break;
            }
            Some(Err(error)) => return Err(error),
            Some(Ok(Got::NotModified { date })) => {
                let mark = stored.cloned().expect("only a page read before is a 304");
                (mark, date)
            }
            Some(Ok(Got::Changed {
                etag, body, date, ..
            })) => {
                let read = watch::parse_pulls(&due.repository, &body)?;
                let full = read.len() >= PAGE;
                let numbers = read.iter().map(|p| p.number).collect();
                let seen: HashSet<u64> = pulls.iter().map(|p| p.number).collect();
                pulls.extend(read.into_iter().filter(|p| !seen.contains(&p.number)));
                changed = true;
                let mark = PageMark {
                    etag,
                    full,
                    numbers,
                };
                (mark, date)
            }
        };
        read_at = read_at.or(Some(date.unwrap_or_else(archon_core::now)));
        let full = mark.full;
        pages.push(mark);
        if !full {
            break;
        }
    }
    assert!(pulls.len() <= PAGE * MAX_PAGES as usize, "bounded pages");
    let read_at = match read_at {
        Some(read_at) if changed => read_at,
        _ if complete => return Ok(Scan::Unchanged),
        _ => return Ok(Scan::Cut),
    };
    assert!(!pages.is_empty(), "a changed list has a page");
    let listed = complete.then(|| {
        pages
            .iter()
            .flat_map(|p| p.numbers.iter().copied())
            .collect()
    });
    Ok(Scan::Changed(Walked {
        pages,
        pulls,
        complete,
        read_at,
        listed,
    }))
}

/// Pages of one pull request's reviews a check reads at most: the newest.
const MAX_REVIEW_PAGES: u32 = 10;

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
        let Some((commented, comment_id)) = comments(asker, pass, pull.number, mark).await else {
            told.followed.kept.insert(pull.number);
            continue;
        };
        let Some(read) = reviews(asker, pass, pull.number).await else {
            told.followed.kept.insert(pull.number);
            continue;
        };
        let (reviewed, seen) = mark.reviewed.after(mark, &read);
        if commented {
            told.changes.push(pull.told("new_comment", Vec::new()));
        } else if reviewed {
            told.changes.push(pull.told("new_review", Vec::new()));
        }
        let seen = Seen {
            comment_id,
            reviewed: seen,
        };
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

/// Whether #`number` has a comment newer than `mark`, and the newest comment
/// id seen. Comments get their ids when made, so the first page and, when
/// there are more, the last, which holds the newest, tell. `None` when unread.
async fn comments(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    number: u64,
    mark: &PullMark,
) -> Option<(bool, u64)> {
    // `since` filters by update; a second's overlap keeps the boundary in.
    let since = mark.updated_at - chrono::TimeDelta::seconds(1);
    let repository = asker.due.repository.clone();
    let endpoint = |page| watch_read::comments_endpoint(&repository, number, since, page);
    let (mut items, last) = read_feed(asker, pass, &endpoint(1), Feed::Comments).await?;
    if let Some(last) = last {
        items.extend(
            read_feed(asker, pass, &endpoint(last), Feed::Comments)
                .await?
                .0,
        );
    }
    let known = mark.comment_id;
    let news = items
        .iter()
        .any(|a| a.id > known && a.at.is_some_and(|at| mark.is_after(at)));
    let newest = items.iter().map(|a| a.id).max().unwrap_or(0).max(known);
    assert!(!news || newest > known, "a new comment has a new id");
    Some((news, newest))
}

/// #`number`'s reviews: every page, or the newest [`MAX_REVIEW_PAGES`]. A
/// review started long ago may be submitted now, so no page is skipped
/// below that bound. `None` when a page went unread.
async fn reviews(
    asker: &mut Asker<'_>,
    pass: &mut Pass,
    number: u64,
) -> Option<Vec<watch_read::Activity>> {
    let repository = asker.due.repository.clone();
    let endpoint = |page| watch_read::reviews_endpoint(&repository, number, page);
    let (mut items, last) = read_feed(asker, pass, &endpoint(1), Feed::Reviews).await?;
    let last = last.unwrap_or(1);
    let first = match last {
        last if last <= MAX_REVIEW_PAGES => 2,
        last => {
            items.clear();
            last - (MAX_REVIEW_PAGES - 1)
        }
    };
    for page in first..=last {
        items.extend(
            read_feed(asker, pass, &endpoint(page), Feed::Reviews)
                .await?
                .0,
        );
    }
    assert!(
        items.len() <= PAGE * MAX_REVIEW_PAGES as usize,
        "bounded review pages"
    );
    Some(items)
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
        Ok(Got::NotModified { .. }) => return Some((Vec::new(), None)),
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
    if !still_current(&store, due)? {
        return Ok(false);
    }
    let Some(current) = watch::get(&store, &due.id)? else {
        unreachable!("a current watch is stored");
    };
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
    let highest = walked.pulls.iter().map(|p| p.number).max().unwrap_or(0);
    let marks = watch::merged_marks(
        read.pulls.as_ref(),
        &walked.pulls,
        walked.listed.as_ref(),
        followed,
        walked.read_at,
    );
    let settled = walked
        .pulls
        .iter()
        .all(|p| marks.get(&p.number).is_none_or(|m| m.settled));
    current.pages = if complete && settled {
        walked.pages.clone()
    } else {
        Vec::new()
    };
    if read.pulls.is_none() {
        current.baseline_at = Some(walked.read_at);
    }
    current.max_number = current.max_number.max(highest);
    current.fetched_at = Some(archon_core::now());
    current.pulls = Some(marks);
    assert!(
        (complete && settled) || current.pages.is_empty(),
        "an incomplete or unsettled read is asked again"
    );
    assert!(
        current.baseline_at >= read.baseline_at,
        "the baseline clock never moves back"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn walked(numbers: &[u64], complete: bool, read_at: &str) -> Walked {
        let pulls: Vec<serde_json::Value> = numbers
            .iter()
            .map(|n| serde_json::json!({"number": n, "head": {"sha": "a"}, "updated_at": "2026-11-02T00:00:00Z"}))
            .collect();
        let body = serde_json::Value::from(pulls).to_string();
        Walked {
            pages: vec![PageMark {
                etag: Some(format!("W/\"{read_at}\"")),
                full: false,
                numbers: numbers.to_vec(),
            }],
            pulls: watch::parse_pulls("o/r", &body).unwrap(),
            complete,
            read_at: read_at.parse().unwrap(),
            listed: complete.then(|| numbers.iter().copied().collect()),
        }
    }

    #[test]
    fn the_baseline_clock_stays_and_only_a_whole_settled_read_keeps_etags() {
        let none = Followed::default();
        let read = WatchRecord::new(5, "o/r".into());
        let baseline = walked(&[3], true, "2026-11-10T00:00:00Z");
        let based = updated(read.clone(), &read, &baseline, &none, true);
        assert_eq!(based.baseline_at, Some(baseline.read_at));
        assert_eq!(
            based.pages, baseline.pages,
            "a settled whole read is conditional next"
        );
        let cut = updated(
            based.clone(),
            &based,
            &walked(&[9], false, "2026-11-12T00:00:00Z"),
            &none,
            false,
        );
        assert!(cut.pages.is_empty(), "every page is asked afresh");
        let whole = walked(&[9], true, "2026-11-13T00:00:00Z");
        let next = updated(cut.clone(), &cut, &whole, &none, true);
        assert_eq!(
            next.baseline_at, based.baseline_at,
            "the cutoff never moves"
        );
        assert_eq!(next.max_number, 9);
        let marks = next.pulls.unwrap();
        assert!(marks[&9].listed && !marks[&3].listed, "#3 left the list");
    }

    #[test]
    fn a_watch_whose_pages_do_not_fit_the_pass_waits_for_the_next() {
        let mut record = WatchRecord::new(5, "o/r".into());
        let page = PageMark {
            etag: Some("W/\"p\"".into()),
            full: true,
            numbers: Vec::new(),
        };
        record.pages = vec![page; MAX_PAGES as usize];
        let mut pass = Pass::default();
        assert!(fits(&pass, &record));
        pass.watch.requested = MAX_WATCH_REQUESTS - MAX_PAGES as usize + 1;
        pass.requested = pass.watch.requested;
        assert!(!fits(&pass, &record), "ten pages, nine left");
        assert!(
            fits(&pass, &WatchRecord::new(5, "o/r".into())),
            "a first read takes one"
        );
    }
}
