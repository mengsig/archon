//! Report publishing through the Lavish plugin, with a plain-path fallback
//! (ARCHITECTURE §14).
//!
//! Reports live in `kv`, one entry each (see `records`). `publish` only
//! records and announces a report; [`supervise`], run by the pump, owns one
//! poll task per open report because `publish` has no `Arc` to spawn one
//! with. A poll's failure stays with its report: only a store failure
//! reaches the pump. Once a report's item finishes, its poll is stopped and
//! its session ended (see `retire`).

mod expiry;
mod files;
mod questions;
mod records;
mod retire;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, ReportId};
use archon_core::notify::{AnswerVia, FeedEntry, NotificationKind};
use archon_core::protocol::{ReportPublished, ReportState, ReportSummary};
use archon_core::run::GateDecision;
use archon_lavish::{Lavish, LavishFeedback};
use tokio::sync::OwnedMutexGuard;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::{AbortHandle, JoinError, JoinSet};

use self::records::ReportRecord;
use crate::context::DaemonContext;
use crate::feed;
use crate::triggers::TriggerEvent;

pub(crate) use self::expiry::expire_files;
pub use self::questions::{already_closed, answer_reopen, asks_to_reopen, close_stale_questions};
pub(crate) use self::records::is_open;
pub use self::records::{KEY_PREFIX, LEGACY_KEY, prune};

/// Failures in a row, of one poll or of one report's poll task, that are
/// retried before the report is given up.
const MAX_POLL_FAILURES: u32 = 5;
const POLL_BACKOFF_MIN: Duration = Duration::from_secs(1);
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Validate the report, snapshot its directory when possible, and publish
/// the copy or its original with a durability warning. Record and announce
/// it, and supervise its poll when Lavish is available.
/// Without Lavish: `published = false` and `location` is the file path.
/// A file published before keeps its report, and `lavish-axi` resumes its
/// session rather than starting another.
pub async fn publish(
    ctx: &DaemonContext,
    title: &str,
    html: &Path,
    item: Option<&QueueItemId>,
) -> Result<ReportPublished, ArchonError> {
    publish_page(ctx, title, html, item)
        .await
        .map(|(published, _)| published)
}

/// Gate questions repeat the same durability warning posted to the feed.
pub(crate) async fn publish_page(
    ctx: &DaemonContext,
    title: &str,
    input: &Path,
    item: Option<&QueueItemId>,
) -> Result<(ReportPublished, Option<String>), ArchonError> {
    let files_guard = ctx.report_files.clone().lock_owned().await;
    let title = title.trim();
    if title.is_empty() {
        return Err(ArchonError::Invalid("a report needs a title".into()));
    }
    let original = validate_html(input)?;
    let run = match item {
        Some(item) => latest_run(ctx, item).await?,
        None => None,
    };
    let earlier = load_reports(ctx)
        .await?
        .into_iter()
        .find(|(_, record)| record.html == original || record.source.as_ref() == Some(&original));
    let first_publication = earlier.is_none();
    let (report, source) = match earlier {
        Some((id, record)) => (id, record.source.unwrap_or_else(|| original.clone())),
        None => (ReportId::new(), original.clone()),
    };
    let (copied, _files) =
        copy_snapshot(ctx, report.clone(), input, first_publication, files_guard).await;
    let (mut snapshot, warning) = warn_if_not_copied(ctx, title, &original, copied).await?;
    let source = if snapshot.is_some() {
        source
    } else {
        original.clone()
    };
    let html = snapshot.as_ref().map_or(original, |copy| copy.html.clone());
    let result = record_publication(
        ctx,
        report,
        PublishedPage {
            title,
            item,
            run,
            source,
            html,
        },
        snapshot.as_mut(),
    )
    .await;
    if result.is_err()
        && let Some(copy) = snapshot.as_mut()
        && let Err(error) = copy.abort()
    {
        feed::system(ctx, format!("Report snapshot cleanup failed: {error}")).await?;
    }
    result.map(|published| (published, warning))
}

/// A failed copy leaves the page where it is and tells the operator it is not durable.
async fn warn_if_not_copied(
    ctx: &DaemonContext,
    title: &str,
    original: &Path,
    copied: Result<files::Snapshot, ArchonError>,
) -> Result<(Option<files::Snapshot>, Option<String>), ArchonError> {
    let error = match copied {
        Ok(snapshot) => return Ok((Some(snapshot), None)),
        Err(error) => error,
    };
    let warning = format!(
        "Report \"{title}\" at {} could not be copied ({error}). The page is not durable: \
         it disappears when its workspace is collected.",
        original.display()
    );
    feed::system(ctx, warning.clone()).await?;
    Ok((None, Some(warning)))
}

