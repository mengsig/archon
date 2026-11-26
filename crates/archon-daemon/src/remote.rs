//! archond's side of the remote listener: a paired device's request runs
//! with the standing its role in archond's store gives (docs/remote.md). An
//! operator-role device has the operator CLI's; an observer only reads.

use std::sync::{Arc, Weak};

use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, Request, ServerFrame, WelcomeDevice, feature};
use archon_remote::{Backend, ChangeSubscription, DesignSubscription, FeedSubscription};
use tokio::sync::{mpsc, watch};

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

    fn refused(&self, request: &Request) {
        // A refused input batch may have carried a release: it must not stay held.
        if let Request::DesignInput { session, .. } = request
            && let Ok(ctx) = self.context()
        {
            ctx.design.input_lost(session);
        }
    }

    fn admitted(&self, request: &Request) -> Option<Box<dyn Send>> {
        // Input read before a stream ends may still press: the stream's end waits for it.
        let Request::DesignInput { session, .. } = request else {
            return None;
        };
        let admitted = self.context().ok()?.design.input_admitted(session)?;
        Some(Box::new(admitted))
    }

    fn features(&self) -> Vec<&'static str> {
        let mut features = crate::handlers::features();
        // Advertised only once every request it covers is served.
        if self.context().is_ok_and(|ctx| ctx.design.enabled()) {
            features.push(feature::DESIGN_SESSIONS);
            features.push(feature::DESIGN_ITERATIONS);
            features.push(feature::DESIGN_UNSUBSCRIBE);
        }
        features
    }

    async fn design(
        &self,
        subscription: DesignSubscription,
        acked: watch::Receiver<u64>,
        frames: mpsc::Sender<ServerFrame>,
    ) {
        let Ok(ctx) = self.context() else {
            return;
        };
        tracing::info!(device = %subscription.device.id, name = %subscription.device.name, role = ?subscription.device.role, session = %subscription.session, "design: stream opened");
        crate::design::stream(ctx, subscription, acked, frames).await;
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

/// Requests a live preview sends many times a second; logging each at INFO
/// would drown the log.
fn is_high_rate(command: &str) -> bool {
    matches!(command, "design_input" | "design_resolve" | "design_region")
}

/// Every change a device asks for names the device and its role in the log.
fn log_change(device: &WelcomeDevice, command: &str, result: &Result<Reply, ArchonError>) {
    let role = device.role;
    let quiet = is_high_rate(command);
    match result {
        Ok(_) if quiet => {
            tracing::debug!(device = %device.id, name = %device.name, ?role, command, "a paired device's request applied")
        }
        Ok(_) => {
            tracing::info!(device = %device.id, name = %device.name, ?role, command, "a paired device's request applied")
        }
        Err(error) if quiet => {
            tracing::debug!(device = %device.id, name = %device.name, ?role, command, %error, "a paired device's request refused")
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

#[cfg(test)]
mod tests {
    use super::is_high_rate;

    #[test]
    fn per_frame_requests_log_quietly() {
        for c in ["design_input", "design_resolve", "design_region"] {
            assert!(is_high_rate(c), "{c}");
        }
        for c in [
            "design_note_add",
            "design_finish_confirm",
            "remote_pair",
            "say",
        ] {
            assert!(!is_high_rate(c), "{c}");
        }
    }
}
