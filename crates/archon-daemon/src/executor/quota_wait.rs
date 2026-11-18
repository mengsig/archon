//! A reviewer delegated on a provider whose quota is spent is blocked by Aether
//! until the quota resets, and a held stage has its deadline moved, so the
//! review could wait for hours. Once such a reviewer has waited
//! [`FAIL_AFTER`], the sweep cancels it with a hint telling its lead to
//! re-plan the review.
//!
//! The sweep keeps its own timer, apart from the dispatcher's. Every stage
//! is checked at once and every Aether request is cut off at
//! [`REQUEST_LIMIT`], so a sweep ends within a few requests however slow any
//! stage answers, and a reviewer is cancelled within five minutes of the
//! sweep that first saw it blocked.
//!
//! Aether has no conditional cancel. The block is read again right before
//! the cancel, which leaves one round trip in which a reviewer whose quota
//! came back is cancelled all the same: a known limit.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageRun, StageState};
use chrono::TimeDelta;
use futures_util::future::join_all;
use tokio::time::MissedTickBehavior;

use super::resume;
use crate::context::DaemonContext;

/// How long a reviewer may wait on a spent quota before it is stopped.
const FAIL_AFTER: TimeDelta = TimeDelta::minutes(3);

/// The wait between the starts of two sweeps, unless one runs longer.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a sweep waits for one answer from Aether. A slower one counts
/// as unanswered and is asked again next sweep.
pub const REQUEST_LIMIT: Duration = Duration::from_secs(10);

/// `kv` key prefix of when a reviewer was first seen waiting, per stage run.
const WAIT_PREFIX: &str = "quota_wait:";

/// What a sweep read of a live reviewer's own quota block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Blocked,
    Free,
    /// Aether did not answer in time: the reviewer's wait stays as it is.
    Unread,
}

/// Sweep every [`SWEEP_INTERVAL`] while Aether is connected and start-up
/// recovery is done; `Err` only when the store fails.
pub async fn watch(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    // Wall time at the start plus monotonic time since: a stepped wall
    // clock neither stretches nor shortens a wait while the daemon runs.
    let (wall, started) = (archon_core::now(), tokio::time::Instant::now());
    let mut timer = tokio::time::interval(SWEEP_INTERVAL);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        if !ctx.aether.is_connected() || !ctx.recovery.is_recovered() {
            continue;
        }
        let uptime = TimeDelta::from_std(started.elapsed()).expect("an uptime fits");
        match sweep_running(&ctx, wall + uptime).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::error!(%error, "quota wait sweep failed"),
        }
    }
}

/// One sweep at `now` over every running stage, all at once, as [`watch`]
/// runs it: no stage's check waits for another's.
pub async fn sweep_running(ctx: &DaemonContext, now: Timestamp) -> Result<(), ArchonError> {
    let live = ctx.store.lock().await.live_stage_runs()?;
    let running: Vec<StageRun> = (live.into_iter())
        .filter(|stage| stage.state == StageState::Running)
        .collect();
    forget_unwatched(ctx, &running).await?;
    // Only the hint's reset time comes from the last reading; Aether's own
    // block says the reviewer waits on quota.
    let resets = crate::quota::spent(ctx).unwrap_or_default();
    let checks = running
        .iter()
        .map(|stage| sweep_stage(ctx, stage, &resets, now));
    let checked = join_all(checks).await;
    assert_eq!(checked.len(), running.len(), "one check a stage");
    checked.into_iter().collect()
}

/// `request` to Aether, cut off at [`REQUEST_LIMIT`] as one left unanswered.
async fn bounded<T>(
    request: impl Future<Output = Result<T, ArchonError>>,
) -> Result<T, ArchonError> {
    match tokio::time::timeout(REQUEST_LIMIT, request).await {
        Ok(answer) => answer,
        Err(_) => Err(ArchonError::Unavailable(format!(
            "no answer within {}s",
            REQUEST_LIMIT.as_secs()
        ))),
    }
}

