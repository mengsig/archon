//! A reviewer delegated on a provider whose quota is spent can remain held
//! for hours. After three minutes the sweep looks for an eligible replacement
//! and reports the block to the lead if none is available.
//!
//! The sweep keeps its own timer, apart from the dispatcher's. Each sweep
//! starts a check of every running stage and waits for none, so no stage
//! pushes another's check back; a stage whose last check still runs is
//! passed over. Every Aether request is cut off at [`REQUEST_LIMIT`].

mod replacement;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model::{ModelChoice, ModelRef};
use archon_core::ports::{Descendant, DescendantState};
use archon_core::quota::QuotaView;
use archon_core::run::{StageRun, StageState};
use chrono::TimeDelta;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::context::DaemonContext;

/// How long a reviewer may wait on a spent quota before Archon intervenes.
const WAIT_AFTER: TimeDelta = TimeDelta::minutes(3);

/// The wait between the starts of two sweeps, however long their checks run.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a check waits for one answer from Aether. A slower one counts
/// as unanswered and is asked again by the stage's next check.
pub const REQUEST_LIMIT: Duration = Duration::from_secs(10);

/// Requests Aether answers to list a lead's descendants: the lead, then the
/// agents of its run (`AetherBackend::descendants`).
const LISTING_REQUESTS: u32 = 2;

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

struct Quota {
    resets: BTreeMap<String, Option<Timestamp>>,
    headroom: Option<QuotaView>,
}

#[derive(Deserialize, Serialize)]
struct Wait {
    since: Timestamp,
    report: Option<Report>,
    replacement: Option<Replacement>,
    #[serde(default)]
    refused: BTreeSet<ModelRef>,
}

impl Wait {
    fn reported(&self) -> bool {
        self.report.as_ref().is_some_and(|report| report.sent)
    }

    fn source<'a>(&'a self, reviewer: &'a Descendant) -> &'a ModelRef {
        self.replacement
            .as_ref()
            .map_or(&reviewer.model, |pending| &pending.from)
    }
}

#[derive(Deserialize, Serialize)]
struct Report {
    text: String,
    sent: bool,
}

#[derive(Deserialize, Serialize)]
struct Replacement {
    from: ModelRef,
    to: ModelChoice,
}

/// The checks of stages that sweeps started and that have not been reaped.
type Checks = BTreeMap<StageRunId, JoinHandle<Result<(), ArchonError>>>;

/// Sweep every [`SWEEP_INTERVAL`] while Aether is connected and start-up
/// recovery is done; `Err` only when the store fails.
pub async fn watch(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    // Wall time at the start plus monotonic time since: a stepped wall
    // clock neither stretches nor shortens a wait while the daemon runs.
    let (wall, started) = (archon_core::now(), tokio::time::Instant::now());
    let mut timer = tokio::time::interval(SWEEP_INTERVAL);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut checks = Checks::new();
    loop {
        timer.tick().await;
        reap(&mut checks).await?;
        if !ctx.aether.is_connected() || !ctx.recovery.is_recovered() {
            continue;
        }
        let uptime = TimeDelta::from_std(started.elapsed()).expect("an uptime fits");
        match start_checks(&ctx, &mut checks, wall + uptime).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::error!(%error, "quota wait sweep not started"),
        }
    }
}

/// One sweep at `now` over every running stage with each check awaited:
/// what [`watch`] starts every period, there without waiting for it.
pub async fn sweep_running(ctx: &Arc<DaemonContext>, now: Timestamp) -> Result<(), ArchonError> {
    let mut checks = Checks::new();
    start_checks(ctx, &mut checks, now).await?;
    let started = checks.len();
    let checked = join_all(checks.into_values().map(joined)).await;
    assert_eq!(checked.len(), started, "one result a check");
    checked.into_iter().collect()
}

/// Start, at `now`, the check of every running stage that has none in
/// `checks`: a check still running is its stage's check for this sweep too.
async fn start_checks(
    ctx: &Arc<DaemonContext>,
    checks: &mut Checks,
    now: Timestamp,
) -> Result<(), ArchonError> {
    let live = ctx.store.lock().await.live_stage_runs()?;
    let running: Vec<StageRun> = (live.into_iter())
        .filter(|stage| stage.state == StageState::Running)
        .collect();
    forget_unwatched(ctx, &running).await?;
    let quota = Arc::new(Quota {
        resets: crate::quota::spent(ctx).unwrap_or_default(),
        headroom: crate::quota::view(ctx),
    });
    for stage in running {
        if checks.contains_key(&stage.id) {
            continue;
        }
        let (ctx, quota, id) = (ctx.clone(), quota.clone(), stage.id.clone());
        let check = async move { sweep_stage(&ctx, &stage, &quota, now).await };
        let replaced = checks.insert(id, tokio::spawn(check));
        assert!(replaced.is_none(), "one check a stage at a time");
    }
    Ok(())
}

