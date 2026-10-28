//! Reviving a stopped orchestrator with backoff (ARCHITECTURE §16). The
//! first revival of a failure streak runs at once, each later one waits
//! longer, and a turn that ends well closes the streak. Revival never stops
//! for good, so a transient engine error recovers on its own.

use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_queue::store::Store;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;
use crate::feed;

/// The pause after each revival of a streak; the last one repeats.
pub const REVIVAL_BACKOFF: [Duration; 5] = [
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
];
/// Revivals in one streak after which the operator gets a `Blocker`.
pub const REVIVALS_BEFORE_BLOCKER: u32 = 3;
/// `kv`: the [`Backoff`], so an `archond` restart keeps the streak's pace.
const BACKOFF_KEY: &str = "orchestrator_backoff";
/// `kv` keys of the give-up rule this backoff replaced; nothing reads them.
const RETIRED_KEYS: [&str; 2] = ["orchestrator_gave_up", "orchestrator_failures"];
/// Turn endings (`agent.turn_completed.next`) that prove the agent works.
const HEALTHY_TURN_ENDS: &[&str] = &["ready", "waiting"];
/// How long the blocker waits for Aether to say whether the engine is logged in.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

/// The orchestrator's current failure streak.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    /// Revivals since the orchestrator last ended a turn well.
    pub revivals: u32,
    /// When the latest of them ran.
    pub last: Option<Timestamp>,
}

impl Backoff {
    /// When the next revival may run, if that is still after `now`. A
    /// `last` in the future (the clock went back) counts as `now`.
    pub fn deferred_until(&self, now: Timestamp) -> Option<Timestamp> {
        let last = self.last?.min(now);
        assert!(self.revivals > 0, "a revival time implies a revival");
        let pause = chrono::TimeDelta::from_std(pause_after(self.revivals))
            .expect("backoff pauses fit a TimeDelta");
        let due = last + pause;
        (due > now).then_some(due)
    }

    /// The streak after one more revival at `now`.
    pub fn revived(self, now: Timestamp) -> Self {
        Self {
            revivals: self.revivals.saturating_add(1),
            last: Some(now),
        }
    }
}

/// The pause after the `revivals`-th revival of a streak.
pub fn pause_after(revivals: u32) -> Duration {
    assert!(revivals > 0, "no pause is owed before the first revival");
    let last = REVIVAL_BACKOFF.len() - 1;
    let index = usize::try_from(revivals - 1).map_or(last, |i| i.min(last));
    REVIVAL_BACKOFF[index]
}

pub(super) async fn load(ctx: &DaemonContext) -> Result<Backoff, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(BACKOFF_KEY)?;
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return Ok(Backoff::default());
    };
    let backoff: Backoff = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {BACKOFF_KEY} = {raw:?}: {e}")))?;
    if backoff.last.is_some() != (backoff.revivals > 0) {
        return Err(ArchonError::Corrupt(format!(
            "kv {BACKOFF_KEY} = {raw:?}: revivals and their time disagree"
        )));
    }
    Ok(backoff)
}

pub(super) async fn save(ctx: &DaemonContext, backoff: &Backoff) -> Result<(), ArchonError> {
    let raw = serde_json::to_string(backoff)?;
    ctx.store.lock().await.kv_set(BACKOFF_KEY, &raw)
}

/// Start a fresh streak: the operator restarted the orchestrator, or it
/// ended a turn well.
pub(super) async fn reset(ctx: &DaemonContext) -> Result<(), ArchonError> {
    save(ctx, &Backoff::default()).await?;
    ctx.supervisor.revive_at.send_replace(None);
    Ok(())
}

