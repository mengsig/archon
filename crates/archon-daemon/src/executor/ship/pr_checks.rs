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
    at + chrono::Duration::from_std(wait).expect("a day fits")
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
        if parse(&earlier, &raw)?.url == url {
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

/// Look once at every watch that is due; returns how many ended.
pub(crate) async fn look(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let watched = ctx.store.lock().await.kv_prefix(WATCH_PREFIX)?;
    let mut ended = 0;
    for (raw_key, raw) in watched {
        let run = raw_key
            .strip_prefix(WATCH_PREFIX)
            .expect("ranged by prefix");
        let run = PipelineRunId::parse(run)
            .map_err(|e| ArchonError::Corrupt(format!("checks watch {raw_key}: {e}")))?;
        let watch = parse(&raw_key, &raw)?;
        if watch.next_at > archon_core::now() {
            continue;
        }
        if look_once(ctx, &run, watch).await? {
            ended += 1;
        }
    }
    Ok(ended)
}

/// Read the checks of `watch` once: `true` when the watch ended.
async fn look_once(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    mut watch: Watch,
) -> Result<bool, ArchonError> {
    let now = archon_core::now();
    let waited = |limit| now >= after(watch.delivered_at, limit);
    let repo = match ctx
        .store
        .lock()
        .await
        .repo(&crate::resolve::repo_ref(&watch.repo))
    {
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
