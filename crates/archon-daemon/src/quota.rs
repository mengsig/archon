//! Aether's per-provider quota headroom (its `providers` reply), read at most
//! every 30 seconds for tier-only routing and pace warnings; `status` shows
//! the last reading.

mod holds;
mod pace;
mod reroute;

pub(crate) use holds::{item_hold, spawn_hold};
pub use pace::warn_short;
pub(crate) use reroute::{
    Moved, Reserve, catalog_moves, fresh_route_moves, note_waits, notice_baseline, record_moves,
    reroute_spawns, route_replacement, switch_note,
};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::protocol::{ProviderQuotaStatus, QuotaStatus};
use archon_core::provider_route::ProviderWeights;
use archon_core::quota::pace::WindowPace;
use archon_core::quota::{ProviderHeadroom, QuotaView};
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

/// The last reading. Held only to copy it in or out.
#[derive(Debug, Default)]
pub struct QuotaCache {
    reading: std::sync::Mutex<Option<Reading>>,
    /// The ask in flight, if any, and when the pace poll last asked. Held
    /// only to join or install an ask, never across one.
    asking: std::sync::Mutex<Asking>,
    /// A background pace poll is running.
    in_flight: AtomicBool,
}

/// An ask's outcome as every caller sharing it sees it; `None` until it ends.
type Outcome = Option<Result<(), String>>;

#[derive(Debug, Default)]
struct Asking {
    /// When the pace poll last asked, answered or not.
    polled: Option<Timestamp>,
    /// The ask in flight; a caller arriving meanwhile waits for its outcome.
    current: Option<tokio::sync::watch::Receiver<Outcome>>,
}

/// Who asks Aether: routing retries a failed ask on its next pass, as it
/// always has; the pace poll asks at most every 30 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asker {
    Routing,
    PacePoll,
}

impl QuotaCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Reading>> {
        self.reading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn asking(&self) -> std::sync::MutexGuard<'_, Asking> {
        self.asking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn reading(&self) -> Option<Reading> {
        self.lock().clone()
    }

    /// Replace the reading, as a refresh or a test sets it.
    pub fn set_reading(&self, reading: Reading) {
        assert!(
            reading.providers.iter().all(|(p, _)| !p.is_empty()),
            "a reading names its providers"
        );
        *self.lock() = Some(reading);
    }
}

/// Clears [`QuotaCache::in_flight`] however the refresh ends.
struct InFlight<'a>(&'a AtomicBool);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Ask Aether for its providers' usage unless the last reading is recent.
/// An aetherd that predates headroom answers none, which routes by weight.
/// One ask runs at a time; a caller arriving while one is in flight takes
/// that ask's outcome, so a silent aetherd costs a pass at most one timeout.
pub async fn refresh(ctx: &DaemonContext, asker: Asker) -> Result<(), ArchonError> {
    let connected = ctx.aether.is_connected();
    match join_or_start(&ctx.quota, asker, connected, archon_core::now()) {
        Turn::Skip => Ok(()),
        Turn::Join(outcome) => joined(outcome).await,
        Turn::Ask(done) => {
            let mut owned = Owned {
                cache: &ctx.quota,
                done: Some(done),
            };
            let asked = ask(ctx).await;
            owned.finish(asked.as_ref().map(|_| ()).map_err(ToString::to_string));
            asked
        }
    }
}

enum Turn {
    Skip,
    Join(tokio::sync::watch::Receiver<Outcome>),
    Ask(tokio::sync::watch::Sender<Outcome>),
}

/// Under one lock: join the ask in flight, skip, or install a new one.
fn join_or_start(cache: &QuotaCache, asker: Asker, connected: bool, now: Timestamp) -> Turn {
    let mut asking = cache.asking();
    if let Some(current) = &asking.current {
        return Turn::Join(current.clone());
    }
    let recent = (cache.reading()).is_some_and(|r| now - r.read_at < REFRESH_EVERY);
    let polled_recently = (asking.polled).is_some_and(|at| now >= at && now - at < REFRESH_EVERY);
    if recent || !connected || (asker == Asker::PacePoll && polled_recently) {
        return Turn::Skip;
    }
    if asker == Asker::PacePoll {
        asking.polled = Some(now);
    }
    let (done, outcome) = tokio::sync::watch::channel(None);
    asking.current = Some(outcome);
    debug_assert!(asking.current.is_some());
    Turn::Ask(done)
}