/// The worker owns the lock so cancellation cannot overlap its copy with
/// a later publish or prune. The returned snapshot drops before the lock.
async fn copy_snapshot(
    ctx: &DaemonContext,
    report: ReportId,
    input: &Path,
    first_publication: bool,
    files_guard: OwnedMutexGuard<()>,
) -> (Result<files::Snapshot, ArchonError>, OwnedMutexGuard<()>) {
    let root = ctx.paths.reports.clone();
    let input = input.to_owned();
    match tokio::task::spawn_blocking(move || {
        (
            files::snapshot(&root, &report, &input, first_publication),
            files_guard,
        )
    })
    .await
    {
        Ok(result) => result,
        Err(error) => (
            Err(ArchonError::Invalid(format!(
                "report snapshot task: {error}"
            ))),
            ctx.report_files.clone().lock_owned().await,
        ),
    }
}

struct PublishedPage<'a> {
    title: &'a str,
    item: Option<&'a QueueItemId>,
    run: Option<PipelineRunId>,
    source: PathBuf,
    html: PathBuf,
}

async fn record_publication(
    ctx: &DaemonContext,
    report: ReportId,
    page: PublishedPage<'_>,
    snapshot: Option<&mut files::Snapshot>,
) -> Result<ReportPublished, ArchonError> {
    let PublishedPage {
        title,
        item,
        run,
        source,
        html,
    } = page;
    let (location, published) = open_in_lavish(ctx, &html).await?;
    let record = ReportRecord {
        source: Some(source),
        html,
        title: title.to_owned(),
        item: item.cloned(),
        run: run.clone(),
        state: if published {
            ReportState::Open
        } else {
            ReportState::Unpublished
        },
        reopen_question: None,
        location: Some(location.clone()),
        updated_at: archon_core::now(),
    };
    upsert_report(ctx, report.clone(), record).await?;
    // No await between writing the record and committing its snapshot:
    // cancellation must never roll back a directory already referenced.
    let cleanup = snapshot.and_then(|copy| copy.commit().err());
    if let Some(error) = cleanup {
        feed::system(
            ctx,
            format!("Report {report} was published, but snapshot cleanup failed: {error}"),
        )
        .await?;
    }
    announce_location(ctx, &report, title, item, &location).await?;
    Ok(ReportPublished {
        report,
        published,
        location,
        run,
    })
}

/// Keep one poll task per open report until its item finishes or shutdown.
/// New reports and item outcomes are seen through the feed; a lagged
/// receiver rescans the records. `Err` only for a store failure: any other
/// failure, a panic included, is its report's.
pub async fn supervise(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let Some(lavish) = ctx.lavish.clone() else {
        // Nothing to poll without Lavish; stay idle rather than look like a failed task.
        return std::future::pending().await;
    };
    let mut feed = ctx.feed.subscribe();
    let mut polls = Polls::default();
    polls.rescan(&ctx, &lavish).await?;
    loop {
        tokio::select! {
            received = feed.recv() => match received {
                Ok(item) => polls.on_feed(&ctx, &lavish, &item.entry).await?,
                Err(RecvError::Lagged(_)) => polls.rescan(&ctx, &lavish).await?,
                Err(RecvError::Closed) => return Ok(()),
            },
            Some(done) = polls.tasks.join_next_with_id() => {
                if let Some((report, outcome)) = polls.forget(done) {
                    polls.ended(&ctx, &lavish, report, outcome).await?;
                }
            }
        }
    }
}

/// The poll task of each report being followed, and how many times in a
/// row each report's task ended abnormally. `reports` also holds a stopped
/// task until it is joined; `polling` only the task followed now.
#[derive(Default)]
struct Polls {
    followed_html: HashMap<ReportId, PathBuf>,
    tasks: JoinSet<Result<(), ArchonError>>,
    reports: HashMap<tokio::task::Id, ReportId>,
    polling: HashMap<ReportId, AbortHandle>,
    failures: HashMap<ReportId, u32>,
}

