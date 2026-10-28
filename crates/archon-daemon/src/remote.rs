//! archond's side of the remote listener: requests a paired device sends
//! are handled as the operator's CLI would send them (docs/remote.md).

use std::sync::{Arc, Weak};

use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Reply, Request, ServerFrame};
use archon_remote::{Backend, FeedSubscription};
use tokio::sync::mpsc;

use crate::context::DaemonContext;

/// Weak, since the context owns the listener that holds this.
struct DaemonBackend(Weak<DaemonContext>);

impl DaemonBackend {
    fn context(&self) -> Result<Arc<DaemonContext>, ArchonError> {
        self.0
            .upgrade()
            .ok_or_else(|| ArchonError::Unavailable("archond is stopping".into()))
    }
}

#[async_trait::async_trait]
impl Backend for DaemonBackend {
    async fn handle(&self, request: Request) -> Result<Reply, ArchonError> {
        assert!(
            archon_remote::allow::allowed(&request),
            "the listener sends only allowed requests"
        );
        let ctx = self.context()?;
        crate::handlers::handle(&ctx, ClientKind::ArchonCli, request).await
    }

    async fn feed(&self, subscription: FeedSubscription, frames: mpsc::Sender<ServerFrame>) {
        let Ok(ctx) = self.context() else {
            return;
        };
        crate::server::feed_frames(ctx, subscription, frames).await;
    }
}

/// Hand the listener its backend and reopen it when it was left enabled.
pub(crate) async fn start(ctx: &Arc<DaemonContext>) {
    ctx.remote
        .attach(Arc::new(DaemonBackend(Arc::downgrade(ctx))));
    ctx.remote.restore().await;
}
