//! Aether's per-provider quota headroom (its `providers` reply), read at
//! most every 30 seconds and kept for tier-only routing, the
//! dispatcher's quota holds and `status`, which never asks Aether itself.

mod holds;
mod reroute;

pub(crate) use holds::refresh_holds;
pub(crate) use reroute::{reroute_spawns, switch_note};

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{ProviderQuotaStatus, QuotaStatus};
use archon_core::provider_route::ProviderWeights;
use archon_core::quota::{ProviderHeadroom, QuotaHold, QuotaView};
use chrono::TimeDelta;

use crate::context::DaemonContext;

/// How long a reading stands before routing asks Aether again.
const REFRESH_EVERY: TimeDelta = TimeDelta::seconds(30);

/// A reading older than this (Aether gone, say) says nothing about quota.
const TRUSTED_FOR: TimeDelta = TimeDelta::minutes(10);

/// The providers that reported usage, as aetherd answered at `read_at`.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub read_at: Timestamp,
    pub providers: Vec<(String, ProviderHeadroom)>,
}

#[derive(Debug, Default)]
struct Cached {
    reading: Option<Reading>,
    /// Queued items the last dispatch pass found no provider with quota for.
    holds: BTreeMap<QueueItemId, QuotaHold>,
}

/// The last reading and the holds drawn from it. Held only to copy in or out.
#[derive(Debug, Default)]
pub struct QuotaCache {
    cached: std::sync::Mutex<Cached>,
}

impl QuotaCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn reading(&self) -> Option<Reading> {
        self.lock().reading.clone()
    }

    /// Replace the reading, as a refresh or a test sets it.
    pub fn set_reading(&self, reading: Reading) {
        assert!(
            reading.providers.iter().all(|(p, _)| !p.is_empty()),
            "a reading names its providers"
        );
        self.lock().reading = Some(reading);
    }

    pub fn hold(&self, item: &QueueItemId) -> Option<QuotaHold> {
        self.lock().holds.get(item).cloned()
    }

    fn set_holds(&self, holds: BTreeMap<QueueItemId, QuotaHold>) {
        self.lock().holds = holds;
    }
}

/// Ask Aether for its providers' usage unless the last reading is recent.
/// An aetherd that predates headroom answers none, which routes by weight.
pub(crate) async fn refresh(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let now = archon_core::now();
    let recent = ctx
        .quota
        .reading()
        .is_some_and(|r| now - r.read_at < REFRESH_EVERY);
    if recent || !ctx.aether.is_connected() {
        return Ok(());
    }
    let providers = ctx.aether.client()?.providers().await?;
    let reported: Vec<(String, ProviderHeadroom)> = providers
        .into_iter()
        .filter_map(|info| Some((info.provider, info.headroom?)))
        .collect();
    debug_assert!(reported.iter().all(|(p, _)| !p.is_empty()));
    ctx.quota.set_reading(Reading {
        read_at: archon_core::now(),
        providers: reported,
    });
    Ok(())
}

/// The headroom routing uses now: the last reading, with the configured
/// reserves; `None` before a reading or once it is too old to trust.
pub(crate) fn view(ctx: &DaemonContext) -> Option<QuotaView> {
    let reading = ctx.quota.reading()?;
    let now = archon_core::now().max(reading.read_at);
    if now - reading.read_at > TRUSTED_FOR {
        return None;
    }
    let config = &ctx.config;
    let reserves = config.models.reserves(&config.orchestrator.model.provider);
    let providers = reading.providers.iter().map(|(p, h)| (p.as_str(), h));
    Some(QuotaView::new(providers, &reserves, reading.read_at, now))
}

/// [`view`] after a refresh, for routing by `weights`; `None` without them,
/// so nothing asks Aether where weights do not route. A failed refresh
/// keeps the last reading: quota is advice, so routing goes on by what it has.
pub(crate) async fn current(
    ctx: &DaemonContext,
    weights: Option<&ProviderWeights>,
) -> Option<QuotaView> {
    weights?;
    if let Err(error) = refresh(ctx).await {
        tracing::warn!(%error, "provider quota refresh failed; using the last reading");
    }
    view(ctx)
}

/// Whether a dispatch pass reads quota: global weights route, or a
/// repository's routing already read it.
pub(crate) fn wanted(ctx: &DaemonContext) -> bool {
    !ctx.config.models.provider_weights.is_empty() || ctx.quota.reading().is_some()
}

/// Each provider's headroom, reserve and next reset for `status`, from the
/// last reading alone.
pub(crate) fn status(ctx: &DaemonContext) -> Option<QuotaStatus> {
    let reading = ctx.quota.reading()?;
    let view = view(ctx).unwrap_or_default();
    let now = archon_core::now();
    let config = &ctx.config;
    let reserves = config.models.reserves(&config.orchestrator.model.provider);
    let providers: Vec<ProviderQuotaStatus> = reading
        .providers
        .iter()
        .map(|(provider, headroom)| ProviderQuotaStatus {
            provider: provider.clone(),
            headroom: view.get(provider).map(|h| h.factor),
            reserve_percent: reserves.percent(provider),
            next_reset: headroom
                .windows
                .iter()
                .filter_map(|w| w.resets_at)
                .filter(|at| *at > now)
                .min(),
            windows: headroom.windows.clone(),
        })
        .collect();
    debug_assert_eq!(providers.len(), reading.providers.len());
    Some(QuotaStatus {
        as_of: reading.read_at,
        providers,
    })
}