impl Polls {
    fn spawn<F>(&mut self, report: ReportId, poll: F)
    where
        F: Future<Output = Result<(), ArchonError>> + Send + 'static,
    {
        assert!(!self.polling.contains_key(&report), "one poll per report");
        let task = self.tasks.spawn(poll);
        self.reports.insert(task.id(), report.clone());
        self.polling.insert(report, task);
        assert!(self.reports.len() >= self.polling.len());
    }

    /// Stop following `report`: its task, and with it its `lavish-axi poll`
    /// child, is dropped. `forget` then leaves out whatever it returned.
    /// Feedback a poll got just before is dropped too; `end_finished` stops
    /// only polls of finished items that no gate waits on.
    fn stop(&mut self, report: &ReportId) {
        if let Some(task) = self.polling.remove(report) {
            assert_eq!(self.reports.get(&task.id()), Some(report));
            task.abort();
        }
        self.failures.remove(report);
        self.followed_html.remove(report);
    }

    /// Act on one feed entry: follow a report just shown, or end the
    /// sessions of reports whose item just finished.
    async fn on_feed(
        &mut self,
        ctx: &Arc<DaemonContext>,
        lavish: &Lavish,
        entry: &FeedEntry,
    ) -> Result<(), ArchonError> {
        match entry {
            FeedEntry::Report { .. } => self.start_open(ctx, lavish).await,
            FeedEntry::Notification(notice)
                if matches!(
                    notice.kind,
                    NotificationKind::Outcome | NotificationKind::Blocker
                ) && notice.item.is_some() =>
            {
                self.end_finished(ctx, lavish).await
            }
            _ => Ok(()),
        }
    }

    /// End what finished while the feed was not watched, then follow the rest.
    async fn rescan(
        &mut self,
        ctx: &Arc<DaemonContext>,
        lavish: &Lavish,
    ) -> Result<(), ArchonError> {
        self.end_finished(ctx, lavish).await?;
        self.start_open(ctx, lavish).await
    }

    /// Stop the poll of every open report whose item finished, then end
    /// its Lavish session. Only this loop restarts polls, so none restarts
    /// before the report is recorded ended.
    async fn end_finished(
        &mut self,
        ctx: &Arc<DaemonContext>,
        lavish: &Lavish,
    ) -> Result<(), ArchonError> {
        for (report, record) in retire::finished(ctx).await? {
            self.stop(&report);
            assert!(
                !self.polling.contains_key(&report),
                "a stopped poll is not followed"
            );
            retire::end(ctx, lavish, &report, &record).await?;
        }
        Ok(())
    }

    /// Start a poll for every open report not followed yet.
    async fn start_open(
        &mut self,
        ctx: &Arc<DaemonContext>,
        lavish: &Lavish,
    ) -> Result<(), ArchonError> {
        for (report, record) in load_reports(ctx).await? {
            if record.state != ReportState::Open {
                continue;
            }
            if self
                .followed_html
                .get(&report)
                .is_some_and(|html| html != &record.html)
            {
                self.stop(&report);
            }
            if self.polling.contains_key(&report) {
                continue;
            }
            self.followed_html
                .insert(report.clone(), record.html.clone());
            let poll = follow_report(ctx.clone(), lavish.clone(), report.clone(), record);
            self.spawn(report, poll);
        }
        Ok(())
    }

    /// The report a finished task polled, and how it ended; a panic is a
    /// failure like any other. `None` for a task `stop` ended, unless it
    /// failed on the store: its report is no longer followed by it.
    fn forget(
        &mut self,
        done: Result<(tokio::task::Id, Result<(), ArchonError>), JoinError>,
    ) -> Option<(ReportId, Result<(), ArchonError>)> {
        let (task, outcome) = match done {
            Ok((task, outcome)) => (task, outcome),
            Err(error) => (error.id(), Err(task_failure(&error))),
        };
        let report = self
            .reports
            .remove(&task)
            .expect("every poll task is registered");
        if self.polling.get(&report).map(AbortHandle::id) == Some(task) {
            self.polling.remove(&report);
            self.followed_html.remove(&report);
            return Some((report, outcome));
        }
        match outcome {
            // `ended` passes a store failure on and touches nothing else.
            Err(error @ ArchonError::Store(_)) => Some((report, Err(error))),
            outcome => {
                tracing::debug!(%report, ?outcome, "a stopped report poll ended");
                None
            }
        }
    }

