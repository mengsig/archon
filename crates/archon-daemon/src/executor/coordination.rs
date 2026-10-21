//! Initiative and stack maintenance runs independently of queue admission.
//! A pass runs when a run finishes or an initiative changes, else on a slow
//! fallback; one initiative or stack that fails never holds up the rest.

use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use super::gh::Pass;
use crate::context::DaemonContext;

#[derive(Debug, Default)]
pub struct Coordination {
    pub changed: Notify,
    pub(super) serial: Mutex<()>,
    /// Held while a delivery pushes into an existing pull request.
    pub(super) onto: super::ship::OntoLocks,
    /// Held while a stage run is delivered what its steers owe it.
    pub(super) steering: super::steer::Locks,
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
            match isolated_pass(&ctx).await {
                Ok(()) => {}
                Err(error) if error.is_store() => return Err(error),
                Err(error) => tracing::warn!(%error, "coordination will retry"),
            }
        }
        if observed.is_none_or(|at| at.elapsed() >= PULL_REQUESTS_EVERY) {
            observed = Some(Instant::now());
            let seen = guarded(
                "observing pull requests",
                super::pull_requests::observe(&ctx),
            );
            match seen.await {
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

/// One [`reconcile`] pass in its own task, so a panic on one record's data
/// is logged and retried on the next pass instead of stopping the daemon.
async fn isolated_pass(ctx: &Arc<DaemonContext>) -> Result<(), ArchonError> {
    // A JoinSet aborts its task when dropped, so stopping the worker stops the pass.
    let mut pass = tokio::task::JoinSet::new();
    pass.spawn({
        let ctx = ctx.clone();
        async move { reconcile(&ctx).await }
    });
    match pass.join_next().await {
        Some(Ok(result)) => result,
        Some(Err(error)) => Err(ArchonError::Protocol(format!(
            "a coordination pass panicked or was cancelled: {error}"
        ))),
        None => unreachable!("one task was spawned"),
    }
}

/// A recoverable maintenance pass, also used by the deterministic harness:
/// GitHub is asked once per repository for its open pull requests.
pub async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    // Before the serial lock: an answer holds `answering` while its shipping waits for `serial`.
    Box::pin(super::gates::close_settled_questions(ctx)).await?;
    let _serial = ctx.coordination.serial.lock().await;
    assert!(ctx.paths.data_dir.is_absolute());
    assert!(ctx.paths.worktrees.is_absolute());
    let mut pass = Pass::default();
    super::initiative_ship::reconcile(ctx, &mut pass).await?;
    super::restack::reconcile(ctx, &mut pass).await?;
    super::ship::pay_owed_merges(ctx).await?;
    super::ship::look_at_checks(ctx).await?;
    super::operator_merge::settle_stale(ctx).await?;
    ctx.dispatch.notify_one();
    Ok(())
}

/// Run `work` on `subject`'s data so that a panic on it is that subject's
/// failure, logged with its name and returned as an error, not the daemon's.
pub(crate) async fn guarded<T>(
    subject: &str,
    work: impl std::future::Future<Output = Result<T, ArchonError>>,
) -> Result<T, ArchonError> {
    assert!(!subject.is_empty(), "a named subject");
    match std::panic::AssertUnwindSafe(Box::pin(work))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(panic) => {
            let said = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            tracing::error!(%subject, panic = %said, "maintenance panicked; only this subject is affected");
            Err(ArchonError::Protocol(format!(
                "{subject} hit a bug on its stored data ({said}); other work goes on. \
                 Run `archon doctor` and report its output"
            )))
        }
    }
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
            set_aside(ctx, subject, item, None);
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
    if due {
        set_aside(ctx, subject, item, Some(&error));
    }
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

/// Keep `subject` (`kind:id`) in the quarantine list while its maintenance
/// fails, with the commands that fix or cancel it; drop it once it passes.
fn set_aside(
    ctx: &DaemonContext,
    subject: &str,
    item: Option<&QueueItemId>,
    failed: Option<&ArchonError>,
) {
    let (kind, id) = subject.split_once(':').unwrap_or(("record", subject));
    let data_dir = &ctx.paths.data_dir;
    let outcome = match failed {
        None => archon_core::quarantine::clear(data_dir, kind, id),
        Some(error) => {
            let (fix, cancel) = match kind {
                "initiative" => (
                    format!("archon initiatives retry {id}"),
                    format!("archon initiatives cancel {id}"),
                ),
                _ => {
                    let item = item.map_or_else(|| id.to_owned(), ToString::to_string);
                    (
                        format!("archon queue retry {item}"),
                        format!("archon queue cancel {item}"),
                    )
                }
            };
            let record = archon_core::quarantine::Quarantined::new(
                kind,
                id,
                &error.to_string(),
                &fix,
                &cancel,
            );
            archon_core::quarantine::write(data_dir, &record)
        }
    };
    if let Err(error) = outcome {
        tracing::error!(%subject, %error, "the quarantine list was not updated");
    }
}

fn stuck(store: &archon_queue::Store, key: &str) -> Result<Option<Stuck>, ArchonError> {
    let Some(raw) = store.kv_get(key)? else {
        return Ok(None);
    };
    match serde_json::from_str::<Stuck>(&raw) {
        Ok(found) if found.failures > 0 => Ok(Some(found)),
        _ => {
            tracing::warn!(key, "unreadable maintenance record; starting it over");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_blocker_record_starts_over_instead_of_failing_the_pass() {
        let mut store = archon_queue::Store::open_in_memory().unwrap();
        store.kv_set("k", "{").unwrap();
        assert!(stuck(&store, "k").unwrap().is_none());
        store
            .kv_set("k", r#"{"error":"e","failures":0,"told":false}"#)
            .unwrap();
        assert!(stuck(&store, "k").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_panic_on_one_subject_is_its_error_and_the_rest_goes_on() {
        let broken = guarded("initiative:ini_x", async {
            if true {
                panic!("bad stored data");
            }
            Ok(())
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(broken.contains("initiative:ini_x") && broken.contains("bad stored data"));
        let fine = guarded("initiative:ini_y", async { Ok(7) }).await.unwrap();
        assert_eq!(fine, 7);
    }
}