/// Take the checks that ended out of `checks`; `Err` when one's store failed.
/// Any other failure of a check is its stage's alone, and is checked again.
async fn reap(checks: &mut Checks) -> Result<(), ArchonError> {
    let ended: Vec<StageRunId> = (checks.iter())
        .filter(|(_, check)| check.is_finished())
        .map(|(stage, _)| stage.clone())
        .collect();
    for stage in ended {
        let check = checks.remove(&stage).expect("listed just above");
        match joined(check).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::error!(%stage, %error, "quota wait check failed"),
        }
    }
    Ok(())
}

/// What a started check came to: a panic in it is its sweep's panic.
async fn joined(check: JoinHandle<Result<(), ArchonError>>) -> Result<(), ArchonError> {
    match check.await {
        Ok(checked) => checked,
        Err(stopped) if stopped.is_panic() => std::panic::resume_unwind(stopped.into_panic()),
        // Nothing aborts a check: only the runtime shutting down ends one.
        Err(stopped) => {
            tracing::info!(%stopped, "quota wait check ended with the runtime");
            Ok(())
        }
    }
}

/// A `call` of `requests` Aether requests in a row, cut off at
/// [`REQUEST_LIMIT`] for each of them as one left unanswered.
async fn bounded<T>(
    requests: u32,
    call: impl Future<Output = Result<T, ArchonError>>,
) -> Result<T, ArchonError> {
    assert!(requests >= 1, "a call asks Aether something");
    let limit = REQUEST_LIMIT * requests;
    match tokio::time::timeout(limit, call).await {
        Ok(answer) => answer,
        Err(_) => Err(ArchonError::Unavailable(format!(
            "no answer within {}s",
            limit.as_secs()
        ))),
    }
}

/// Handle reviewers due for intervention and clear waits as reviewers resume.
async fn sweep_stage(
    ctx: &DaemonContext,
    stage: &StageRun,
    quota: &Quota,
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
    let due = waiting
        .into_iter()
        .filter(|(_, wait)| now - wait.since >= WAIT_AFTER);
    let interventions = due
        .map(|(reviewer, wait)| async move { intervene(ctx, stage, &reviewer, wait, quota).await });
    join_all(interventions).await.into_iter().collect()
}

