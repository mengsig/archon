//! Initiative and stack maintenance runs independently of queue admission.

use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use tokio::sync::{Mutex, Notify};

use crate::context::DaemonContext;

#[derive(Debug, Default)]
pub struct Coordination {
    pub changed: Notify,
    pub(super) serial: Mutex<()>,
}

pub(crate) async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        if ctx.aether.is_connected() {
            match reconcile(&ctx).await {
                Ok(()) => {}
                Err(error) if error.is_store() => return Err(error),
                Err(error) => tracing::warn!(%error, "coordination will retry"),
            }
        }
        tokio::select! {
            () = ctx.coordination.changed.notified() => {}
            () = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
    }
}

/// A recoverable maintenance pass, also used by the deterministic harness.
pub async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let _serial = ctx.coordination.serial.lock().await;
    assert!(ctx.paths.data_dir.is_absolute());
    assert!(ctx.paths.worktrees.is_absolute());
    super::initiative_ship::reconcile(ctx).await?;
    super::restack::reconcile(ctx).await?;
    ctx.dispatch.notify_one();
    Ok(())
}
