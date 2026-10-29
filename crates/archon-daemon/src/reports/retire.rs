//! Ending the Lavish sessions of reports Archon no longer follows: their
//! item finished, a newer page of the same title replaced them, or they were
//! shown longer ago than [`FOLLOW_LIFETIME`]. Each open report keeps a
//! `lavish-axi poll` process alive until its session ends, so one left open
//! would outlive its purpose. The report file is kept.

use std::collections::BTreeMap;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::ReportId;
use archon_core::protocol::ReportState;
use archon_lavish::Lavish;
use archon_queue::Store;

use super::records::{self, ReportRecord};
use crate::context::DaemonContext;
use crate::feed;

/// How long after a report was last shown Archon stops following its
/// Lavish session; `archon reports reopen` shows it again.
pub const FOLLOW_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// When Archon stops following `record`'s session unless it settles first.
pub(super) fn expires_at(record: &ReportRecord) -> Timestamp {
    let lifetime = chrono::TimeDelta::from_std(FOLLOW_LIFETIME).expect("the lifetime fits");
    record.updated_at + lifetime
}

/// Open reports that are settled and that no open gate waits on: their item
/// settled after they were shown or was pruned, a newer page replaced them,
/// or their lifetime ran out. A report shown about an item that had already
/// settled was meant for review then, so only its lifetime ends it.
pub(super) async fn finished(
    ctx: &DaemonContext,
) -> Result<Vec<(ReportId, ReportRecord)>, ArchonError> {
    let now = archon_core::now();
    let mut store = ctx.store.lock().await;
    let all = records::all(&mut store)?;
    let mut done = Vec::new();
    for (report, record) in &all {
        if record.state == ReportState::Open && is_finished(&store, report, record, &all, now)? {
            done.push((report.clone(), record.clone()));
        }
    }
    assert!(done.len() <= all.len(), "only recorded reports end");
    Ok(done)
}

fn is_finished(
    store: &Store,
    report: &ReportId,
    record: &ReportRecord,
    all: &BTreeMap<ReportId, ReportRecord>,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    assert_eq!(record.state, ReportState::Open, "only open reports end");
    let settled = now >= expires_at(record)
        || is_superseded(report, record, all)
        || item_settled(store, record)?;
    if !settled {
        return Ok(false);
    }
    let gate = store.open_gate_by_report(report)?;
    assert!(
        gate.as_ref()
            .is_none_or(|g| g.report.as_ref() == Some(report)),
        "a gate found by report shows it"
    );
    Ok(gate.is_none())
}

/// Whether `record`'s item settled after it was shown, or was pruned.
fn item_settled(store: &Store, record: &ReportRecord) -> Result<bool, ArchonError> {
    let Some(item) = &record.item else {
        return Ok(false);
    };
    match store.item(item) {
        Ok(found) => Ok(found.state.is_terminal() && found.updated_at >= record.updated_at),
        Err(ArchonError::NotFound(_)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether a page of the same title and item was shown after `record`, as
/// when an orchestrator or a retried stage publishes a page anew.
fn is_superseded(
    report: &ReportId,
    record: &ReportRecord,
    all: &BTreeMap<ReportId, ReportRecord>,
) -> bool {
    all.iter().any(|(other, newer)| {
        other != report
            && newer.title == record.title
            && newer.item == record.item
            && (newer.updated_at, other) > (record.updated_at, report)
    })
}

/// End `report`'s Lavish session, then record the report ended; its poll
/// task must already be stopped. Ending first means a daemon stopped in
/// between still finds the report open, and ends it, at its next start.
/// A session Lavish fails to end is told to the operator: the report is
/// recorded ended all the same and can be reopened.
pub(super) async fn end(
    ctx: &DaemonContext,
    lavish: &Lavish,
    report: &ReportId,
    record: &ReportRecord,
) -> Result<(), ArchonError> {
    assert_eq!(record.state, ReportState::Open, "a report is ended once");
    assert!(record.html.is_absolute(), "lavish-axi takes absolute paths");
    tracing::info!(%report, html = %record.html.display(), "ending a settled report's Lavish session");
    let ended = lavish.end(&record.html).await;
    record_ended(ctx, report, record).await?;
    let Err(error) = ended else {
        return Ok(());
    };
    tracing::warn!(%report, %error, "lavish could not end a settled report's session");
    let text = format!(
        "Lavish could not end the session of \"{}\" ({error}); Archon no longer follows it. The report is at {}.",
        record.title,
        record.html.display()
    );
    feed::system(ctx, text).await.map(drop)
}

/// Record `report` ended unless its record changed since `seen`, as when
/// `archon reports reopen` showed it again meanwhile; then it is left as is.
async fn record_ended(
    ctx: &DaemonContext,
    report: &ReportId,
    seen: &ReportRecord,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let Some(current) = records::get(&mut store, report)? else {
        return Err(ArchonError::Corrupt(format!(
            "report {report} has no record"
        )));
    };
    if current != *seen {
        tracing::info!(%report, state = current.state.as_str(), "the report changed while its session ended");
        return Ok(());
    }
    let ended = ReportRecord {
        state: ReportState::Ended,
        ..current
    };
    records::put(&mut store, report, ended, archon_core::now())?;
    assert!(
        !records::is_open(&mut store, report)?,
        "an ended report is not polled"
    );
    Ok(())
}