/// The outcome of the ask `outcome` follows; one abandoned midway failed.
async fn joined(mut outcome: tokio::sync::watch::Receiver<Outcome>) -> Result<(), ArchonError> {
    let ended = match outcome.wait_for(Option::is_some).await {
        Ok(ended) => ended.clone(),
        Err(_) => Some(Err("it was abandoned".to_owned())),
    };
    match ended.expect("waited for an outcome") {
        Ok(()) => Ok(()),
        Err(why) => Err(ArchonError::Aether(format!(
            "the providers ask in flight failed: {why}"
        ))),
    }
}

/// The ask this caller installed; clears it however the caller ends, so a
/// cancelled ask never blocks the next.
struct Owned<'a> {
    cache: &'a QuotaCache,
    done: Option<tokio::sync::watch::Sender<Outcome>>,
}

impl Owned<'_> {
    fn finish(&mut self, outcome: Result<(), String>) {
        let done = self.done.take().expect("an ask finishes once");
        self.cache.asking().current = None;
        // No receiver left is fine: nobody joined.
        let _ = done.send(Some(outcome));
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        if self.done.take().is_some() {
            self.cache.asking().current = None;
        }
    }
}

/// One `providers` ask, its answer stored as the reading.
async fn ask(ctx: &DaemonContext) -> Result<(), ArchonError> {
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

/// The last reading and the moment to judge it at; `None` before a reading
/// or once it is too old to trust.
fn trusted(ctx: &DaemonContext) -> Option<(Reading, Timestamp)> {
    judged(ctx.quota.reading()?)
}

/// `reading` and the moment to judge it at, while it can be trusted.
fn judged(reading: Reading) -> Option<(Reading, Timestamp)> {
    let now = archon_core::now().max(reading.read_at);
    if now - reading.read_at > TRUSTED_FOR {
        return None;
    }
    debug_assert!(now >= reading.read_at);
    Some((reading, now))
}

/// [`refresh`] in a task of its own, one at a time, then warn of any window
/// running short, so pace stays current without provider weights and no
/// unweighted dispatch pass waits on Aether for it.
pub(crate) fn refresh_in_background(ctx: &Arc<DaemonContext>) {
    if ctx.quota.in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    let ctx = Arc::clone(ctx);
    tokio::spawn(async move {
        let _done = InFlight(&ctx.quota.in_flight);
        if let Err(error) = refresh(&ctx, Asker::PacePoll).await {
            tracing::warn!(%error, "provider quota refresh failed; using the last reading");
        }
        if let Err(error) = warn_short(&ctx).await {
            tracing::error!(%error, "quota pace warning failed; the next pass tries again");
        }
    });
}

/// The headroom routing uses now, with the configured reserves; `None`
/// before a reading or once it is too old to trust.
pub(crate) fn view(ctx: &DaemonContext) -> Option<QuotaView> {
    let (reading, now) = trusted(ctx)?;
    Some(view_of(ctx, &reading, now))
}

fn view_of(ctx: &DaemonContext, reading: &Reading, now: Timestamp) -> QuotaView {
    assert!(now >= reading.read_at, "a reading is never from the future");
    let config = &ctx.config;
    let reserves = config.models.reserves(&config.orchestrator.model.provider);
    let providers = reading.providers.iter().map(|(p, h)| (p.as_str(), h));
    QuotaView::new(providers, &reserves, reading.read_at, now)
}

/// Each provider the last trusted reading shows with no usage left at all,
/// reserves aside, and when it frees up; `None` without a trusted reading.
pub(crate) fn spent(ctx: &DaemonContext) -> Option<BTreeMap<String, Option<Timestamp>>> {
    let (reading, now) = trusted(ctx)?;
    let spent: BTreeMap<String, Option<Timestamp>> = (reading.providers.iter())
        .filter_map(|(provider, headroom)| {
            let left = archon_core::quota::provider_headroom(headroom, 0, reading.read_at, now)?;
            left.exhausted().then(|| (provider.clone(), left.resets_at))
        })
        .collect();
    debug_assert!(spent.keys().all(|p| !p.is_empty()));
    Some(spent)
}

/// Each provider the last trusted reading reports, and whether it has no
/// usage left at all; `None` without a trusted reading.
pub(crate) fn standing(ctx: &DaemonContext) -> Option<BTreeMap<String, bool>> {
    let (reading, now) = trusted(ctx)?;
    Some(
        (reading.providers.iter())
            .map(|(provider, headroom)| {
                let left = archon_core::quota::provider_headroom(headroom, 0, reading.read_at, now);
                (provider.clone(), left.is_some_and(|l| l.exhausted()))
            })
            .collect(),
    )
}

/// Each provider's window pace by the last trusted reading; empty without
/// one, so stale or missing data never warns.
pub(crate) fn paces(ctx: &DaemonContext) -> BTreeMap<String, Vec<WindowPace>> {
    let Some((reading, now)) = trusted(ctx) else {
        return BTreeMap::new();
    };
    paces_of(ctx, &reading, now)
}

fn paces_of(
    ctx: &DaemonContext,
    reading: &Reading,
    now: Timestamp,
) -> BTreeMap<String, Vec<WindowPace>> {
    assert!(now >= reading.read_at, "a reading is never from the future");
    let margin = ctx.config.models.pacing_margin_percent();
    let paces: BTreeMap<String, Vec<WindowPace>> = (reading.providers.iter())
        .map(|(provider, headroom)| {
            let pace =
                archon_core::quota::pace::provider_pace(headroom, reading.read_at, now, margin);
            (provider.clone(), pace)
        })
        .filter(|(_, pace)| !pace.is_empty())
        .collect();
    debug_assert!(paces.len() <= reading.providers.len());
    paces
}

/// [`view`] after a refresh, only where `weights` route. A failed refresh
/// keeps the last reading: quota is advice, so routing goes on by it.
pub(crate) async fn current(
    ctx: &DaemonContext,
    weights: Option<&ProviderWeights>,
) -> Option<QuotaView> {
    weights?;
    if let Err(error) = refresh(ctx, Asker::Routing).await {
        tracing::warn!(%error, "provider quota refresh failed; using the last reading");
    }
    view(ctx)
}

/// Each provider's headroom, reserve, next reset and pace for `status`, all
/// from one copy of the last reading.
pub(crate) fn status(ctx: &DaemonContext) -> Option<QuotaStatus> {
    let reading = ctx.quota.reading()?;
    let now = archon_core::now().max(reading.read_at);
    let trusted = now - reading.read_at <= TRUSTED_FOR;
    let (view, mut paces) = match trusted {
        true => (view_of(ctx, &reading, now), paces_of(ctx, &reading, now)),
        false => (QuotaView::default(), BTreeMap::new()),
    };
    let config = &ctx.config;
    let reserves = config.models.reserves(&config.orchestrator.model.provider);
    let providers: Vec<ProviderQuotaStatus> = reading
        .providers
        .iter()
        .map(|(provider, headroom)| ProviderQuotaStatus {
            provider: provider.clone(),
            headroom: view.get(provider).map(|h| h.factor),
            reserve_percent: reserves.percent(provider),
            next_reset: (headroom.windows.iter())
                .filter_map(|w| w.reset_at(reading.read_at))
                .filter(|at| *at > now)
                .min(),
            windows: headroom.windows.clone(),
            pace: paces.remove(provider).unwrap_or_default(),
        })
        .collect();
    debug_assert_eq!(providers.len(), reading.providers.len());
    Some(QuotaStatus {
        as_of: reading.read_at,
        providers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(cache: &QuotaCache) -> Turn {
        join_or_start(cache, Asker::Routing, true, archon_core::now())
    }

    #[tokio::test]
    async fn a_joiner_takes_the_failure_of_the_ask_it_joined_not_a_newer_one() {
        let cache = QuotaCache::default();
        let Turn::Ask(first) = start(&cache) else {
            panic!("an idle cache starts an ask");
        };
        let Turn::Join(paused) = start(&cache) else {
            panic!("a second caller joins the ask in flight");
        };
        let mut owner = Owned {
            cache: &cache,
            done: Some(first),
        };
        owner.finish(Err("first failed".to_owned()));
        let Turn::Ask(second) = start(&cache) else {
            panic!("after a failure the next caller asks again");
        };
        let why = joined(paused).await.expect_err("the first ask failed");
        assert!(why.to_string().contains("first failed"), "{why}");
        assert!(
            cache.asking().current.is_some(),
            "the second is still in flight"
        );
        drop(Owned {
            cache: &cache,
            done: Some(second),
        });
        assert!(
            cache.asking().current.is_none(),
            "a dropped ask clears itself"
        );
    }
}
