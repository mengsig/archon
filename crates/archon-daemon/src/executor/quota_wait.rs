//! A reviewer delegated on a provider whose quota is spent is blocked by Aether
//! until the quota resets, and a held stage has its deadline moved, so the
//! review could wait for hours. Once such a reviewer has waited
//! [`FAIL_AFTER`], the sweep cancels it with a hint telling its lead to
//! re-plan the review, well within five minutes of the wait being seen.
//! The sweep runs on its own timer, so slow dispatch work never delays it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageRun, StageState};
use chrono::TimeDelta;

use super::resume;
use crate::context::DaemonContext;

/// How long a reviewer may wait on a spent quota before it is stopped.
const FAIL_AFTER: TimeDelta = TimeDelta::minutes(3);

/// The wait between two sweeps.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// `kv` key prefix of when a reviewer was first seen waiting, per stage run.
const WAIT_PREFIX: &str = "quota_wait:";

/// A live reviewer whose provider has no quota left, and when it frees up.
struct Waiting {
    reviewer: Descendant,
    resets_at: Option<Timestamp>,
}

/// Sweep every [`SWEEP_INTERVAL`] while Aether is connected and start-up
/// recovery is done; `Err` only when the store fails.
pub async fn watch(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        if !ctx.aether.is_connected() || !ctx.recovery.is_recovered() {
            continue;
        }
        match sweep_running(&ctx).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::error!(%error, "quota wait sweep failed"),
        }
    }
}

/// One sweep over every running stage, as [`watch`] runs it.
pub async fn sweep_running(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let live = ctx.store.lock().await.live_stage_runs()?;
    let running: Vec<StageRun> = (live.into_iter())
        .filter(|stage| stage.state == StageState::Running)
        .collect();
    assert!(running.iter().all(|s| s.state == StageState::Running));
    sweep(ctx, &running, archon_core::now()).await
}

/// Stop each reviewer of the `running` stages that waited [`FAIL_AFTER`] on
/// a spent quota, and forget the waits of reviewers that no longer wait.
async fn sweep(
    ctx: &DaemonContext,
    running: &[StageRun],
    now: Timestamp,
) -> Result<(), ArchonError> {
    forget_unwatched(ctx, running).await?;
    // Only the hint's reset time comes from the last reading; Aether's own
    // block says the reviewer waits on quota.
    let resets = crate::quota::spent(ctx).unwrap_or_default();
    for stage in running {
        assert_eq!(
            stage.state,
            StageState::Running,
            "only a running stage waits"
        );
        let Some(lead) = &stage.aether_agent else {
            continue;
        };
        let waiting = match waiting_reviewers(ctx, lead, &resets).await {
            Ok(waiting) => waiting,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::info!(stage = %stage.id, %error, "quota wait not read; read again next sweep");
                continue;
            }
        };
        for (waiting, since) in note_waits(ctx, stage, waiting, now).await? {
            if now - since >= FAIL_AFTER {
                stop(ctx, stage, lead, &waiting, now - since).await?;
            }
        }
    }
    Ok(())
}

/// The live reviewers beneath `lead` that Aether itself blocks on a spent
/// quota or usage limit, each with its provider's reset from `resets`. Each
/// is judged by its own block, so no other hold stops one. An Aether that
/// names no block class stops none.
async fn waiting_reviewers(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    resets: &BTreeMap<String, Option<Timestamp>>,
) -> Result<Vec<Waiting>, ArchonError> {
    let tree = ctx.backend.pulse(lead).await?;
    if !tree.is_some_and(|pulse| pulse.held_for_cause) {
        return Ok(Vec::new());
    }
    let beneath = ctx.backend.descendants(lead).await?;
    let mut waiting = Vec::new();
    for reviewer in beneath {
        if reviewer.state != DescendantState::Live || reviewer.role != "reviewer" {
            continue;
        }
        let pulse = ctx.backend.pulse(&reviewer.agent).await?;
        if !pulse.is_some_and(|pulse| pulse.quota_blocked) {
            continue;
        }
        assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
        let resets_at = resets.get(&reviewer.model.provider).copied().flatten();
        waiting.push(Waiting {
            reviewer,
            resets_at,
        });
    }
    debug_assert!(
        waiting
            .iter()
            .all(|w| w.reviewer.state == DescendantState::Live)
    );
    Ok(waiting)
}

