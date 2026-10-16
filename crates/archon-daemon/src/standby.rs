//! Standby after the store hit a full disk (ARCHITECTURE §16): the work
//! stops, as in-memory state may no longer match a store whose write
//! failed, but the socket keeps serving reads and `archon gc` (which
//! writes nothing to the store), so `archon status` and `archon gc` still
//! work and clients do not restart archond in a loop.
//! Once disk space is back archond exits, and its next start recovers.

use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::protocol::Request;
use tokio::task::JoinHandle;

use crate::context::DaemonContext;
use crate::disk::FullDisk;
use crate::dispatcher;

/// How often standby reads the free space.
const POLL: Duration = Duration::from_secs(30);

/// The server task; it ends on `Shutdown`.
pub(crate) type Server = JoinHandle<Result<(), ArchonError>>;

/// Mark the daemon in standby for `error`, a store write on a full disk.
pub(crate) fn enter(ctx: &DaemonContext, error: &ArchonError) {
    assert!(
        matches!(error, ArchonError::DiskFull(_)),
        "standby is for a full disk only"
    );
    let why = error.to_string();
    tracing::error!(%why, "the store's disk is full; archond serves reads only until space is free");
    ctx.disk.report_full(&why, FullDisk::Archon, Instant::now());
    if ctx.standby.set(why).is_err() {
        tracing::warn!("standby entered twice; the first cause is kept");
    }
    assert!(ctx.disk.holding(), "standby holds dispatch");
}

/// Serve reads until `Shutdown`, a stop signal, or free disk space.
pub(crate) async fn serve(
    ctx: &DaemonContext,
    mut server: Server,
    stop: impl Future<Output = Result<(), ArchonError>>,
) -> Result<(), ArchonError> {
    assert!(ctx.standby.get().is_some(), "entered before it is served");
    let result = tokio::select! {
        stopped = stop => stopped,
        served = &mut server => crate::server_ended(served),
        () = space_back(ctx) => {
            tracing::info!("disk space is back; archond exits so its next start recovers");
            Ok(())
        }
    };
    server.abort();
    result
}

/// Wait until the disk hold a full store engaged ends.
async fn space_back(ctx: &DaemonContext) {
    let min_free = ctx.config.limits.min_free_bytes();
    loop {
        tokio::time::sleep(POLL).await;
        match dispatcher::read_disk(ctx).await {
            Ok(reading) => {
                if ctx.disk.update(reading, min_free, Instant::now()).is_none() {
                    return;
                }
            }
            Err(error) => tracing::warn!(%error, "free disk space unknown; standby goes on"),
        }
    }
}

/// The refusal of `request` in standby, which serves reads and `Shutdown`.
pub(crate) fn refusal(ctx: &DaemonContext, request: &Request) -> Option<ArchonError> {
    let why = ctx.standby.get()?;
    if allows(request) {
        return None;
    }
    let command = crate::handlers::command_name(request);
    assert!(!why.is_empty(), "standby names its cause");
    Some(ArchonError::DiskFull(format!(
        "archond is in standby and refuses {command} ({why}); once disk space is \
         free (`archon gc`) it exits, and the next command restarts it"
    )))
}

fn allows(request: &Request) -> bool {
    matches!(
        request,
        Request::Shutdown
            | Request::Status(_)
            | Request::ListItems(_)
            | Request::ListItemSummaries(_)
            | Request::GetItem { .. }
            | Request::ResolveItem { .. }
            | Request::RepoList
            | Request::ListPipelines { .. }
            | Request::ModelPolicy { .. }
            | Request::OrchestratorStatus
            | Request::ListQuestions { .. }
            | Request::FeedHistory { .. }
            | Request::ListReports
            | Request::Autonomy { .. }
            | Request::GithubWatcher
            | Request::Gc { .. }
            | Request::CostReport(_)
            | Request::ModelEvidence(_)
    )
}
