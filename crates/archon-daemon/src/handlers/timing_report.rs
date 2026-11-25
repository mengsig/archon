use archon_core::error::ArchonError;
use archon_core::protocol::Reply;
use archon_core::timing_report::TimingQuery;

use crate::context::DaemonContext;

pub(super) async fn report(ctx: &DaemonContext, query: &TimingQuery) -> Result<Reply, ArchonError> {
    let range = query.range(archon_core::now().date_naive())?;
    let report = ctx.read_store().await?.timing_report(&range)?;
    assert_eq!(
        report.range, range,
        "the report covers the requested UTC days"
    );
    assert!(
        report.retry_ms <= report.stage_ms,
        "retry time is part of stage time"
    );
    Ok(Reply::TimingReport(Box::new(report)))
}