    /// Restart a poll that failed after a backoff, or give its report up
    /// after [`MAX_POLL_FAILURES`] in a row. A store failure is returned.
    async fn ended(
        &mut self,
        ctx: &Arc<DaemonContext>,
        lavish: &Lavish,
        report: ReportId,
        outcome: Result<(), ArchonError>,
    ) -> Result<(), ArchonError> {
        let error = match outcome {
            Ok(()) => {
                self.failures.remove(&report);
                return Ok(());
            }
            Err(error) if error.is_store() => return Err(error),
            Err(error) => error,
        };
        let failures = self.failures.entry(report.clone()).or_default();
        *failures += 1;
        tracing::warn!(%report, failures = *failures, %error, "report poll task failed");
        let Some(delay) = backoff(*failures) else {
            self.failures.remove(&report);
            return give_up(ctx, &report, &error).await;
        };
        let restart = restart_after(ctx.clone(), lavish.clone(), report.clone(), delay);
        self.spawn(report, restart);
        Ok(())
    }
}

fn task_failure(error: &JoinError) -> ArchonError {
    ArchonError::Unavailable(format!("report poll task ended abnormally: {error}"))
}

/// The wait before retry `failures` (counted from one); `None` once
/// [`MAX_POLL_FAILURES`] retries were spent.
fn backoff(failures: u32) -> Option<Duration> {
    assert!(failures >= 1, "failures are counted from one");
    if failures > MAX_POLL_FAILURES {
        return None;
    }
    let delay = POLL_BACKOFF_MIN.saturating_mul(1 << (failures - 1));
    Some(delay.min(POLL_BACKOFF_MAX))
}

/// Follow the report again after `delay`, if it is still open then.
async fn restart_after(
    ctx: Arc<DaemonContext>,
    lavish: Lavish,
    report: ReportId,
    delay: Duration,
) -> Result<(), ArchonError> {
    tokio::time::sleep(delay).await;
    let Some(record) = load_report(&ctx, &report).await? else {
        return Ok(());
    };
    if record.state != ReportState::Open {
        return Ok(());
    }
    follow_report(ctx, lavish, report, record).await
}

/// Mark a still-open report `Stopped` and tell the operator where it is.
async fn give_up(
    ctx: &DaemonContext,
    report: &ReportId,
    error: &ArchonError,
) -> Result<(), ArchonError> {
    let Some(record) = load_report(ctx, report).await? else {
        return Ok(());
    };
    if record.state != ReportState::Open {
        return Ok(());
    }
    set_state(ctx, report, ReportState::Stopped).await?;
    let text = format!(
        "Stopped following Lavish feedback on \"{}\" after {MAX_POLL_FAILURES} failed restarts ({error}). The report is at {}.",
        record.title,
        record.html.display()
    );
    feed::system(ctx, text).await.map(drop)
}

/// Re-run `lavish-axi poll` until the session ends or disconnects. A poll
/// that fails, or feedback Archon cannot act on, is retried with backoff
/// and given up after [`MAX_POLL_FAILURES`] in a row. `Err` only for a
/// store failure.
async fn follow_report(
    ctx: Arc<DaemonContext>,
    lavish: Lavish,
    report: ReportId,
    record: ReportRecord,
) -> Result<(), ArchonError> {
    assert_eq!(
        record.state,
        ReportState::Open,
        "only open reports are polled"
    );
    let mut failures = 0;
    loop {
        let error = match poll_once(&ctx, &lavish, &report, &record).await {
            Ok(true) => {
                failures = 0;
                continue;
            }
            Ok(false) => return Ok(()),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => error,
        };
        failures += 1;
        tracing::warn!(%report, failures, %error, "following a lavish report failed");
        let Some(delay) = backoff(failures) else {
            return give_up(&ctx, &report, &error).await;
        };
        tokio::time::sleep(delay).await;
        if !is_open(&mut *ctx.store.lock().await, &report)? {
            return Ok(());
        }
    }
}

/// One poll and its feedback; `false` once the report needs no more
/// polling. Feedback Archon cannot act on is reported to the operator.
async fn poll_once(
    ctx: &DaemonContext,
    lavish: &Lavish,
    report: &ReportId,
    record: &ReportRecord,
) -> Result<bool, ArchonError> {
    let feedback = lavish.poll(&record.html, None).await?;
    match handle_feedback(ctx, report, record, feedback).await {
        Err(error) if !error.is_store() => {
            let text = format!(
                "Archon could not act on Lavish feedback on \"{}\": {error}. It keeps following the page.",
                record.title
            );
            feed::system(ctx, text).await?;
            Err(error)
        }
        handled => handled,
    }
}

