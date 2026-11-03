//! archond's side of the remote listener: requests a paired device's role
//! allows run with `archon-web` standing (docs/remote.md), so a handler can
//! refuse what only the operator's CLI may do.

use std::sync::{Arc, Weak};

use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, DeviceRole, Reply, Request, ServerFrame, feature};
use archon_remote::{Backend, FeedSubscription};
use tokio::sync::mpsc;

use crate::context::DaemonContext;

/// The protocol 24 features remote access serves.
pub(crate) const FEATURES: &[&str] = &[feature::DEVICE_ROLES];

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
    async fn handle(&self, role: DeviceRole, request: Request) -> Result<Reply, ArchonError> {
        assert!(
            archon_remote::allow::allowed(role, &request),
            "the listener sends only what the device's role allows"
        );
        assert!(
            !matches!(request, Request::ForAgent { .. } | Request::Tracked { .. }),
            "a device's request is its own, never forwarded"
        );
        let ctx = self.context()?;
        crate::handlers::handle(&ctx, ClientKind::ArchonWeb, request).await
    }

    fn features(&self) -> Vec<&'static str> {
        crate::handlers::features()
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
