//! archond's side of the remote listener: a paired device's request runs
//! with the standing its role in archond's store gives (docs/remote.md). An
//! operator-role device has the operator CLI's; an observer only reads.

use std::sync::{Arc, Weak};

use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, Request, ServerFrame, WelcomeDevice, feature};
use archon_remote::{Backend, ChangeSubscription, FeedSubscription};
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
    async fn handle(&self, device: &WelcomeDevice, request: Request) -> Result<Reply, ArchonError> {
        assert!(
            archon_remote::allow::allowed(device.role, &request),
            "the listener sends only what the device's role allows"
        );
        let ctx = self.context()?;
        if request.is_read_only() {
            return crate::handlers::handle_device(&ctx, device, request).await;
        }
        let command = archon_remote::allow::command_name(&request);
        let result = crate::handlers::handle_device(&ctx, device, request).await;
        log_change(device, &command, &result);
        result
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

    async fn changes(&self, subscription: ChangeSubscription, frames: mpsc::Sender<ServerFrame>) {
        let Ok(ctx) = self.context() else {
            return;
        };
        let sub = crate::handlers::changes::ChangeSubscription {
            id: subscription.id,
            request: subscription.request,
            after: subscription.after,
        };
        crate::handlers::changes::stream(ctx, sub, frames).await;
    }
}

/// Every change a device asks for names the device and its role in the log.
fn log_change(device: &WelcomeDevice, command: &str, result: &Result<Reply, ArchonError>) {
    let role = device.role;
    match result {
        Ok(_) => {
            tracing::info!(device = %device.id, name = %device.name, ?role, command, "a paired device's request applied")
        }
        Err(error) => {
            tracing::info!(device = %device.id, name = %device.name, ?role, command, %error, "a paired device's request refused")
        }
    }
}

/// Hand the listener its backend and reopen it when it was left enabled.
pub(crate) async fn start(ctx: &Arc<DaemonContext>) {
    ctx.remote
        .attach(Arc::new(DaemonBackend(Arc::downgrade(ctx))));
    ctx.remote.restore().await;
}
