//! Ending the Lavish sessions of reports whose item has finished. Each open
//! report keeps a `lavish-axi poll` process alive until its session ends, so
//! one left open outlives its item. The report file is kept.

use archon_core::error::ArchonError;
use archon_core::id::ReportId;
use archon_core::protocol::ReportState;
use archon_lavish::Lavish;
use archon_queue::Store;

use super::records::{self, ReportRecord};
use crate::context::DaemonContext;
use crate::feed;

/// Open reports whose item settled after they were shown, or was pruned,
/// and that no open gate waits on. A report shown about an item that had
/// already settled was meant for review then, so it is left open.
pub(super) async fn finished(
    ctx: &DaemonContext,
) -> Result<Vec<(ReportId, ReportRecord)>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut done = Vec::new();
    for (report, record) in records::all(&mut store)? {
        if record.state == ReportState::Open && is_finished(&store, &report, &record)? {
            done.push((report, record));
        }
    }
    Ok(done)
}

fn is_finished(
    store: &Store,
    report: &ReportId,
    record: &ReportRecord,
) -> Result<bool, ArchonError> {
    assert_eq!(record.state, ReportState::Open, "only open reports end");
    let Some(item) = &record.item else {
        return Ok(false);
    };
    let settled = match store.item(item) {
        Ok(found) => found.state.is_terminal() && found.updated_at >= record.updated_at,
        Err(ArchonError::NotFound(_)) => true,
        Err(error) => return Err(error),
    };
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
    assert!(record.item.is_some(), "only an item's report ends with it");
    assert_eq!(record.state, ReportState::Open, "a report is ended once");
    tracing::info!(%report, html = %record.html.display(), "ending the finished item's Lavish session");
    let ended = lavish.end(&record.html).await;
    record_ended(ctx, report, record).await?;
    let Err(error) = ended else {
        return Ok(());
    };
    tracing::warn!(%report, %error, "lavish could not end a finished item's session");
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