/// The live reviewers beneath `lead`, each with its own quota block as
/// Aether has it, all read at once. Each is judged by its own block alone,
/// so no other hold stops one, and an Aether that names no block class
/// stops none. `Err` when Aether does not list them.
async fn read_reviewers(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
) -> Result<Vec<(Descendant, Block)>, ArchonError> {
    let beneath = bounded(LISTING_REQUESTS, ctx.backend.descendants(lead)).await?;
    let live = (beneath.into_iter())
        .filter(|agent| agent.state == DescendantState::Live && agent.role == "reviewer");
    let reads = live.map(|reviewer| async move {
        assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
        let block = match bounded(1, ctx.backend.quota_blocked(&reviewer.agent)).await {
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
) -> Result<Vec<(Descendant, Wait)>, ArchonError> {
    let prefix = format!("{WAIT_PREFIX}{}:", stage.id);
    let mut store = ctx.store.lock().await;
    let mut seen: BTreeMap<String, String> = store.kv_prefix(&prefix)?.into_iter().collect();
    let mut noted = Vec::new();
    for (reviewer, block) in reviewers {
        let key = format!("{prefix}{}", reviewer.agent);
        let recorded = seen.remove(&key);
        let recorded = match (block, recorded) {
            (Block::Unread, _) => continue,
            (Block::Free, Some(_)) => {
                store.kv_delete(&key)?;
                continue;
            }
            (Block::Free, None) => continue,
            (Block::Blocked, Some(raw)) => Some(read_wait(&key, &raw)?),
            (Block::Blocked, None) => None,
        };
        // A new wait counts from this sweep, and is saved so; so is one that a
        // wall clock, stepped back since an earlier run saved it, left ahead.
        let wait = match recorded.filter(|wait| wait.since <= now) {
            Some(wait) => wait,
            None => {
                store.kv_set(&key, &now.to_rfc3339())?;
                Wait {
                    since: now,
                    report: None,
                    replacement: None,
                    refused: BTreeSet::new(),
                }
            }
        };
        noted.push((reviewer, wait));
    }
    // What is left names reviewers that are no longer live.
    if !seen.is_empty() {
        let gone: Vec<&str> = seen.keys().map(String::as_str).collect();
        store.kv_update(&[], &gone)?;
    }
    assert!(noted.iter().all(|(_, wait)| wait.since <= now));
    Ok(noted)
}

fn read_wait(key: &str, raw: &str) -> Result<Wait, ArchonError> {
    assert!(key.starts_with(WAIT_PREFIX));
    if raw.starts_with('{') {
        return serde_json::from_str(raw)
            .map_err(|error| ArchonError::Corrupt(format!("kv {key}: {error}")));
    }
    let since = raw
        .parse::<Timestamp>()
        .map_err(|error| ArchonError::Corrupt(format!("kv {key}: {error}")))?;
    Ok(Wait {
        since,
        report: None,
        replacement: None,
        refused: BTreeSet::new(),
    })
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

async fn intervene(
    ctx: &DaemonContext,
    stage: &StageRun,
    reviewer: &Descendant,
    mut wait: Wait,
    quota: &Quota,
) -> Result<(), ArchonError> {
    let lead = stage
        .aether_agent
        .as_ref()
        .expect("a stage with reviewers has a lead");
    assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
    assert_eq!(reviewer.role, "reviewer", "only reviewers wait here");
    let key = format!("{WAIT_PREFIX}{}:{}", stage.id, reviewer.agent);
    if let Some(pending) = &wait.replacement
        && pending.from != reviewer.model
        && pending.to.model != reviewer.model
    {
        return ctx.store.lock().await.kv_delete(&key);
    }
    let choice = match replacement::choose(ctx, stage, reviewer, quota.headroom.as_ref(), &wait)
        .await
    {
        Ok(choice) => choice,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::warn!(stage = %stage.id, reviewer = %reviewer.agent, %error, "could not choose a quota replacement");
            None
        }
    };
    if choice.is_none() && wait.reported() {
        return Ok(());
    }
    match bounded(1, ctx.backend.quota_blocked(&reviewer.agent)).await? {
        Some(true) => {}
        Some(false) | None => return ctx.store.lock().await.kv_delete(&key),
    }
    match choice {
        Some(to) if to.model == reviewer.model => {
            replacement::release(ctx, reviewer, &key, &wait).await
        }
        Some(to) => replacement::apply(ctx, reviewer, &key, &mut wait, to).await,
        None => report(ctx, lead, reviewer, &key, wait, &quota.resets).await,
    }
}

async fn save_wait(ctx: &DaemonContext, key: &str, wait: &Wait) -> Result<(), ArchonError> {
    assert!(key.starts_with(WAIT_PREFIX));
    let stored = serde_json::to_string(wait)?;
    ctx.store.lock().await.kv_set(key, &stored)
}

async fn report(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &Descendant,
    key: &str,
    mut wait: Wait,
    resets: &BTreeMap<String, Option<Timestamp>>,
) -> Result<(), ArchonError> {
    assert_ne!(reviewer.agent, *lead, "the lead receives the report");
    if wait.report.is_none() {
        let source = wait.source(reviewer);
        let resets_at = resets.get(&source.provider).copied().flatten();
        wait.report = Some(Report {
            text: report_text(&reviewer.agent, source, resets_at),
            sent: false,
        });
        save_wait(ctx, key, &wait).await?;
    }
    let report = wait.report.as_mut().expect("the report was saved");
    assert!(!report.sent, "a wait is reported once");
    let message_key = format!("{key}:report:{}", wait.since.to_rfc3339());
    bounded(
        1,
        ctx.backend.send_message(lead, &report.text, &message_key),
    )
    .await?;
    report.sent = true;
    save_wait(ctx, key, &wait).await
}

fn report_text(reviewer: &AetherAgentId, model: &ModelRef, resets_at: Option<Timestamp>) -> String {
    let provider = &model.provider;
    let reset = resets_at.map_or_else(
        || "no reset is known".to_owned(),
        |at| format!("resets at {}", at.format("%Y-%m-%d %H:%M UTC")),
    );
    format!(
        "Archon: reviewer {reviewer} ({model}) is still waiting on {provider} because its quota is spent \
         ({reset}). No eligible replacement with confirmed headroom is available. The review \
         stays pending on this reviewer."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_with_no_due_wait_ends_within_a_period() {
        let reads = REQUEST_LIMIT * (LISTING_REQUESTS + 1);
        assert!(reads <= SWEEP_INTERVAL, "{reads:?}");
    }
}
