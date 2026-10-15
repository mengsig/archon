//! Initiative and stack maintenance runs independently of queue admission.
//! A pass runs when a run finishes or an initiative changes, else on a slow
//! fallback; one initiative or stack that fails never holds up the rest.

use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use super::gh::Pass;
use crate::context::DaemonContext;

#[derive(Debug, Default)]
pub struct Coordination {
    pub changed: Notify,
    pub(super) serial: Mutex<()>,
}

/// How often GitHub is asked what became of the pull requests of finished runs.
const PULL_REQUESTS_EVERY: Duration = Duration::from_secs(600);
/// The pass that runs when nothing announced a change.
const FALLBACK_EVERY: Duration = Duration::from_secs(300);
/// `kv`: `coordination_blocker:<subject>` → the JSON [`Stuck`] of its maintenance.
const BLOCKER_PREFIX: &str = "coordination_blocker:";
/// Failed passes before a failure another pass might clear is a blocker.
const TRANSIENT_PASSES: u32 = 3;

/// Why one subject's maintenance keeps failing, and whether the operator was told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stuck {
    error: String,
    failures: u32,
    told: bool,
}

pub(crate) async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut observed: Option<Instant> = None;
    loop {
        if ctx.aether.is_connected() {
            match reconcile(&ctx).await {
                Ok(()) => {}
                Err(error) if error.is_store() => return Err(error),
                Err(error) => tracing::warn!(%error, "coordination will retry"),
            }
        }
        if observed.is_none_or(|at| at.elapsed() >= PULL_REQUESTS_EVERY) {
            observed = Some(Instant::now());
            match super::pull_requests::observe(&ctx).await {
                Ok(seen) => tracing::debug!(?seen, "pull requests of finished runs observed"),
                Err(error) if error.is_store() => return Err(error),
                Err(error) => tracing::warn!(%error, "pull requests are observed again later"),
            }
        }
        tokio::select! {
            () = ctx.coordination.changed.notified() => {}
            () = tokio::time::sleep(FALLBACK_EVERY) => {}
        }
    }
}

/// A recoverable maintenance pass, also used by the deterministic harness:
/// GitHub is asked once per repository for its open pull requests.
pub async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let _serial = ctx.coordination.serial.lock().await;
    assert!(ctx.paths.data_dir.is_absolute());
    assert!(ctx.paths.worktrees.is_absolute());
    let mut pass = Pass::default();
    super::initiative_ship::reconcile(ctx, &mut pass).await?;
    super::restack::reconcile(ctx, &mut pass).await?;
    super::ship::pay_owed_merges(ctx).await?;
    super::ship::look_at_checks(ctx).await?;
    ctx.dispatch.notify_one();
    Ok(())
}

/// Record how maintaining `subject` went. A failure other than the store's
/// is its blocker, told once as `what` is stuck; the pass goes on.
pub(super) async fn isolate(
    ctx: &DaemonContext,
    subject: &str,
    item: Option<&QueueItemId>,
    what: &str,
    result: Result<(), ArchonError>,
) -> Result<(), ArchonError> {
    assert!(!subject.is_empty() && !what.is_empty(), "a named subject");
    let key = format!("{BLOCKER_PREFIX}{subject}");
    let error = match result {
        Err(error) if error.is_store() => return Err(error),
        Err(error) => error,
        Ok(()) => {
            let mut store = ctx.store.lock().await;
            if store.kv_get(&key)?.is_some() {
                store.kv_delete(&key)?;
            }
            return Ok(());
        }
    };
    let before = stuck(&*ctx.store.lock().await, &key)?;
    let failures = before.as_ref().map_or(0, |s| s.failures) + 1;
    let transient = matches!(error, ArchonError::Unavailable(_));
    let due = !transient || failures >= TRANSIENT_PASSES;
    let told = before.as_ref().is_some_and(|s| s.told);
    let now = Stuck {
        error: error.to_string(),
        failures,
        told: told || due,
    };
    tracing::warn!(%subject, %error, failures, "maintenance failed; retried next pass");
    ctx.store
        .lock()
        .await
        .kv_set(&key, &serde_json::to_string(&now)?)?;
    if told || !due {
        return Ok(());
    }
    let text = format!(
        "{what} is stuck: {error}. Archon tries again on every maintenance pass and says \
         nothing more until it passes"
    );
    super::finish::notify(ctx, NotificationKind::Blocker, &text, item, None).await
}

fn stuck(store: &archon_queue::Store, key: &str) -> Result<Option<Stuck>, ArchonError> {
    let Some(raw) = store.kv_get(key)? else {
        return Ok(None);
    };
    let found: Stuck =
        serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
    assert!(found.failures > 0, "a recorded blocker failed");
    Ok(Some(found))
}