/// Stop each reviewer of the running `stage` that waited [`FAIL_AFTER`] on
/// a spent quota, and forget the waits of its reviewers that no longer wait.
async fn sweep_stage(
    ctx: &DaemonContext,
    stage: &StageRun,
    resets: &BTreeMap<String, Option<Timestamp>>,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert_eq!(
        stage.state,
        StageState::Running,
        "only a running stage waits"
    );
    let Some(lead) = &stage.aether_agent else {
        return Ok(());
    };
    let reviewers = match read_reviewers(ctx, lead).await {
        Ok(reviewers) => reviewers,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::info!(stage = %stage.id, %error, "quota wait not read; read again next sweep");
            return Ok(());
        }
    };
    let waiting = note_waits(ctx, stage, reviewers, now).await?;
    let due = (waiting.iter()).filter(|(_, since)| now - *since >= FAIL_AFTER);
    let stops = due.map(|(reviewer, _)| {
        let resets_at = resets.get(&reviewer.model.provider).copied().flatten();
        stop(ctx, stage, lead, reviewer, resets_at)
    });
    join_all(stops).await.into_iter().collect()
}

/// The live reviewers beneath `lead`, each with its own quota block as
/// Aether has it, all read at once. Each is judged by its own block alone,
/// so no other hold stops one, and an Aether that names no block class
/// stops none. `Err` when Aether does not list them.
async fn read_reviewers(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
) -> Result<Vec<(Descendant, Block)>, ArchonError> {
    let beneath = bounded(ctx.backend.descendants(lead)).await?;
    let live = (beneath.into_iter())
        .filter(|agent| agent.state == DescendantState::Live && agent.role == "reviewer");
    let reads = live.map(|reviewer| async move {
        assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
        let block = match bounded(ctx.backend.quota_blocked(&reviewer.agent)).await {
            Ok(Some(true)) => Block::Blocked,
            // One Aether no longer knows has ended: it does not wait.
            Ok(Some(false) | None) => Block::Free,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::info!(reviewer = %reviewer.agent, %error, "quota block not read; read again next sweep");
                Block::Unread
            }
        };
        Ok((reviewer, block))
    });
    join_all(reads).await.into_iter().collect()
}

/// Record when each blocked one of `reviewers` was first seen waiting on
/// `stage`, keep the wait of one left unread, and forget every other: one
/// Aether unblocked, as its quota was restored, has its wait cleared. Each
/// blocked reviewer with the time its wait began.
async fn note_waits(
    ctx: &DaemonContext,
    stage: &StageRun,
    reviewers: Vec<(Descendant, Block)>,
    now: Timestamp,
) -> Result<Vec<(Descendant, Timestamp)>, ArchonError> {
    let prefix = format!("{WAIT_PREFIX}{}:", stage.id);
    let mut store = ctx.store.lock().await;
    let mut seen: BTreeMap<String, String> = store.kv_prefix(&prefix)?.into_iter().collect();
    let mut noted = Vec::new();
    for (reviewer, block) in reviewers {
        let key = format!("{prefix}{}", reviewer.agent);
        let recorded = seen.remove(&key);
        let since = match (block, recorded) {
            (Block::Unread, _) => continue,
            (Block::Free, Some(_)) => {
                store.kv_delete(&key)?;
                continue;
            }
            (Block::Free, None) => continue,
            (Block::Blocked, Some(raw)) => raw
                .parse::<Timestamp>()
                .map_err(|e| ArchonError::Corrupt(format!("kv {key}: {e}")))?,
            (Block::Blocked, None) => {
                store.kv_set(&key, &now.to_rfc3339())?;
                now
            }
        };
        // A wall clock stepped back since an earlier run recorded the wait.
        noted.push((reviewer, since.min(now)));
    }
    // What is left names reviewers that are no longer live.
    if !seen.is_empty() {
        let gone: Vec<&str> = seen.keys().map(String::as_str).collect();
        store.kv_update(&[], &gone)?;
    }
    assert!(noted.iter().all(|(_, since)| *since <= now));
    Ok(noted)
}

/// Drop the waits recorded for stage runs other than the `watched` ones.
async fn forget_unwatched(ctx: &DaemonContext, watched: &[StageRun]) -> Result<(), ArchonError> {
    assert!(watched.iter().all(|s| s.state == StageState::Running));
    let live: BTreeSet<String> = watched.iter().map(|s| s.id.to_string()).collect();
    let mut store = ctx.store.lock().await;
    let marks = store.kv_prefix(WAIT_PREFIX)?;
    let gone: Vec<&str> = (marks.iter())
        .map(|(key, _)| key.as_str())
        .filter(|key| {
            let rest = key.strip_prefix(WAIT_PREFIX).expect("the requested prefix");
            rest.split_once(':')
                .is_none_or(|(stage, _)| !live.contains(stage))
        })
        .collect();
    if !gone.is_empty() {
        store.kv_update(&[], &gone)?;
    }
    Ok(())
}