/// Delete what the retired give-up rule left in `kv` (ARCHITECTURE §16).
/// Returns how many keys were there.
pub(crate) fn prune_retired_keys(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for key in RETIRED_KEYS {
        assert_ne!(key, BACKOFF_KEY, "the live streak is never retired");
        if store.kv_get(key)?.is_some() {
            store.kv_delete(key)?;
            pruned += 1;
        }
    }
    for prefix in ["host_context:", "agent_host_context:"] {
        for (key, _) in store.kv_prefix(prefix)? {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Ask [`revive_when_due`] to try again at `due`.
pub(super) fn schedule(ctx: &DaemonContext, due: Timestamp) {
    let longest = REVIVAL_BACKOFF[REVIVAL_BACKOFF.len() - 1];
    let ahead = (due - archon_core::now())
        .to_std()
        .unwrap_or(Duration::ZERO);
    assert!(
        ahead <= longest,
        "a revival is never owed later than {longest:?} ahead"
    );
    ctx.supervisor.revive_at.send_replace(Some(due));
}

/// Why a stopped orchestrator is not revived yet. The wait is relative, so
/// it reads right in any timezone; a revival already due is running now.
pub(super) fn deferred_error(state: &str, due: Timestamp, now: Timestamp) -> ArchonError {
    assert!(!state.is_empty(), "a stopped orchestrator has a state");
    let Ok(wait) = (due - now).to_std() else {
        return reviving_now(state);
    };
    if wait.is_zero() {
        return reviving_now(state);
    }
    let secs = wait.as_millis().div_ceil(1000);
    assert!(
        wait <= REVIVAL_BACKOFF[REVIVAL_BACKOFF.len() - 1],
        "a revival is never owed later than the longest pause"
    );
    ArchonError::Unavailable(format!(
        "the orchestrator is {state}; Archon revives it again in {secs}s, or run `archon orchestrator restart` now"
    ))
}

fn reviving_now(state: &str) -> ArchonError {
    ArchonError::Unavailable(format!(
        "the orchestrator is {state}; Archon is reviving it now, and `archon orchestrator` shows how it goes"
    ))
}

/// The one `Blocker` of a streak that keeps failing. When `provider`, the
/// failed agent's, is not logged in, it names the login step, not a restart.
pub(super) async fn keeps_failing(
    ctx: &DaemonContext,
    backoff: &Backoff,
    provider: &str,
    error: Option<&str>,
) -> Result<(), ArchonError> {
    assert_eq!(
        backoff.revivals, REVIVALS_BEFORE_BLOCKER,
        "told once per streak"
    );
    assert!(!provider.is_empty(), "an agent's model names its provider");
    let cause = error
        .map(|e| format!(" (last error: {e})"))
        .unwrap_or_default();
    let most = REVIVAL_BACKOFF[REVIVAL_BACKOFF.len() - 1].as_secs() / 60;
    let fix = match login_problem(ctx, provider).await {
        Some(problem) => format!("{problem}; the next revival after that brings it back."),
        None => "Run `archon orchestrator restart` to start a fresh one now.".to_owned(),
    };
    let text = format!(
        "The orchestrator stopped {} times in a row{cause}. Archon keeps reviving it, waiting longer each time and then every {most} minutes. Its news is held until it recovers; item outcomes still show here. {fix}",
        backoff.revivals
    );
    feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    Ok(())
}

/// Why `provider` is not logged in, as Aether's `auth_status` says; `None`
/// when it is, or when Aether cannot say (logged, and the restart hint stays).
async fn login_problem(ctx: &DaemonContext, provider: &str) -> Option<String> {
    let lookup = async { ctx.aether.client()?.auth_status().await };
    let auth = match tokio::time::timeout(AUTH_TIMEOUT, lookup).await {
        Ok(Ok(auth)) => auth,
        Ok(Err(error)) => {
            tracing::warn!(%error, "auth_status failed; the blocker suggests a restart");
            return None;
        }
        Err(_) => {
            tracing::warn!("auth_status timed out; the blocker suggests a restart");
            return None;
        }
    };
    archon_aether::auth::login_problem(provider, &auth)
}

/// A revival Aether answered with an error, returned for the caller to report.
/// One that says Aether itself is in trouble counts in the paced streak.
pub(super) async fn failed(
    ctx: &DaemonContext,
    backoff: Backoff,
    now: Timestamp,
    provider: &str,
    error: ArchonError,
) -> Result<ArchonError, ArchonError> {
    if !matches!(error, ArchonError::Aether(_) | ArchonError::Protocol(_)) {
        return Ok(error);
    }
    let backoff = backoff.revived(now);
    save(ctx, &backoff).await?;
    let due = backoff
        .deferred_until(now)
        .expect("a revival that just ran is followed by a pause");
    schedule(ctx, due);
    if backoff.revivals == REVIVALS_BEFORE_BLOCKER {
        keeps_failing(ctx, &backoff, provider, Some(&error.to_string())).await?;
    }
    Ok(error)
}

/// An orchestrator turn ended with Aether status `next`. One that ended
/// well closes the failure streak.
pub async fn turn_ended(ctx: &DaemonContext, next: Option<&str>) -> Result<(), ArchonError> {
    if !next.is_some_and(|next| HEALTHY_TURN_ENDS.contains(&next)) {
        return Ok(());
    }
    let backoff = load(ctx).await?;
    if backoff.revivals == 0 {
        return Ok(());
    }
    reset(ctx).await?;
    if backoff.revivals >= REVIVALS_BEFORE_BLOCKER {
        feed::system(ctx, "The orchestrator is working again.").await?;
    }
    Ok(())
}

/// Runs each revival `ensure_orchestrator` deferred, once it is due. Only a
/// store failure ends it; any other failure is left to the next trigger.
pub async fn revive_when_due(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut scheduled = ctx.supervisor.revive_at.subscribe();
    loop {
        let Some(due) = *scheduled.borrow_and_update() else {
            // The sender lives in `ctx`, so `changed` cannot fail here.
            let _ = scheduled.changed().await;
            continue;
        };
        let wait = (due - archon_core::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = scheduled.changed() => continue,
        }
        ctx.supervisor
            .revive_at
            .send_if_modified(|at| at.take_if(|at| *at == due).is_some());
        match super::ensure_orchestrator(&ctx).await {
            Ok(id) => tracing::info!(%id, "a deferred orchestrator revival ran"),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::warn!(%error, "deferred orchestrator revival failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn pauses_grow_then_repeat_every_ten_minutes() {
        let pauses: Vec<u64> = (1..=7).map(|n| pause_after(n).as_secs()).collect();
        assert_eq!(pauses, [10, 30, 120, 300, 600, 600, 600]);
        assert_eq!(pause_after(u32::MAX), Duration::from_secs(600));
    }

    #[test]
    fn the_first_revival_of_a_streak_is_never_deferred() {
        assert_eq!(Backoff::default().deferred_until(at(0)), None);
        let once = Backoff::default().revived(at(0));
        assert_eq!(once.deferred_until(at(9)), Some(at(10)));
        assert_eq!(once.deferred_until(at(10)), None, "due exactly on time");
        let twice = once.revived(at(10));
        assert_eq!(twice.revivals, 2);
        assert_eq!(twice.deferred_until(at(11)), Some(at(40)));
    }

    #[test]
    fn a_deferral_counts_down_and_a_due_one_is_reviving_now() {
        let waiting = deferred_error("failed", at(10), at(0)).to_string();
        assert!(
            waiting.contains("Archon revives it again in 10s"),
            "{waiting}"
        );
        let rounded = deferred_error("failed", at(10), at(9) - chrono::TimeDelta::milliseconds(1));
        assert!(rounded.to_string().contains("again in 2s"), "{rounded}");
        for now in [at(10), at(70)] {
            let due = deferred_error("failed", at(10), now).to_string();
            assert!(due.contains("Archon is reviving it now"), "{due}");
            assert!(!due.contains("again in"), "{due}");
        }
    }

    #[test]
    fn a_clock_that_went_back_does_not_stretch_the_pause() {
        let revived = Backoff::default().revived(at(3_600));
        assert_eq!(revived.deferred_until(at(0)), Some(at(10)));
    }
}