/// Record when each of `waiting` was first seen waiting on `stage`, and
/// forget every other reviewer of it: one Aether unblocked in time, as its
/// quota was restored, has its wait cleared. Each reviewer with the time its wait began.
async fn note_waits(
    ctx: &DaemonContext,
    stage: &StageRun,
    waiting: Vec<Waiting>,
    now: Timestamp,
) -> Result<Vec<(Waiting, Timestamp)>, ArchonError> {
    let prefix = format!("{WAIT_PREFIX}{}:", stage.id);
    let mut store = ctx.store.lock().await;
    let mut seen: BTreeMap<String, String> = store.kv_prefix(&prefix)?.into_iter().collect();
    let mut noted = Vec::with_capacity(waiting.len());
    for waiting in waiting {
        let key = format!("{prefix}{}", waiting.reviewer.agent);
        let since = match seen.remove(&key) {
            Some(raw) => raw
                .parse::<Timestamp>()
                .map_err(|e| ArchonError::Corrupt(format!("kv {key}: {e}")))?,
            None => {
                store.kv_set(&key, &now.to_rfc3339())?;
                now
            }
        };
        assert!(since <= now, "a wait is not seen before it began");
        noted.push((waiting, since));
    }
    if !seen.is_empty() {
        let gone: Vec<&str> = seen.keys().map(String::as_str).collect();
        store.kv_update(&[], &gone)?;
    }
    Ok(noted)
}

/// Drop the waits recorded for stage runs other than the `watched` ones.
async fn forget_unwatched(ctx: &DaemonContext, watched: &[StageRun]) -> Result<(), ArchonError> {
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

/// Cancel the waiting reviewer if it is still quota-blocked, with the
/// re-plan hint as the reason its lead reads, then tell the lead the same.
async fn stop(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    waiting: &Waiting,
    waited: TimeDelta,
) -> Result<(), ArchonError> {
    let reviewer = &waiting.reviewer;
    assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
    assert!(
        waited >= FAIL_AFTER,
        "a reviewer is stopped only after its wait"
    );
    let key = format!("{WAIT_PREFIX}{}:{}", stage.id, reviewer.agent);
    // Aether has no conditional cancel: the block is read again just before
    // the cancel, which leaves one round trip for the reviewer to resume in.
    match still_blocked(ctx, stage, &reviewer.agent).await? {
        Some(true) => {}
        Some(false) => return ctx.store.lock().await.kv_delete(&key),
        None => return Ok(()),
    }
    let text = replan_hint(reviewer, waiting.resets_at);
    match ctx.backend.cancel(&reviewer.agent, &text).await {
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

/// Whether Aether still blocks `reviewer` on a spent quota; `None` when it
/// did not answer, so the next sweep reads it again.
async fn still_blocked(
    ctx: &DaemonContext,
    stage: &StageRun,
    reviewer: &AetherAgentId,
) -> Result<Option<bool>, ArchonError> {
    assert_eq!(
        stage.state,
        StageState::Running,
        "only a running stage waits"
    );
    match ctx.backend.quota_blocked(reviewer).await {
        Ok(blocked) => Ok(Some(blocked.unwrap_or(false))),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::info!(stage = %stage.id, %reviewer, %error, "quota block not read again; read next sweep");
            Ok(None)
        }
    }
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
    match ctx.backend.send_message(lead, text, &key).await {
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

    /// A wait is seen within one dispatch pass and stopped by the first pass
    /// after [`FAIL_AFTER`], so a blocked reviewer ends within five minutes.
    /// Each sweep's own work, a few Aether calls a reviewer, is ignored here.
    #[test]
    fn a_blocked_reviewer_is_stopped_within_five_minutes() {
        let pass = SWEEP_INTERVAL.as_secs();
        let seconds = u64::try_from(FAIL_AFTER.num_seconds()).expect("positive");
        assert!(
            2 * pass + seconds < 5 * 60,
            "{pass}s passes, {seconds}s wait"
        );
    }
}