/// Route one poll result; `false` once the report needs no more polling.
/// Prompts on a page an operator gate waits on decide that gate.
async fn handle_feedback(
    ctx: &DaemonContext,
    report: &ReportId,
    record: &ReportRecord,
    feedback: LavishFeedback,
) -> Result<bool, ArchonError> {
    let (raw, prompts, keep_polling) = match feedback {
        LavishFeedback::Prompts { raw, prompts } => (raw, prompts, true),
        LavishFeedback::ArtifactFailure { raw } => (raw, Vec::new(), true),
        LavishFeedback::Ended { raw, prompts } => {
            set_state(ctx, report, ReportState::Ended).await?;
            (raw, prompts, false)
        }
        LavishFeedback::Disconnected { .. } => {
            questions::disconnected(ctx, report).await?;
            return Ok(false);
        }
    };
    // A pin whose prompt was left empty answers nothing.
    let prompts: Vec<String> = prompts
        .into_iter()
        .filter(|prompt| !prompt.trim().is_empty())
        .collect();
    if !prompts.is_empty() && decide_gate(ctx, report, &prompts).await? {
        return Ok(keep_polling);
    }
    // The trigger leaves out the DOM snapshot and bounds the rest.
    tracing::debug!(%report, raw, "lavish feedback for the orchestrator");
    let feedback =
        TriggerEvent::lavish_feedback(report.clone(), record.item.clone(), &raw, prompts);
    crate::triggers::push(ctx, feedback).await?;
    Ok(keep_polling)
}

/// Decide the gate open on `report` with the page's prompts, as if they
/// answered its question, which is closed only once the decision is acted
/// on. `false` when no gate waits on the page; then the orchestrator gets
/// them.
async fn decide_gate(
    ctx: &DaemonContext,
    report: &ReportId,
    prompts: &[String],
) -> Result<bool, ArchonError> {
    assert!(!prompts.is_empty(), "only prompts decide a gate");
    let _answering = ctx.answering.lock().await;
    let Some(gate) = ctx.store.lock().await.open_gate_by_report(report)? else {
        return Ok(false);
    };
    assert_eq!(
        gate.report.as_ref(),
        Some(report),
        "the gate shows this report"
    );
    let text = prompts.join("\n");
    let decision = GateDecision::from_answer(&text)?;
    // A failure must not stop the report polls; the operator hears of it.
    if let Err(error) = crate::executor::decide_gate(ctx, &gate, decision).await {
        tracing::warn!(%error, %report, "acting on a gate decision from Lavish failed");
        let text = format!(
            "Acting on your Lavish answer for {} failed: {error}. Its question is still open.",
            gate.item
        );
        feed::system(ctx, text).await?;
        return Ok(true);
    }
    let Some(question) = &gate.question else {
        return Ok(true);
    };
    let answered = ctx
        .store
        .lock()
        .await
        .answer_question(question, &text, archon_core::now());
    match answered {
        Ok(closed) => assert!(!closed.is_open(), "an answered question is closed"),
        // Closed by an older archond before its gate was decided.
        Err(ArchonError::InvalidTransition(_)) => return Ok(true),
        Err(error) => return Err(error),
    }
    let entry = FeedEntry::Answer {
        question: question.clone(),
        text,
        via: AnswerVia::Archon,
    };
    feed::append(ctx, entry).await?;
    Ok(true)
}

/// `(location, published)`: the session URL, or the file path when Lavish is
/// absent or cannot open it (then a `System` note says why).
async fn open_in_lavish(ctx: &DaemonContext, html: &Path) -> Result<(String, bool), ArchonError> {
    let path = html.display().to_string();
    let Some(lavish) = &ctx.lavish else {
        return Ok((path, false));
    };
    match lavish.open(html).await {
        Ok(session) => Ok((session.url, true)),
        Err(error) => {
            tracing::warn!(%error, html = %path, "lavish could not open the report");
            let text =
                format!("Lavish could not open {path} ({error}); showing the file path instead.");
            feed::system(ctx, text).await?;
            Ok((path, false))
        }
    }
}

