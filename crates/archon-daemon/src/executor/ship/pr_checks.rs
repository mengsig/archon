//! The checks of a delivered pull request, watched as an owed look
//! (ARCHITECTURE §8.7). Archon tells the orchestrator and the operator once
//! when they fail, and separately when GitHub never started them: that is the
//! operator's to fix (billing, runners), not the change's. Checks that pass,
//! or a look GitHub keeps refusing, end the watch quietly.

use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::notify::NotificationKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::finish;
use super::super::gh::checks::{self, Checks};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// `kv`: `ship_checks:<run>` → the JSON [`Watch`] of its pull request's checks.
const WATCH_PREFIX: &str = "ship_checks:";
/// The first look waits this long after delivery: checks take a while to appear.
const FIRST_LOOK: Duration = Duration::from_secs(120);
/// Between looks while checks are pending or GitHub did not answer.
const LOOK_EVERY: Duration = Duration::from_secs(600);
/// A check still queued this long after delivery never started.
const QUEUED_LIMIT: Duration = Duration::from_secs(1_800);
/// A pull request with no checks this long after delivery has none to watch.
const NO_CHECKS_LIMIT: Duration = Duration::from_secs(1_800);
/// The watch ends, logged, this long after delivery.
const GIVE_UP: Duration = Duration::from_secs(24 * 3_600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Watch {
    item: QueueItemId,
    repo: RepoId,
    url: String,
    delivered_at: DateTime<Utc>,
    next_at: DateTime<Utc>,
}

fn key(run: &PipelineRunId) -> String {
    format!("{WATCH_PREFIX}{run}")
}

fn after(at: DateTime<Utc>, wait: Duration) -> DateTime<Utc> {
    let wait = chrono::Duration::from_std(wait).expect("a day fits");
    at.checked_add_signed(wait)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// Watch the checks of `url`, just delivered for `run`; a later delivery
/// into the same pull request replaces an earlier run's watch.
pub(super) fn owe(
    store: &mut archon_queue::Store,
    run: &PipelineRunId,
    item: &QueueItemId,
    repo: &RepoId,
    url: &str,
) -> Result<(), ArchonError> {
    assert!(url.starts_with("https://"), "a pull request GitHub listed");
    for (earlier, raw) in store.kv_prefix(WATCH_PREFIX)? {
        // A malformed watch is left for `look` to set aside.
        if parse(&earlier, &raw).is_ok_and(|watch| watch.url == url) {
            store.kv_delete(&earlier)?;
        }
    }
    let now = archon_core::now();
    let watch = Watch {
        item: item.clone(),
        repo: repo.clone(),
        url: url.to_owned(),
        delivered_at: now,
        next_at: after(now, FIRST_LOOK),
    };
    store.kv_set(&key(run), &serde_json::to_string(&watch)?)
}

fn parse(key: &str, raw: &str) -> Result<Watch, ArchonError> {
    serde_json::from_str(raw).map_err(|e| ArchonError::Corrupt(format!("checks watch {key}: {e}")))
}

/// Look once at every watch that is due; returns how many ended. An unusable
/// watch is removed and a failing one logged, so the others are still looked at.
pub(crate) async fn look(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let watched = ctx.store.lock().await.kv_prefix(WATCH_PREFIX)?;
    let mut ended = 0;
    for (raw_key, raw) in watched {
        let decoded = decode(&raw_key, &raw);
        let (run, watch) = match decoded {
            Ok(found) => found,
            Err(error) => {
                reject(ctx, &raw_key, &raw, &error).await?;
                continue;
            }
        };
        if watch.next_at > archon_core::now() {
            continue;
        }
        let subject = format!("checks watch {run}");
        match super::super::coordination::guarded(&subject, look_once(ctx, &run, watch)).await {
            Ok(true) => ended += 1,
            Ok(false) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::error!(%run, %error, "a checks watch failed; the rest go on"),
        }
    }
    Ok(ended)
}

fn decode(raw_key: &str, raw: &str) -> Result<(PipelineRunId, Watch), ArchonError> {
    let run = raw_key
        .strip_prefix(WATCH_PREFIX)
        .ok_or_else(|| ArchonError::Corrupt(format!("checks watch {raw_key}: foreign key")))?;
    let run = PipelineRunId::parse(run)
        .map_err(|e| ArchonError::Corrupt(format!("checks watch {raw_key}: {e}")))?;
    Ok((run, parse(raw_key, raw)?))
}

/// Remove a watch that cannot be read and say how to look at the checks by hand.
async fn reject(
    ctx: &DaemonContext,
    raw_key: &str,
    raw: &str,
    error: &ArchonError,
) -> Result<(), ArchonError> {
    let id = raw_key
        .strip_prefix(WATCH_PREFIX)
        .filter(|id| !id.is_empty())
        .unwrap_or(raw_key);
    let record = archon_core::quarantine::Quarantined::new(
        "checks-watch",
        id,
        &format!("the stored checks watch ({raw}) is unusable: {error}"),
        "look at the pull request's checks on GitHub yourself",
        "nothing more to do: the watch was removed",
    );
    tracing::error!(%raw_key, %error, "a checks watch is unusable and was removed");
    if let Err(write) = archon_core::quarantine::write(&ctx.paths.data_dir, &record) {
        tracing::error!(%write, "the quarantine record was not written; the watch is kept");
        return Ok(());
    }
    ctx.store.lock().await.kv_delete(raw_key)
}

/// Read the checks of `watch` once: `true` when the watch ended.
async fn look_once(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    mut watch: Watch,
) -> Result<bool, ArchonError> {
    let now = archon_core::now();
    let waited = |limit| now >= after(watch.delivered_at, limit);
    // Not in the match head: its store guard would live through the arms, and
    // `end` takes the store again (a self-deadlock that hung archond).
    let found = ctx
        .store
        .lock()
        .await
        .repo(&crate::resolve::repo_ref(&watch.repo));
    let repo = match found {
        Ok(repo) => repo,
        Err(ArchonError::NotFound(_)) => return end(ctx, run, &watch, "repository removed").await,
        Err(error) => return Err(error),
    };
    let binary = &ctx.config.github.binary;
    let read = checks::of_pull_request(binary, &repo, &watch.url, waited(QUEUED_LIMIT)).await;
    let checks = match read {
        Ok(checks) => checks,
        Err(error) if error.is_store() => return Err(error),
        Err(error) if waited(GIVE_UP) => return end(ctx, run, &watch, &error.to_string()).await,
        Err(error) => {
            tracing::warn!(%run, url = %watch.url, %error, "pull request checks not read; looking again later");
            Checks::Pending
        }
    };
    match checks {
        Checks::Passed => end(ctx, run, &watch, "checks passed").await,
        Checks::None if waited(NO_CHECKS_LIMIT) => end(ctx, run, &watch, "no checks").await,
        Checks::None | Checks::Pending if waited(GIVE_UP) => {
            end(
                ctx,
                run,
                &watch,
                "checks still pending a day after delivery",
            )
            .await
        }
        Checks::None | Checks::Pending => {
            watch.next_at = after(now, LOOK_EVERY);
            let raw = serde_json::to_string(&watch)?;
            ctx.store.lock().await.kv_set(&key(run), &raw)?;
            Ok(false)
        }
        Checks::Failed { names } => report_failed(ctx, run, &watch, &names).await,
        Checks::NotStarted { names, reason } => {
            report_not_started(ctx, run, &watch, &names, &reason).await
        }
    }
}

async fn report_failed(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    watch: &Watch,
    names: &[String],
) -> Result<bool, ArchonError> {
    let detail = format!("failed: {}", names.join(", "));
    let text = format!(
        "The checks of {} failed after delivery ({}): they ran on the change. Retry the \
         item with an amendment, or enqueue a follow-up",
        watch.url,
        names.join(", ")
    );
    report(
        ctx,
        run,
        watch,
        "failed",
        detail,
        NotificationKind::Outcome,
        text,
    )
    .await
}

async fn report_not_started(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    watch: &Watch,
    names: &[String],
    reason: &str,
) -> Result<bool, ArchonError> {
    let detail = format!("{}: {reason}", names.join(", "));
    let text = format!(
        "The checks of {} never started ({}): {reason}. This is not a failure of the \
         change; fix the GitHub Actions account or runners, then re-run them on GitHub",
        watch.url,
        names.join(", ")
    );
    report(
        ctx,
        run,
        watch,
        "not_started",
        detail,
        NotificationKind::Blocker,
        text,
    )
    .await
}

/// Tell the orchestrator and the operator how the checks ended, once.
async fn report(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    watch: &Watch,
    state: &str,
    detail: String,
    kind: NotificationKind,
    text: String,
) -> Result<bool, ArchonError> {
    assert!(
        matches!(state, "failed" | "not_started"),
        "a state to report"
    );
    ctx.store.lock().await.kv_delete(&key(run))?;
    let item = match ctx.store.lock().await.item(&watch.item) {
        Ok(_) => Some(&watch.item),
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    tracing::info!(%run, url = %watch.url, state, "pull request checks reported");
    finish::notify(ctx, kind, &text, item, item.and(Some(run))).await?;
    if let Some(item) = item {
        crate::push::pr_checks(ctx, item, &watch.url, state).await;
    }
    let event = TriggerEvent::PrChecks {
        item: watch.item.clone(),
        url: watch.url.clone(),
        state: state.to_owned(),
        detail,
    };
    triggers::push(ctx, event).await?;
    Ok(true)
}

async fn end(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    watch: &Watch,
    why: &str,
) -> Result<bool, ArchonError> {
    tracing::info!(%run, url = %watch.url, why, "pull request checks no longer watched");
    ctx.store.lock().await.kv_delete(&key(run))?;
    Ok(true)
}