/// Cancel the due `reviewer` if Aether still blocks it on its quota, with
/// the re-plan hint as the reason its lead reads, then tell the lead the same.
async fn stop(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    reviewer: &Descendant,
    resets_at: Option<Timestamp>,
) -> Result<(), ArchonError> {
    assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
    assert_eq!(reviewer.role, "reviewer", "only a reviewer is stopped");
    let key = format!("{WAIT_PREFIX}{}:{}", stage.id, reviewer.agent);
    // Read again right before the cancel: see the module's known limit.
    match bounded(ctx.backend.quota_blocked(&reviewer.agent)).await {
        Ok(Some(true)) => {}
        Ok(Some(false) | None) => return ctx.store.lock().await.kv_delete(&key),
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::info!(stage = %stage.id, reviewer = %reviewer.agent, %error, "quota block not read again; read next sweep");
            return Ok(());
        }
    }
    let text = replan_hint(reviewer, resets_at);
    match bounded(ctx.backend.cancel(&reviewer.agent, &text)).await {
        Ok(()) => {
            tracing::info!(stage = %stage.id, reviewer = %reviewer.agent, "reviewer waiting on a spent quota stopped");
            tell_lead(ctx, stage, lead, &reviewer.agent, &text).await?;
        }
        Err(error) if error.is_store() => return Err(error),
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(stage = %stage.id, %error, "quota-waiting reviewer not stopped; tried again next sweep");
            return Ok(());
        }
        // Most likely the lead or its reviewer ended meanwhile.
        Err(refusal) => {
            tracing::warn!(stage = %stage.id, reviewer = %reviewer.agent, %refusal, "quota-waiting reviewer not stopped");
        }
    }
    ctx.store.lock().await.kv_delete(&key)
}

/// Tell `lead` the hint its cancelled reviewer's outcome also carries. Keyed,
/// so it is delivered once; a lost message leaves the hint in that outcome.
async fn tell_lead(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
    text: &str,
) -> Result<(), ArchonError> {
    assert!(
        text.contains("Re-plan the review"),
        "the hint says what to do"
    );
    let key = format!("{}:quota-wait:{reviewer}", stage.id);
    match bounded(ctx.backend.send_message(lead, text, &key)).await {
        Ok(()) => Ok(()),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(stage = %stage.id, %lead, %error, "re-plan hint not sent; the cancelled reviewer's outcome carries it");
            Ok(())
        }
    }
}

/// What the lead is told; the same words for a repeat of the same wait.
fn replan_hint(reviewer: &Descendant, resets_at: Option<Timestamp>) -> String {
    let provider = &reviewer.model.provider;
    let resets = resets_at.map_or_else(
        || "names no reset".to_owned(),
        |at| format!("resets at {}", at.format("%Y-%m-%d %H:%M UTC")),
    );
    let text = format!(
        "Archon: your reviewer {agent} ({model}) waited over {minutes} min for {provider}, whose \
         quota is spent and {resets}, so Archon cancelled it. Re-plan the review: delegate it \
         again on a reviewer model of another provider your instructions allow. If they allow \
         none, call fail_task saying the {provider} quota is spent, so the orchestrator re-plans.",
        agent = reviewer.agent,
        model = reviewer.model,
        minutes = FAIL_AFTER.num_minutes(),
    );
    assert!(
        text.contains("Re-plan the review"),
        "the hint says what to do"
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Aether requests a stage's check makes in a row up to its cancel: the
    /// lead's descendants, each reviewer's block, that block again, the cancel.
    const REQUESTS_TO_CANCEL: u32 = 4;

    /// The first sweep starting [`FAIL_AFTER`] after a wait was seen stops
    /// it. Sweeps start a period apart, or as far as the slowest sweep runs,
    /// its message to the lead included; so a blocked reviewer ends within
    /// five minutes however slow Aether answers within [`REQUEST_LIMIT`].
    #[test]
    fn a_blocked_reviewer_is_stopped_within_five_minutes() {
        assert!(REQUEST_LIMIT <= Duration::from_secs(20));
        let to_cancel = REQUEST_LIMIT * REQUESTS_TO_CANCEL;
        let slowest_sweep = to_cancel + REQUEST_LIMIT;
        let between_sweeps = SWEEP_INTERVAL.max(slowest_sweep);
        let wait = FAIL_AFTER.to_std().expect("positive");
        let worst = wait + between_sweeps + to_cancel;
        assert!(worst <= Duration::from_secs(5 * 60), "{worst:?}");
    }
}