/// Absolute, existing `.html`/`.htm` file, canonicalized so one report file
/// always maps to one record.
fn validate_html(html: &Path) -> Result<PathBuf, ArchonError> {
    if !html.is_absolute() {
        return Err(ArchonError::Invalid(format!(
            "report path {} must be absolute",
            html.display()
        )));
    }
    let is_html = html
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"));
    if !is_html {
        return Err(ArchonError::Invalid(format!(
            "report {} is not an .html file",
            html.display()
        )));
    }
    let canonical = std::fs::canonicalize(html).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ArchonError::NotFound(format!("report {}", html.display())),
        _ => ArchonError::Invalid(format!("resolving report {}: {e}", html.display())),
    })?;
    if !canonical.is_file() {
        return Err(ArchonError::Invalid(format!(
            "report {} is not a file",
            html.display()
        )));
    }
    Ok(canonical)
}

/// The item's newest run; the item must exist.
async fn latest_run(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<Option<PipelineRunId>, ArchonError> {
    let store = ctx.store.lock().await;
    store.item(item)?;
    let runs = store.runs_for_item(item)?;
    Ok(runs.last().map(|run| run.id.clone()))
}

/// Store a shown report under its allocated id. Close every reopen
/// question about its served or original file, including legacy orphans.
async fn upsert_report(
    ctx: &DaemonContext,
    report: ReportId,
    record: ReportRecord,
) -> Result<ReportId, ArchonError> {
    assert!(
        record.reopen_question.is_none(),
        "a report being shown asks nothing"
    );
    let (report, closed) = {
        let mut store = ctx.store.lock().await;
        let now = archon_core::now();
        let asked = records::get(&mut store, &report)?.and_then(|r| r.reopen_question);
        let mut closed =
            questions::withdraw_shown_again(&mut store, &record.html, asked.as_ref(), now)?;
        if let Some(source) = &record.source {
            closed.extend(questions::withdraw_shown_again(
                &mut store, source, None, now,
            )?);
        }
        records::put(&mut store, &report, record, now)?;
        (report, closed)
    };
    feed::publish(ctx, closed);
    Ok(report)
}

/// Record `record` as shown at its location, and say where in the feed.
async fn announce(
    ctx: &DaemonContext,
    report: ReportId,
    record: ReportRecord,
) -> Result<ReportId, ArchonError> {
    let location = record
        .location
        .clone()
        .expect("a report being shown has a location");
    let (title, item) = (record.title.clone(), record.item.clone());
    let report = upsert_report(ctx, report, record).await?;
    announce_location(ctx, &report, &title, item.as_ref(), &location).await?;
    Ok(report)
}

async fn announce_location(
    ctx: &DaemonContext,
    report: &ReportId,
    title: &str,
    item: Option<&QueueItemId>,
    location: &str,
) -> Result<(), ArchonError> {
    let entry = FeedEntry::Report {
        report: report.clone(),
        title: title.to_owned(),
        location: location.to_owned(),
        item: item.cloned(),
    };
    feed::append(ctx, entry).await?;
    Ok(())
}

/// Every recorded report, most recently changed first.
pub async fn list(ctx: &DaemonContext) -> Result<Vec<ReportSummary>, ArchonError> {
    let mut reports: Vec<ReportSummary> = load_reports(ctx)
        .await?
        .iter()
        .map(|(report, record)| record.summary(report))
        .collect();
    reports.sort_by_key(|report| std::cmp::Reverse(report.updated_at));
    Ok(reports)
}

/// Show a recorded report again at the operator's request. Its Lavish
/// session is resumed, one the operator ended included, and polled again.
/// Without Lavish its path is named. Unless it is shown, the record is
/// left as it was.
pub async fn reopen(
    ctx: &DaemonContext,
    report: &ReportId,
) -> Result<ReportPublished, ArchonError> {
    if let Some(lavish) = &ctx.lavish {
        return show_again(ctx, lavish, report).await;
    }
    let _files = ctx.report_files.lock().await;
    let record = load_report(ctx, report)
        .await?
        .ok_or_else(|| ArchonError::NotFound(format!("report {report}")))?;
    validate_html(&record.html)?;
    Ok(ReportPublished {
        report: report.clone(),
        published: false,
        location: record.html.display().to_string(),
        run: record.run,
    })
}

/// Reopen the report's Lavish session (`--reopen`: the operator asked) and
/// record it `Open`; its poll restarts from the feed entry. A failure
/// leaves the record untouched and says what it still is.
async fn show_again(
    ctx: &DaemonContext,
    lavish: &Lavish,
    report: &ReportId,
) -> Result<ReportPublished, ArchonError> {
    let _files = ctx.report_files.lock().await;
    let record = load_report(ctx, report)
        .await?
        .ok_or_else(|| ArchonError::NotFound(format!("report {report}")))?;
    validate_html(&record.html)?;
    let session = lavish.reopen(&record.html).await.map_err(|error| {
        ArchonError::Unavailable(format!(
            "Lavish could not show \"{}\" ({report}) again: {error}. It is still {}, at {}",
            record.title,
            record.state.as_str(),
            record.html.display()
        ))
    })?;
    let run = record.run.clone();
    let shown = ReportRecord {
        state: ReportState::Open,
        reopen_question: None,
        location: Some(session.url.clone()),
        ..record
    };
    let shown_as = announce(ctx, report.clone(), shown).await?;
    debug_assert_eq!(&shown_as, report, "one record per report file");
    Ok(ReportPublished {
        report: shown_as,
        published: true,
        location: session.url,
        run,
    })
}

async fn set_state(
    ctx: &DaemonContext,
    report: &ReportId,
    state: ReportState,
) -> Result<(), ArchonError> {
    update_record(ctx, report, |record| record.state = state).await
}

async fn update_record(
    ctx: &DaemonContext,
    report: &ReportId,
    change: impl FnOnce(&mut ReportRecord),
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut record = records::get(&mut store, report)?
        .ok_or_else(|| ArchonError::Corrupt(format!("report {report} has no record")))?;
    change(&mut record);
    records::put(&mut store, report, record, archon_core::now())
}

async fn load_reports(
    ctx: &DaemonContext,
) -> Result<BTreeMap<ReportId, ReportRecord>, ArchonError> {
    records::all(&mut *ctx.store.lock().await)
}

async fn load_report(
    ctx: &DaemonContext,
    report: &ReportId,
) -> Result<Option<ReportRecord>, ArchonError> {
    records::get(&mut *ctx.store.lock().await, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_existing_html_files_are_reports() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("index.html");
        std::fs::write(&html, "<p>hi</p>").unwrap();
        assert_eq!(
            validate_html(&html).unwrap(),
            std::fs::canonicalize(&html).unwrap()
        );
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "hi").unwrap();
        assert!(matches!(validate_html(&text), Err(ArchonError::Invalid(_))));
        assert!(matches!(
            validate_html(Path::new("index.html")),
            Err(ArchonError::Invalid(_))
        ));
        let missing = dir.path().join("missing.html");
        assert!(matches!(
            validate_html(&missing),
            Err(ArchonError::NotFound(_))
        ));
        let folder = dir.path().join("folder.html");
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(
            validate_html(&folder),
            Err(ArchonError::Invalid(_))
        ));
    }

    #[test]
    fn restarts_back_off_and_are_bounded() {
        let delays: Vec<_> = (1..=MAX_POLL_FAILURES)
            .map(|n| backoff(n).unwrap())
            .collect();
        assert_eq!(delays[0], POLL_BACKOFF_MIN);
        assert!(
            delays
                .windows(2)
                .all(|w| w[0] < w[1] || w[1] == POLL_BACKOFF_MAX)
        );
        assert!(delays.iter().all(|d| *d <= POLL_BACKOFF_MAX));
        assert_eq!(backoff(MAX_POLL_FAILURES + 1), None, "then given up");
    }

    /// A panicking poll task names its report and is a failure the
    /// supervisor contains, never a store failure it passes on.
    #[tokio::test]
    async fn a_panicking_poll_is_a_contained_failure_of_its_report() {
        let mut polls = Polls::default();
        let report = ReportId::new();
        polls.spawn(report.clone(), async { panic!("poll bug") });
        let done = polls.tasks.join_next_with_id().await.unwrap();
        let (ended, outcome) = polls.forget(done).expect("a followed poll");
        assert_eq!(ended, report);
        assert!(
            matches!(outcome, Err(ArchonError::Unavailable(_))),
            "{outcome:?}"
        );
        assert!(polls.polling.is_empty() && polls.reports.is_empty());
    }
}
