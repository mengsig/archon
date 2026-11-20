//! Settling stage runs: infra retries, persisting outcomes and verdicts,
//! and applying the driver's actions.

use std::time::{Duration, Instant};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model_tier::ModelCatalog;
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::run::{
    PipelineRun, RecordRepair, RunState, StageOutcome, StageRun, StageState, Verdict, VerdictNote,
};
use archon_core::steer::RunEdit;
use archon_pipeline::{EngineAction, PipelineSpec, SpawnPlan};

use super::{
    FollowUp, RunTable, SettleRetry, descendants, engine_wait, finish, gates, notional, pages,
    recover, restatement, resume, ship, siblings, spawn, stall, swarm, team,
};
use crate::context::DaemonContext;
use crate::disk::{self, FullDisk};
use crate::resolve::RepoEnv;

/// The first wait before a completed stage whose verification failed on a
/// `gh` call is settled again; it doubles up to [`SETTLE_RETRY_MAX`].
const SETTLE_RETRY_FIRST: Duration = Duration::from_secs(15);
const SETTLE_RETRY_MAX: Duration = Duration::from_secs(15 * 60);

/// Settle `id` with `outcome`. A `gh` call that fails while the outcome is
/// verified is that stage's trouble alone: the stage stays live and is
/// settled again after a growing wait (the sweep asks), its item's blocker
/// told once, and the event stream and every other stage go on.
pub(super) async fn settle(
    ctx: &DaemonContext,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    let _claim = if matches!(outcome, StageOutcome::Completed { .. }) {
        let Some(claim) = claim_verification(ctx, id).await else {
            return Ok(FollowUp::default());
        };
        Some(claim)
    } else {
        None
    };
    match settle_verified(ctx, id, outcome).await {
        Err(error) if super::gh::is_failure(&error) => {
            hold_for_retry(
                ctx,
                id,
                "verifying the completed stage against GitHub",
                error,
            )
            .await
        }
        Err(ArchonError::Unavailable(why)) if why.starts_with(super::model_catalog::RETRY_WAIT) => {
            let error = ArchonError::Unavailable(why);
            hold_for_retry(ctx, id, "settling the stage", error).await
        }
        Err(error) => Err(error),
        Ok(follow) => {
            let cleared = super::coordination::clear_stage(ctx, &id.to_string()).await?;
            let mut table = ctx.runs.lock().await;
            if cleared {
                table.settle_retries.remove(id);
            } else {
                schedule_retry(&mut table, id);
            }
            Ok(follow)
        }
    }
}

async fn claim_verification(
    ctx: &DaemonContext,
    id: &StageRunId,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let mut table = ctx.runs.lock().await;
    if table
        .settle_retries
        .get(id)
        .is_some_and(|r| r.not_before > Instant::now())
    {
        return None;
    }
    let lock = table
        .settle_claims
        .get(id)
        .and_then(std::sync::Weak::upgrade)
        .unwrap_or_else(|| {
            let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            table
                .settle_claims
                .insert(id.clone(), std::sync::Arc::downgrade(&lock));
            lock
        });
    lock.try_lock_owned().ok()
}

async fn hold_for_retry(
    ctx: &DaemonContext,
    id: &StageRunId,
    operation: &str,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    let item = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        store.run(&stage.run)?.item
    };
    let failures = {
        let mut table = ctx.runs.lock().await;
        schedule_retry(&mut table, id)
    };
    tracing::warn!(stage = %id, %item, %error, failures, operation, "stage settlement will retry");
    let what = format!("Item {item}: {operation} ({id})");
    let subject = format!("stage:{id}");
    super::coordination::isolate(ctx, &subject, Some(&item), &what, Err(error)).await?;
    Ok(FollowUp::default())
}

pub(super) fn schedule_retry(table: &mut RunTable, id: &StageRunId) -> u32 {
    let failures = table
        .settle_retries
        .get(id)
        .map_or(0, |r| r.failures)
        .saturating_add(1);
    let wait = (SETTLE_RETRY_FIRST * 2u32.pow((failures - 1).min(6))).min(SETTLE_RETRY_MAX);
    table.settle_retries.insert(
        id.clone(),
        SettleRetry {
            failures,
            not_before: Instant::now() + wait,
        },
    );
    failures
}

async fn settle_verified(
    ctx: &DaemonContext,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    let completed = matches!(outcome, StageOutcome::Completed { .. });
    // A completion of a stage the review cap already stopped is not verified.
    if !completed || stall::escalation_marked(&*ctx.store.lock().await, id)? {
        let mut table = ctx.runs.lock().await;
        let outcome = {
            let store = ctx.store.lock().await;
            let outcome = stall::intended(&store, id, outcome)?;
            notional::label(&store, id, outcome)?
        };
        return settle_locked(ctx, &mut table, id, outcome).await;
    }
    let (stage, run, review, daemon_ships, planner, record_stage, records_build, as_sent) = {
        let table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        if stage.state.is_terminal() {
            return Ok(FollowUp::default());
        }
        let run = store.run(&stage.run)?;
        let driver = table.drivers.get(&run.id);
        let review = super::review::Owed {
            reviewers: driver.map_or(0, |d| d.reviewers_owed(&stage.stage_id)),
            records_ship: driver.is_some_and(|d| d.records_ship(&stage.stage_id)),
        };
        let daemon_ships = driver.is_some_and(|d| d.ships_by_daemon());
        let planner = driver.and_then(|d| swarm::Planner::of(d.spec(), &stage.stage_id));
        let record_stage = driver.and_then(|d| {
            let spec = d.spec();
            let record = spec.ship_record_stage()?;
            let after = spec.stage_index(&stage.stage_id)? > spec.stage_index(&record.id)?;
            after.then(|| record.id.clone())
        });
        let records_build = driver.is_some_and(|d| {
            archon_pipeline::acceptance::reports_requirements(d.spec(), &stage.stage_id)
        });
        let as_sent = driver.is_some_and(|d| decided_as_sent(d, &stage, &outcome));
        (
            stage,
            run,
            review,
            daemon_ships,
            planner,
            record_stage,
            records_build,
            as_sent,
        )
    };
    if review.reviewers > 0
        && let Some(capped) = super::review::capped_at_completion(ctx, &stage).await?
    {
        return settle_capped(ctx, id, capped).await;
    }
    if as_sent {
        return settle_as_sent(ctx, &stage, &run, outcome).await;
    }
    // Leave the stage live until verification is persisted. If interrupted,
    // reconnect reconciliation reads the task outcome and verifies it again.
    let outcome = super::intent::verify(ctx, &run, &stage, outcome).await?;
    let proposal_refused = matches!(&outcome, StageOutcome::Failed { error, .. }
        if super::intent::is_refusal(error));
    let outcome = ship::verify(ctx, &run, &stage, daemon_ships, outcome).await?;
    let outcome = match (review.records_ship, &record_stage) {
        (true, _) => {
            let outcome = ship::with_full_ids(&run, outcome).await?;
            ship::verify_no_change(ctx, &run, outcome).await?
        }
        (false, Some(record)) => {
            ship::verify_standing_no_change(ctx, &run, record, &stage, outcome).await?
        }
        (false, None) => outcome,
    };
    let Some(outcome) = Box::pin(super::acceptance::verify(
        ctx,
        &run,
        &stage,
        records_build,
        outcome,
    ))
    .await?
    else {
        return Ok(FollowUp::default());
    };
    let outcome = super::refresh::verify_unreviewed(ctx, &run, review, outcome).await?;
    let (outcome, verified) = super::review::verify(ctx, &run, &stage, review, outcome).await?;
    let planned = planner.is_some();
    let was_completed = matches!(outcome, StageOutcome::Completed { .. });
    let (outcome, assigned) = swarm::verify(ctx, &run, &stage, planner, outcome).await?;
    // Refused plans and proposal drafts return through the bounded correction path.
    let refused = proposal_refused
        || (planned && was_completed && matches!(outcome, StageOutcome::Failed { .. }));
    let mut table = ctx.runs.lock().await;
    let (stage, run, all) = {
        let mut store = ctx.store.lock().await;
        let current_stage = store.stage_run(id)?;
        let current_run = store.run(&stage.run)?;
        if current_stage.state != stage.state || current_run.state != run.state {
            // Cancellation, another settle or a driver decision won the race.
            // Its outcome takes precedence over this verification result.
            return Ok(FollowUp::poke());
        }
        // So does the review cap's stop, marked while this was verified.
        if stall::escalation_marked(&store, id)? {
            let escalated = stall::intended(&store, id, outcome)?;
            drop(store);
            return settle_locked(ctx, &mut table, id, escalated).await;
        }
        let all = store.stage_runs(&current_run.id)?;
        assert_eq!(
            current_stage.run, current_run.id,
            "a stage settles into its own run"
        );
        // Only a pass the review gate held may later anchor a refresh's waiver.
        let completed = matches!(outcome, StageOutcome::Completed { .. });
        if let Some(verified) = verified.as_ref().filter(|_| completed) {
            assert!(
                was_completed,
                "only a completed stage holds a verified review"
            );
            store.record_run_review(&current_run.id, verified)?;
        }
        assert!(
            all.iter().any(|s| s.id == current_stage.id),
            "the run lists the stage it settles"
        );
        (current_stage, current_run, all)
    };
    if refused
        && let Some(follow) =
            infra_retry(ctx, &mut table, &stage, &run, &all, &outcome, true).await?
    {
        return Ok(follow);
    }
    if let Some(edit) = assigned {
        assign_roles(ctx, &mut table, &run, edit).await?;
    }
    // Other verification failures are final stage outcomes, not infrastructure
    // failures from Aether: they must not enter the stage's retry path.
    Box::pin(persist(ctx, &mut table, stage, run, &all, outcome)).await
}

/// Settle a completion of a stage whose review already failed the round
/// cap, its reviewers' events having been missed, as the cap's escalation.
async fn settle_capped(
    ctx: &DaemonContext,
    id: &StageRunId,
    capped: super::review::Capped,
) -> Result<FollowUp, ArchonError> {
    assert_eq!(capped.stage, *id, "the cap of the stage settling");
    assert!(
        StageOutcome::ends_run(&capped.reason),
        "a capped review escalates"
    );
    tracing::warn!(stage = %id, "review round cap reached with reviewer events missed");
    let escalated = StageOutcome::Failed {
        error: capped.reason,
        infra: false,
    };
    let mut table = ctx.runs.lock().await;
    settle_locked(ctx, &mut table, id, escalated).await
}

/// Record the roles a swarm plan gives a team stage and drop the run's
/// driver, which `decide` then rebuilds from the store with them.
async fn assign_roles(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: &PipelineRun,
    edit: RunEdit,
) -> Result<(), ArchonError> {
    let RunEdit::AssignRoles { stage, roles, .. } = &edit else {
        unreachable!("a plan assigns roles, not {edit:?}");
    };
    assert!(!roles.is_empty(), "a plan assigns at least one role");
    let text = format!(
        "The swarm plan assigned tasks {} to team stage `{stage}`.",
        roles.join(", ")
    );
    ctx.store
        .lock()
        .await
        .append_run_edit(&run.id, &edit, archon_core::now())?;
    table.drivers.remove(&run.id);
    progress(ctx, run, text).await
}

/// Whether the gate decides `stage`'s completed `outcome` as its agent sent
/// it: a stated failure, or no `structured`, of a stage that owes a record.
fn decided_as_sent(
    driver: &archon_pipeline::RunDriver,
    stage: &StageRun,
    outcome: &StageOutcome,
) -> bool {
    assert!(!stage.state.is_terminal(), "only a live stage settles");
    let StageOutcome::Completed {
        summary,
        structured,
    } = outcome
    else {
        panic!("only a completed outcome is verified")
    };
    driver.decides_as_sent(&stage.stage_id, summary, structured.as_ref())
}

/// Persist a completed `outcome` unverified, for the gate to decide as sent:
/// it fails or repairs it and passes neither, so nothing unverified ships.
async fn settle_as_sent(
    ctx: &DaemonContext,
    seen: &StageRun,
    seen_run: &PipelineRun,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(matches!(outcome, StageOutcome::Completed { .. }));
    assert_eq!(seen.run, seen_run.id, "a stage settles into its own run");
    let mut table = ctx.runs.lock().await;
    let (stage, run, all) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(&seen.id)?;
        let run = store.run(&stage.run)?;
        if stage.state != seen.state || run.state != seen_run.state {
            // Cancellation, another settle or a driver decision won the race.
            return Ok(FollowUp::poke());
        }
        // So does the review cap's stop, marked meanwhile.
        if stall::escalation_marked(&store, &seen.id)? {
            let escalated = stall::intended(&store, &seen.id, outcome)?;
            drop(store);
            return settle_locked(ctx, &mut table, &seen.id, escalated).await;
        }
        let all = store.stage_runs(&run.id)?;
        (stage, run, all)
    };
    Box::pin(persist(ctx, &mut table, stage, run, &all, outcome)).await
}

/// Settle one stage run while the caller holds the run table. A stage run
/// that is already terminal is left alone (events arrive at least once).
/// Completed outcomes must be verified outside the lock via `settle`;
/// spawn failure/cancellation paths can call this directly.
pub(super) async fn settle_locked(
    ctx: &DaemonContext,
    table: &mut RunTable,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(
        !matches!(outcome, StageOutcome::Completed { .. }),
        "completed outcomes must use unlocked verification"
    );
    let (stage, run, all, outcome, flagged) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        let run = store.run(&stage.run)?;
        let all = store.stage_runs(&run.id)?;
        let flagged = matches!(outcome, StageOutcome::Failed { infra: true, .. });
        let lost = stage.aether_agent.is_none() && lost_before_spawn(&run, &outcome);
        let outcome = blame_missing_workspace(&store, &stage, &run, outcome)?;
        (stage, run, all, outcome, flagged || lost)
    };
    if stage.state.is_terminal() {
        return Ok(FollowUp::default());
    }
    if let Some(follow) = infra_retry(ctx, table, &stage, &run, &all, &outcome, false).await? {
        return Ok(follow);
    }
    let outcome = recorded(outcome, flagged);
    // Cancellation also polls settlement deep inside a request's follow-up chain.
    Box::pin(persist(ctx, table, stage, run, &all, outcome)).await
}

/// A stage that failed with its workspace gone failed for that, whatever
/// its engine made of the missing directory, and no retry in it can pass;
/// the engine's own error follows. Other outcomes are returned as they are.
fn blame_missing_workspace(
    store: &archon_queue::Store,
    stage: &StageRun,
    run: &PipelineRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    assert_eq!(stage.run, run.id, "the stage belongs to the run");
    let StageOutcome::Failed { error, .. } = &outcome else {
        return Ok(outcome);
    };
    // A team slot works in its own worktree; a stage without a stored request, in the run's.
    let workspace = match spawn::load_spawn(store, &stage.id) {
        Ok(spawn) => spawn.request.workspace,
        Err(error) if error.is_store() => return Err(error),
        Err(_) => run.workspace.clone(),
    };
    assert!(workspace.is_absolute(), "run workspaces are absolute");
    if workspace.is_dir() || error.starts_with(spawn::WORKSPACE_MISSING) {
        return Ok(outcome);
    }
    let error = format!(
        "{} {}{}{error}",
        spawn::WORKSPACE_MISSING,
        workspace.display(),
        StageOutcome::ENGINE_SAID,
    );
    Ok(StageOutcome::Failed {
        error,
        infra: false,
    })
}

/// Whether a stage run's workspace was gone before its agent was created:
/// the spawn's own refusal of it, or any failure once the run's tree is gone.
/// A worker tree never made because preparing it was refused is not.
fn lost_before_spawn(run: &PipelineRun, outcome: &StageOutcome) -> bool {
    assert!(run.workspace.is_absolute(), "run workspaces are absolute");
    let StageOutcome::Failed { error, .. } = outcome else {
        return false;
    };
    error.starts_with(spawn::WORKSPACE_MISSING) || !run.workspace.is_dir()
}

/// `outcome` as it is recorded, after the retry decision read it: a failure
/// the machine caused (`flagged`) stays so though its workspace went.
fn recorded(outcome: StageOutcome, flagged: bool) -> StageOutcome {
    match outcome {
        StageOutcome::Failed { error, infra } => StageOutcome::Failed {
            error,
            infra: infra || flagged,
        },
        other => other,
    }
}

/// Record a final outcome and apply the driver's resulting decision.
async fn persist(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: StageRun,
    run: PipelineRun,
    all: &[StageRun],
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    let mut settled = settled_stage(stage, outcome, archon_core::now());
    if let Some(driver) = table.drivers.get(&run.id) {
        let holds = driver.holds_verdict(&settled.stage_id);
        unnote_missing_verdict(&mut settled, holds);
    }
    let (settled, owes_stop, kept) = {
        let mut store = ctx.store.lock().await;
        // A final restated verdict that is no pass stands over the result.
        let settled = restatement::bound(&store, settled)?;
        let kept = resume::after_settle(&store, &settled)?;
        let owes_stop = descendants::owe(&mut store, &settled, false)?;
        store.upsert_stage_run(&settled)?;
        restatement::forget(&mut store, &settled)?;
        (settled, owes_stop, kept)
    };
    table.spawn_errors.remove(&settled.id);
    let mut follow = decide(ctx, table, run, &settled, all).await?;
    follow.kept.extend(kept);
    follow.reap.push(settled.id.clone());
    if owes_stop {
        follow.descendant_stops.push(settled.id);
    }
    Ok(follow)
}

/// Drop the `missing_verdict_line` note of a stage whose contract asks for no
/// verdict (`holds`): it gave none because none was owed, so the note says nothing.
fn unnote_missing_verdict(settled: &mut StageRun, holds: bool) {
    assert!(settled.state.is_terminal(), "only an outcome is noted");
    if holds || settled.verdict_note != Some(VerdictNote::MissingVerdictLine) {
        return;
    }
    assert_eq!(
        settled.verdict,
        Some(Verdict::Unknown),
        "a note explains an unknown verdict"
    );
    settled.verdict_note = None;
}

/// Feed a stored settle to the run's driver and apply what it decides.
async fn decide(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    settled: &StageRun,
    all: &[StageRun],
) -> Result<FollowUp, ArchonError> {
    assert!(settled.state.is_terminal(), "the driver hears of settles");
    if run.state.is_terminal() {
        // It may have been the last of the finished run's agents.
        return Ok(FollowUp {
            discard: vec![run.id],
            ..FollowUp::poke()
        });
    }
    let (actions, publish) = match table.drivers.get_mut(&run.id) {
        // A gate stage's page is published when its question is asked.
        Some(driver) => (
            driver.on_stage_settled(settled, all),
            pages::lists_pages(settled) && driver.gates().subject(&settled.stage_id).is_none(),
        ),
        None => {
            // A driver is missing if a swarm plan just reshaped the run or an
            // earlier rebuild failed: rebuild it from the store, with this settle.
            let stages = ctx.store.lock().await.stage_runs(&run.id)?;
            return recover::resume_run(ctx, table, run, &stages).await;
        }
    };
    let mut follow = apply(ctx, table, run, actions).await?;
    if publish {
        follow.publish.push(settled.id.clone());
    }
    Ok(follow)
}

/// The terminal stage run for `outcome`.
pub(super) fn settled_stage(
    mut stage: StageRun,
    outcome: StageOutcome,
    now: Timestamp,
) -> StageRun {
    assert!(
        !stage.state.is_terminal(),
        "stage {} settled twice",
        stage.id
    );
    stage.state = match outcome {
        StageOutcome::Completed {
            summary,
            structured,
        } => {
            // A review-only lead's review record states its verdict when its result states none.
            let (verdict, note) = match stage.only_reviews {
                Some(true) => Verdict::read_review_lead(&summary, structured.as_ref()),
                _ => Verdict::read(&summary, structured.as_ref()),
            };
            stage.verdict = Some(verdict);
            stage.verdict_note = note;
            stage.summary = Some(summary);
            stage.structured = structured;
            StageState::Completed
        }
        StageOutcome::Failed { error, infra } => {
            stage.infrastructure_failure = infra;
            StageState::Failed { error }
        }
        StageOutcome::Cancelled { reason } => {
            stage.summary = reason;
            StageState::Cancelled
        }
        StageOutcome::TimedOut => StageState::TimedOut,
    };
    stage.finished_at = Some(now.max(stage.started_at));
    // A stage started by an earlier release is marked as its outcome lands.
    stage.record_repair.get_or_insert(RecordRepair::Open);
    debug_assert!(stage.state.is_terminal());
    stage
}

/// Re-spawns per slot for failures on a full disk. They wait for free space
/// and do not use up `limits.infra_retries` (ARCHITECTURE §6.3).
const MAX_DISK_FULL_RETRIES: usize = 5;

/// Wakes per slot of agents that failed on their engine's login. Each waits
/// for a probe to find the engine logged in, so only probes that wrongly
/// find it so spend them; none uses up `limits.infra_retries`.
const MAX_LOGIN_WAKES: usize = 48;

/// How long an infrastructure retry waits out a cause that clears with time.
const TRANSIENT_WAIT_MINUTES: i64 = 5;

/// What an infrastructure failure's `error` names that clears with time: an
/// engine or provider that is down or refusing for now. A failed login holds
/// its engine instead (`RetryKind::EngineLogin`).
fn transient_cause(error: &str) -> Option<&'static str> {
    // Aether words a model error `<Class>: <message>`, as in `RateLimit: slow down`.
    const CAUSES: [(&str, &str); 5] = [
        ("unavailable", "the engine to come back"),
        ("overloaded", "the provider to recover"),
        ("ratelimit:", "the rate limit to lift"),
        ("rate limit", "the rate limit to lift"),
        ("usagelimit:", "the usage limit to reset"),
    ];
    let error = error.to_ascii_lowercase();
    CAUSES
        .iter()
        .find(|(word, _)| error.contains(word))
        .map(|(_, wait)| *wait)
}

/// How many minutes an agent Aether stopped at its stage deadline is woken
/// for, once per slot, to commit what works and report.
pub(super) const DEADLINE_WAKE_MINUTES: i64 = 10;

/// What a stage stopped at its deadline failed on, as its retry tells it.
const DEADLINE_PASSED: &str = "stage deadline passed";

/// Why a failed stage is re-spawned; each kind has its own count per slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    Infra,
    DiskFull,
    /// A refused plan or proposal draft, sent back with the problems.
    Refused,
    /// An agent Aether stopped at its stage deadline, woken once to report.
    Timeout,
    /// An agent whose engine's login failed, woken once the engine works.
    EngineLogin,
}

impl RetryKind {
    /// Why `error` is retried, if it is. Only infrastructure (`infra`) is
    /// trusted to report a full disk: a failure the agent wrote is one only
    /// when the free space confirms it, else it is the agent's own.
    async fn of_failure(ctx: &DaemonContext, error: &str, infra: bool) -> Option<Self> {
        if infra && archon_core::engine_login::is_login_failure(error) {
            return Some(Self::EngineLogin);
        }
        if !disk::is_disk_full(error) {
            return infra.then_some(Self::Infra);
        }
        if infra || disk_nearly_full(ctx).await {
            return Some(Self::DiskFull);
        }
        tracing::info!(%error, "the agent reported a full disk, but there is room; not a disk-full failure");
        None
    }

    /// An earlier run of the slot, which a retry replaced. A replaced
    /// failure that says the disk is full was retried as one.
    fn of_replaced(stage: &StageRun, planner: bool) -> Self {
        match &stage.state {
            StageState::Failed { error }
                if !stage.infrastructure_failure
                    && ((planner && swarm::is_refusal(error))
                        || super::intent::is_refusal(error)) =>
            {
                Self::Refused
            }
            StageState::Failed { .. } if stage.failed_on_engine_login() => Self::EngineLogin,
            StageState::Failed { error } if disk::is_disk_full(error) => Self::DiskFull,
            StageState::TimedOut => Self::Timeout,
            _ => Self::Infra,
        }
    }

    fn limit(self, ctx: &DaemonContext) -> usize {
        match self {
            Self::Infra => ctx.config.limits.infra_retries as usize,
            Self::DiskFull => MAX_DISK_FULL_RETRIES,
            Self::Refused => archon_pipeline::swarm::MAX_CORRECTIONS,
            Self::Timeout => 1,
            Self::EngineLogin => MAX_LOGIN_WAKES,
        }
    }
}

/// Re-spawn the slot of a live run with its original plan
/// (`RunDriver::respawn`): `limits.infra_retries` times for infrastructure
/// failures, and for a failure on a full disk, which holds dispatch until
/// space is free. Refused plans and proposal drafts return the same way,
/// `MAX_CORRECTIONS` times. The failure never reaches the driver, so the slot keeps
/// its capacity. The first infrastructure replacement of a slot wakes the failed
/// agent in place, and only a refused wake spawns a fresh agent, the cold retry.
async fn infra_retry(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    run: &PipelineRun,
    all: &[StageRun],
    outcome: &StageOutcome,
    refused: bool,
) -> Result<Option<FollowUp>, ArchonError> {
    let StageOutcome::Failed { error, infra } = outcome else {
        return Ok(None);
    };
    let kind = match refused {
        true => Some(RetryKind::Refused),
        false => RetryKind::of_failure(ctx, error, *infra).await,
    };
    let Some(kind) = kind else {
        return Ok(None);
    };
    if kind == RetryKind::DiskFull {
        ctx.disk
            .report_full(error, FullDisk::Aether, Instant::now());
    }
    if kind == RetryKind::EngineLogin {
        engine_wait::hold(ctx, stage, error).await?;
    }
    respawn(ctx, table, (stage, run, all), outcome, error, kind).await
}

/// Settle stage `id`, whose agent Aether stopped at its deadline: the agent is
/// woken once per slot with [`DEADLINE_WAKE_MINUTES`] to commit what works and
/// report, the retry `limits.infra_retries` does not count; else it timed out.
pub(super) async fn deadline_stopped(
    ctx: &DaemonContext,
    id: &StageRunId,
) -> Result<FollowUp, ArchonError> {
    let mut table = ctx.runs.lock().await;
    let (stage, run, all, outcome) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        let run = store.run(&stage.run)?;
        let all = store.stage_runs(&run.id)?;
        // A stop archond marked, a stall or an escalation, settles as marked.
        let outcome = stall::intended(&store, id, StageOutcome::TimedOut)?;
        (stage, run, all, outcome)
    };
    if stage.state.is_terminal() {
        return Ok(FollowUp::default());
    }
    assert_eq!(stage.run, run.id, "a stage settles into its own run");
    if outcome == StageOutcome::TimedOut {
        let woken = respawn(
            ctx,
            &mut table,
            (&stage, &run, &all),
            &outcome,
            DEADLINE_PASSED,
            RetryKind::Timeout,
        )
        .await?;
        if let Some(follow) = woken {
            return Ok(follow);
        }
    }
    settle_locked(ctx, &mut table, id, outcome).await
}

/// Reserve the replacement of `stage`'s slot, which ended with `outcome` on
/// `error`, as the retry of `kind` it is; `None` when the slot has none left.
/// A run that took a deadline wake is the slot's last: what ends it is final.
async fn respawn(
    ctx: &DaemonContext,
    table: &mut RunTable,
    (stage, run, all): (&StageRun, &PipelineRun, &[StageRun]),
    outcome: &StageOutcome,
    error: &str,
    kind: RetryKind,
) -> Result<Option<FollowUp>, ArchonError> {
    assert!(
        matches!(
            (kind, outcome),
            (RetryKind::Timeout, StageOutcome::TimedOut) | (_, StageOutcome::Failed { .. })
        ),
        "a {kind:?} retry replaces a matching outcome, not {outcome:?}"
    );
    if run.state.is_terminal() || took_deadline_wake(&*ctx.store.lock().await, stage)? {
        return Ok(None);
    }
    let slot: Vec<&StageRun> = all
        .iter()
        .filter(|s| spawn::same_slot_as(s, stage))
        .collect();
    let planner = table
        .drivers
        .get(&run.id)
        .is_some_and(|d| d.spec().assigned_by(&stage.stage_id).is_some());
    let mut failed = settled_stage(stage.clone(), outcome.clone(), archon_core::now());
    // Not the model's failure, and it marks the agent as one a wake may take.
    failed.infrastructure_failure |= kind == RetryKind::Timeout;
    let (retry, wake) = slot_retry(ctx, &failed, &slot, kind, planner, error).await?;
    let limit = kind.limit(ctx);
    // A timed-out agent goes on only woken: a fresh one could not finish either.
    if retry > limit || (kind == RetryKind::Timeout && wake.is_none()) {
        return Ok(None);
    }
    let refusal = (kind == RetryKind::Refused).then_some((error, retry));
    let waits = passing_cause(kind, error);
    let not_before =
        waits.map(|_| archon_core::now() + chrono::Duration::minutes(TRANSIENT_WAIT_MINUTES));
    let extension =
        (kind == RetryKind::Timeout).then(|| chrono::Duration::minutes(DEADLINE_WAKE_MINUTES));
    let reserved = spawn::Retry {
        nth: slot.len(),
        not_before,
        wake: wake.clone(),
        extension,
        engine_login: kind == RetryKind::EngineLogin,
    };
    let replaced = replace(ctx, table, run, all, &failed, reserved, refusal).await?;
    let Some((replacement, woken)) = replaced else {
        return Ok(None);
    };
    assert!(!woken || wake.is_some(), "only a planned wake is kept");
    let text = retry_text(stage, run, error, kind, woken, waits, (retry, limit));
    progress(ctx, run, text).await?;
    // Issued at once, or by the sweep once a disk hold ends; in either
    // case after what the failed agent delegated is stopped and reaped.
    Ok(Some(FollowUp {
        issue: vec![replacement],
        descendant_stops: vec![stage.id.clone()],
        reap: vec![stage.id.clone()],
        poke: kind == RetryKind::DiskFull,
        ..FollowUp::default()
    }))
}

/// Whether `stage` ran on an agent woken at its stage deadline.
fn took_deadline_wake(store: &archon_queue::Store, stage: &StageRun) -> Result<bool, ArchonError> {
    match spawn::load_spawn(store, &stage.id) {
        Ok(spawn) => Ok(spawn.deadline_wake),
        Err(ArchonError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The cause that clears with time an infrastructure failure's `error`
/// names, which its retry waits out.
fn passing_cause(kind: RetryKind, error: &str) -> Option<&'static str> {
    match kind {
        RetryKind::Infra => transient_cause(error),
        RetryKind::DiskFull | RetryKind::Refused | RetryKind::Timeout | RetryKind::EngineLogin => {
            None
        }
    }
}

/// The feed line of a retry: what `stage` hit, how and when it is retried,
/// and which of the `count` retries this is.
fn retry_text(
    stage: &StageRun,
    run: &PipelineRun,
    error: &str,
    kind: RetryKind,
    woken: bool,
    waits: Option<&str>,
    (retry, limit): (usize, usize),
) -> String {
    assert!(retry >= 1 && retry <= limit, "a retry within its limit");
    let (what, how) = match (kind, woken) {
        (RetryKind::Infra, true) => ("an infrastructure failure", " in the same agent"),
        (RetryKind::Infra, false) => ("an infrastructure failure", ""),
        (RetryKind::DiskFull, _) => ("a full disk", " once disk space is free"),
        (RetryKind::Refused, _) => ("a refused stage result", ""),
        (RetryKind::Timeout, _) => (
            "its deadline",
            " in the same agent, which has a few minutes to commit and report",
        ),
        (RetryKind::EngineLogin, true) => (
            "a failed engine login",
            " in the same agent once its engine's login works again",
        ),
        (RetryKind::EngineLogin, false) => (
            "a failed engine login",
            " once its engine's login works again",
        ),
    };
    let when = waits.map_or(String::new(), |cause| {
        format!(" in {TRANSIENT_WAIT_MINUTES} min, waiting for {cause},")
    });
    let count = match (kind, woken) {
        (RetryKind::Timeout, _) => "it times out if Aether cannot wake it".to_owned(),
        (RetryKind::EngineLogin, _) => format!("no retry spent; wait {retry} of {limit}"),
        (_, true) => "a fresh agent if Aether cannot wake it".to_owned(),
        (_, false) => format!("{retry} of {limit}"),
    };
    let text = format!(
        "Stage `{}` of run {} hit {what} ({error}); retrying{how}{when} ({count}).",
        stage.stage_id, run.id,
    );
    assert!(text.contains(error), "the feed names the failure");
    text
}

/// The number of the retry `failed`'s replacement would be among the `slot`'s
/// retries of `kind`, and the agent it wakes. A replacement that woke its agent
/// spent no retry; one is tried per slot, while a cold retry would be allowed.
async fn slot_retry(
    ctx: &DaemonContext,
    failed: &StageRun,
    slot: &[&StageRun],
    kind: RetryKind,
    planner: bool,
    error: &str,
) -> Result<(usize, Option<spawn::KeptAgent>), ArchonError> {
    assert!(
        slot.iter().any(|s| s.id == failed.id),
        "the slot holds the failure"
    );
    let earlier = slot.iter().filter(|s| s.id != failed.id);
    let replaced = earlier
        .filter(|s| RetryKind::of_replaced(s, planner) == kind)
        .count();
    if kind == RetryKind::Timeout {
        let wake = (replaced < kind.limit(ctx)).then(|| resume::wake_at_deadline(failed));
        return Ok((1 + replaced, wake.flatten()));
    }
    // Each failed login wakes the same agent again, once its engine works.
    if kind == RetryKind::EngineLogin {
        return Ok((1 + replaced, resume::wake_for(failed, error)));
    }
    if kind != RetryKind::Infra {
        return Ok((1 + replaced, None));
    }
    let store = ctx.store.lock().await;
    let (tried, took) = resume::wakes(&store, slot)?;
    assert!(
        took <= replaced + 1,
        "each woken agent took over one failure"
    );
    let retry = 1 + replaced - took.min(replaced);
    let wake = match tried || retry > kind.limit(ctx) {
        true => None,
        false => resume::wake_for(failed, error),
    };
    Ok((retry, wake))
}

/// Whether a watched path has less free than a disk-full hold waits for,
/// which bears out a full disk an agent reported. An unreadable disk
/// bears out nothing: the failure is then the agent's own, as before.
async fn disk_nearly_full(ctx: &DaemonContext) -> bool {
    match crate::dispatcher::read_disk(ctx).await {
        Ok(reading) => reading.free_bytes < disk::DISK_FULL_RESUME_BYTES,
        Err(error) => {
            tracing::warn!(%error, "free disk space unknown; an agent's disk-full failure is its own");
            false
        }
    }
}

/// Reserve the replacement of `failed`, the `retry` of its slot, and then
/// persist the failure; with whether it still wakes the failed agent, or
/// `None` when the driver or the budget allows none.
async fn replace(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: &PipelineRun,
    all: &[StageRun],
    failed: &StageRun,
    retry: spawn::Retry,
    refusal: Option<(&str, usize)>,
) -> Result<Option<(StageRunId, bool)>, ArchonError> {
    assert!(
        failed.state.is_terminal(),
        "only a failed stage run is replaced"
    );
    assert!(retry.nth >= 1, "the slot holds the failed stage run");
    let now_all = replaced(all, failed);
    let driver = table.drivers.get(&run.id);
    let plan = driver.and_then(|driver| driver.respawn(failed, &now_all).zip(Some(driver)));
    let Some((mut plan, driver)) = plan else {
        return Ok(None);
    };
    if driver.spec().assigned_by(&plan.stage_id).is_some() {
        let cap = planner_cap(ctx, run).await?;
        swarm::brief(&mut plan, driver.spec(), cap, refusal);
    }
    if let Some((error, correction)) = refusal.filter(|(error, _)| super::intent::is_refusal(error))
    {
        plan.prompt.push_str(&format!(
            "\n\nYour proposal result was refused (correction {correction} of {}): {error}\n\
             Keep the completed proposal and images; fix the draft or files named above.",
            archon_pipeline::swarm::MAX_CORRECTIONS,
        ));
    }
    let spec = driver.spec().clone();
    let mut retry = retry;
    // A deadline wake goes on in the stopped agent, on the model it runs: nothing
    // is chosen here, and issuing it waits for a catalog that lists that model.
    let mut store = if retry.extension.is_some() {
        assert!(retry.wake.is_some(), "only a wake is extended");
        // The plan's choice may since have moved, or never been the agent's.
        assert!(
            failed.aether_agent.is_some(),
            "a wake keeps the stopped agent"
        );
        plan.choice = failed.current_choice().clone();
        ctx.store.lock().await
    } else {
        let catalog = crate::learned::catalog(ctx).await?;
        let mut store = ctx.store.lock().await;
        let read = ((run, &*spec), &catalog);
        plan = retry_choice(ctx, (&mut store, table), read, (failed, plan), &mut retry)?;
        store
    };
    // Owed before the replacement exists, so it never starts beside them.
    let woken = retry.wake.is_some();
    descendants::owe(&mut store, failed, woken)?;
    let Some(id) = spawn::reserve_replacement(ctx, &mut store, failed, &plan, retry)? else {
        return Ok(None);
    };
    // Replacement first: a crash in between leaves a superseded live
    // stage run, never a failure that looks final on resume.
    // The attempt goes on, so its restatement is kept for the replacement too.
    let failed = restatement::bound(&store, failed.clone())?;
    store.upsert_stage_run(&failed)?;
    Ok(Some((id, woken)))
}

/// The plan `failed`'s retry runs: a wake keeps the stopped agent on the
/// model it ran (I1) unless a steer, retirement or judge moves it; a fresh
/// retry is routed by quota as a fresh stage is (I2).
fn retry_choice(
    ctx: &DaemonContext,
    (store, table): (&mut archon_queue::Store, &mut RunTable),
    ((run, spec), catalog): ((&PipelineRun, &PipelineSpec), &ModelCatalog),
    (failed, chosen): (&StageRun, SpawnPlan),
    retry: &mut spawn::Retry,
) -> Result<SpawnPlan, ArchonError> {
    assert_eq!(failed.run, run.id, "the failed attempt is the run's");
    assert_eq!(
        failed.stage_id, chosen.stage_id,
        "the retry is the failed slot's"
    );
    let mut plan = super::model_catalog::retry_plan(ctx, store, table, run, chosen, catalog, spec)?;
    let ran = failed.current_choice();
    if retry.wake.is_some() {
        let item = store.item(&run.item)?;
        if let Some(held) = super::resume::held_on_agent(&item, &plan, ran, catalog) {
            plan.choice = held;
        }
    }
    // The agent a wake keeps runs the model it was last steered to.
    let kept = catalog.canonical_alias(&ran.model);
    let woken = retry.wake.is_some().then_some(kept);
    retry_judge(ctx, store, run, &mut plan, woken)?;
    if plan.choice.model != *kept {
        retry.wake = None;
    }
    if retry.wake.is_none() {
        route_fresh(ctx, (store, table), (run, spec), &mut plan, catalog)?;
    }
    Ok(plan)
}

/// Route fresh retry `plan` by quota, letting the run's driver follow a
/// stored plan the move changed, as a reserved stage's move is followed.
fn route_fresh(
    ctx: &DaemonContext,
    (store, table): (&mut archon_queue::Store, &mut RunTable),
    (run, spec): (&PipelineRun, &PipelineSpec),
    plan: &mut SpawnPlan,
    catalog: &ModelCatalog,
) -> Result<(), ArchonError> {
    let mut item = store.item(&run.item)?;
    assert_eq!(item.id, run.item, "the run works for the item");
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&crate::resolve::repo_ref(id)))
        .transpose()?;
    let before = item.model_plan.plan().clone();
    let resume = plan.resume.clone();
    crate::quota::route_replacement(ctx, store, (&mut item, spec), plan, repo.as_ref())?;
    assert_eq!(
        plan.resume, resume,
        "routing leaves what the attempt is told"
    );
    let after = item.model_plan.plan();
    if before != *after {
        super::model_catalog::follow_models(table, &run.id, (&before, after), catalog);
    }
    Ok(())
}

/// An acceptance retry is judged by the `woken` judge while it is still
/// eligible, else by one selected as its first attempt was, never the placeholder.
fn retry_judge(
    ctx: &DaemonContext,
    store: &mut archon_queue::Store,
    run: &PipelineRun,
    plan: &mut SpawnPlan,
    woken: Option<&archon_core::model::ModelRef>,
) -> Result<(), ArchonError> {
    if plan.stage_id != archon_pipeline::acceptance::STAGE {
        return Ok(());
    }
    let item = store.item(&run.item)?;
    let plans = std::slice::from_mut(plan);
    let selected = super::acceptance::select_model(ctx, store, &item, run, plans, woken);
    match selected {
        Ok(()) => {
            assert_eq!(plan.choice.tier, None, "a judge is selected, never tiered");
            Ok(())
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => Err(ArchonError::Unavailable(format!(
            "{}: {error}",
            super::model_catalog::RETRY_WAIT
        ))),
    }
}

pub(super) async fn replace_refused_model(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    reason: String,
) -> Result<FollowUp, ArchonError> {
    let (run, all) = {
        let store = ctx.store.lock().await;
        (store.run(&stage.run)?, store.stage_runs(&stage.run)?)
    };
    let error = format!("cannot create agent: {reason}");
    if run.state.is_terminal() {
        return settle_locked(
            ctx,
            table,
            &stage.id,
            StageOutcome::Cancelled {
                reason: Some(error),
            },
        )
        .await;
    }
    let outcome = StageOutcome::Failed {
        error: error.clone(),
        infra: true,
    };
    let failed = settled_stage(stage.clone(), outcome, archon_core::now());
    let retry = spawn::Retry {
        nth: all.iter().filter(|s| spawn::same_slot_as(s, stage)).count(),
        wake: None,
        not_before: None,
        extension: None,
        engine_login: false,
    };
    match replace(ctx, table, &run, &all, &failed, retry, None).await {
        Ok(Some((id, _))) => Ok(FollowUp {
            issue: vec![id],
            descendant_stops: vec![stage.id.clone()],
            ..FollowUp::default()
        }),
        // The model was refused, not the machine: no infrastructure failure.
        Ok(None) => spawn::fail_stage_and_run(ctx, table, stage, error, false).await,
        Err(ArchonError::Unavailable(why)) => {
            let subject = format!("stage:{}", stage.id);
            let what = format!(
                "Stage {} waits for a model; re-plan item {}",
                stage.stage_id, run.item
            );
            super::coordination::isolate(
                ctx,
                &subject,
                Some(&run.item),
                &what,
                Err(ArchonError::Invalid(why)),
            )
            .await?;
            Ok(FollowUp::default())
        }
        Err(error) => Err(error),
    }
}

/// The agent cap a replaced planner is told, from the repository as it is now.
async fn planner_cap(ctx: &DaemonContext, run: &PipelineRun) -> Result<u32, ArchonError> {
    let repo = ctx.store.lock().await.item(&run.item)?.repo;
    let env = RepoEnv::load(ctx, repo.as_ref()).await?;
    Ok(env.agent_cap(ctx, &run.pipeline))
}

/// `all` with `settled` in place of its earlier self.
fn replaced(all: &[StageRun], settled: &StageRun) -> Vec<StageRun> {
    let swapped: Vec<StageRun> = all
        .iter()
        .map(|s| {
            if s.id == settled.id {
                settled.clone()
            } else {
                s.clone()
            }
        })
        .collect();
    assert_eq!(
        swapped.len(),
        all.len(),
        "one stage run replaced, none added"
    );
    assert!(
        swapped.contains(settled),
        "the settled stage run is part of its run"
    );
    swapped
}

pub(super) async fn progress(
    ctx: &DaemonContext,
    run: &PipelineRun,
    text: String,
) -> Result<(), ArchonError> {
    let entry = FeedEntry::Notification(Notification {
        id: archon_core::id::NotificationId::new(),
        kind: NotificationKind::Progress,
        text,
        item: Some(run.item.clone()),
        run: Some(run.id.clone()),
        at: archon_core::now(),
    });
    crate::feed::append(ctx, entry).await.map(|_| ())
}

/// Execute driver actions. Team siblings of a failed role are cancelled
/// with their reason first, even when the run also finishes; other cancels
/// that come with a finish need no work, as finishing stops the run's
/// agents. A gate or a merge comes alone.
pub(super) async fn apply(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    actions: Vec<EngineAction>,
) -> Result<FollowUp, ArchonError> {
    check_actions(&actions);
    let siblings = sibling_cancels(table, &run, &actions);
    let mut follow = FollowUp::default();
    if !siblings.is_empty() {
        follow = cancel_siblings(ctx, table, &run, siblings).await?;
    }
    follow.merge(apply_decision(ctx, table, run, actions).await?);
    Ok(follow)
}

/// The cancels in `actions` that stop team siblings of a failed role.
/// Without a finish every cancel is one; with a finish, only those the
/// run's driver issued as such, so their reason is not lost to the finish.
fn sibling_cancels(
    table: &RunTable,
    run: &PipelineRun,
    actions: &[EngineAction],
) -> Vec<(StageRunId, String)> {
    let finishes = actions
        .iter()
        .any(|a| matches!(a, EngineAction::Finish { .. }));
    let driver = table.drivers.get(&run.id);
    let siblings: Vec<(StageRunId, String)> = actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::CancelStage { stage_run, reason }
                if !finishes || driver.is_some_and(|d| d.cancels_sibling(stage_run)) =>
            {
                Some((stage_run.clone(), reason.clone()))
            }
            _ => None,
        })
        .collect();
    debug_assert!(siblings.len() <= actions.len());
    siblings
}

/// Persist each live team sibling cancelled with its reason, so the
/// driver's decision holds on resume, with the stop its agent is owed.
async fn cancel_siblings(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: &PipelineRun,
    siblings: Vec<(StageRunId, String)>,
) -> Result<FollowUp, ArchonError> {
    assert!(!run.state.is_terminal(), "only a live run cancels siblings");
    assert!(!siblings.is_empty(), "called with siblings to cancel");
    let mut follow = FollowUp::poke();
    let mut cancelled: Vec<(StageRun, String)> = Vec::new();
    {
        let mut store = ctx.store.lock().await;
        let now = archon_core::now();
        for (id, reason) in siblings {
            let stage = store.stage_run(&id)?;
            assert_eq!(stage.run, run.id, "a sibling belongs to the same run");
            if stage.state.is_terminal() {
                continue;
            }
            // A spawn still in flight has its agent cancelled once recorded.
            if let Some(agent) = &stage.aether_agent {
                siblings::owe(&mut store, &id, agent, &reason)?;
                follow.sibling_stops.push(id.clone());
            }
            let outcome = StageOutcome::Cancelled {
                reason: Some(reason.clone()),
            };
            let settled = restatement::bound(&store, settled_stage(stage, outcome, now))?;
            store.upsert_stage_run(&settled)?;
            restatement::forget(&mut store, &settled)?;
            table.spawn_errors.remove(&id);
            cancelled.push((settled, reason));
        }
    }
    if let Some(text) = sibling_notice(table, run, &cancelled) {
        progress(ctx, run, text).await?;
    }
    Ok(follow)
}

/// One progress line naming the cancelled roles and why.
fn sibling_notice(
    table: &RunTable,
    run: &PipelineRun,
    cancelled: &[(StageRun, String)],
) -> Option<String> {
    let (first, reason) = cancelled.first()?;
    let roles = table
        .drivers
        .get(&run.id)
        .and_then(|driver| driver.team_roles(&first.stage_id));
    let names: Vec<String> = cancelled
        .iter()
        .map(
            |(stage, _)| match roles.and_then(|r| r.get(stage.fan_index as usize)) {
                Some(role) => format!("`{role}`"),
                None => format!("#{}", stage.fan_index + 1),
            },
        )
        .collect();
    Some(format!(
        "Run {}: role(s) {} {reason}.",
        run.id,
        names.join(", ")
    ))
}

async fn apply_decision(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    actions: Vec<EngineAction>,
) -> Result<FollowUp, ArchonError> {
    let mut spawns: Vec<SpawnPlan> = Vec::new();
    let mut finish_state: Option<RunState> = None;
    let mut wait = None;
    for action in actions {
        match action {
            EngineAction::Spawn(plan) => spawns.push(plan),
            EngineAction::CancelStage { .. } => {}
            EngineAction::Finish { state } => finish_state = Some(state),
            EngineAction::AwaitOperator(awaited) => {
                assert!(wait.is_none(), "a run waits on one gate at a time");
                wait = Some(awaited);
            }
            EngineAction::MergeTeam(plan) => return Ok(team::queue(table, &run, plan)),
            EngineAction::Ship(plan) => return Ok(ship::queue(table, &run, plan)),
        }
    }
    if let Some(wait) = wait {
        assert!(
            spawns.is_empty() && finish_state.is_none(),
            "a waiting run acts on nothing else"
        );
        return open_gate(ctx, table, run, wait).await;
    }
    if let Some(state) = finish_state {
        assert!(spawns.is_empty(), "a finishing driver spawns nothing");
        return finish::finish_run(ctx, table, run, state, false).await;
    }
    if !spawns.is_empty() {
        queue_spawns(ctx, table, run, spawns).await?;
    }
    Ok(FollowUp::poke())
}

/// A finish comes at most once, cancels never with a gate, and a team merge
/// or a delivery alone.
fn check_actions(actions: &[EngineAction]) {
    let finishes = actions
        .iter()
        .filter(|a| matches!(a, EngineAction::Finish { .. }))
        .count();
    let cancels = actions
        .iter()
        .any(|a| matches!(a, EngineAction::CancelStage { .. }));
    let waits = actions
        .iter()
        .any(|a| matches!(a, EngineAction::AwaitOperator(_)));
    assert!(finishes <= 1, "a driver finishes a run once");
    assert!(
        !cancels || !waits,
        "stage cancels come with a finish or a team's next spawns"
    );
    let alone = |a: &EngineAction| matches!(a, EngineAction::MergeTeam(_) | EngineAction::Ship(_));
    assert!(
        !actions.iter().any(alone) || actions.len() == 1,
        "a team merge or a delivery comes alone"
    );
}

/// Record the stage the run is now in and queue its spawns for admission.
async fn queue_spawns(
    ctx: &DaemonContext,
    table: &mut RunTable,
    mut run: PipelineRun,
    spawns: Vec<SpawnPlan>,
) -> Result<(), ArchonError> {
    let first = &spawns[0];
    assert!(
        spawns
            .iter()
            .all(|s| s.stage_id == first.stage_id && s.attempt == first.attempt),
        "a driver emits one stage group at a time"
    );
    let now_in = RunState::Running {
        stage_id: first.stage_id.clone(),
        attempt: first.attempt,
    };
    if run.state != now_in {
        run.state = now_in;
        ctx.store.lock().await.update_run(&run)?;
    }
    let queue = table.pending.entry(run.id.clone()).or_default();
    for plan in spawns {
        let queued = queue.iter().any(|q| {
            (q.stage_id.as_str(), q.attempt, q.fan_index)
                == (plan.stage_id.as_str(), plan.attempt, plan.fan_index)
        });
        if !queued {
            queue.push(plan);
        }
    }
    Ok(())
}

async fn open_gate(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    wait: archon_pipeline::OperatorWait,
) -> Result<FollowUp, ArchonError> {
    let stage_id = wait.stage_id.clone();
    let item = ctx.store.lock().await.item(&run.item)?;
    let intent_gate = wait.subject == archon_core::run::GateSubject::Proposal
        && (item.intent.is_some()
            || item
                .model_plan
                .0
                .contains_key(archon_pipeline::acceptance::STAGE));
    match gates::open(ctx, run.clone(), wait).await {
        Err(error) if intent_gate && !error.is_store() => {
            let state = RunState::Failed {
                stage_id,
                error: format!("approved proposal cannot be frozen: {error}"),
            };
            finish::finish_run(ctx, table, run, state, false).await
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(stage: &str) -> StageRun {
        let now = archon_core::now();
        StageRun {
            only_reviews: None,
            id: StageRunId::new(),
            run: archon_core::id::PipelineRunId::new(),
            stage_id: stage.to_owned(),
            kind: None,
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            record_repair: None,
            host_context: Default::default(),
            choice: archon_core::model::ModelChoice {
                model: "claude-code/claude-sonnet-5".parse().unwrap(),
                effort: archon_core::model::Effort::Medium,
                tier: None,
            },
            aether_agent: None,
            aether_task: None,
            state: StageState::Running,
            verdict: None,
            verdict_note: None,
            sizing: Default::default(),
            restatement: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at: now,
            finished_at: None,
        }
    }

    /// A `direct` implement owes no verdict line, so its result's lack of one
    /// is not noted; a stage whose contract asks for one keeps the note.
    #[test]
    fn only_a_stage_that_owes_a_verdict_notes_a_missing_one() {
        let done = || StageOutcome::Completed {
            summary: "Implemented it.".to_owned(),
            structured: Some(serde_json::json!({"review": {"verdict": "pass"}})),
        };
        let now = archon_core::now();
        let mut ungated = settled_stage(running("implement"), done(), now);
        assert_eq!(ungated.verdict_note, Some(VerdictNote::MissingVerdictLine));
        unnote_missing_verdict(&mut ungated, false);
        assert_eq!(
            (ungated.verdict, ungated.verdict_note),
            (Some(Verdict::Unknown), None)
        );
        let mut gated = settled_stage(running("review"), done(), now);
        unnote_missing_verdict(&mut gated, true);
        assert_eq!(gated.verdict_note, Some(VerdictNote::MissingVerdictLine));
        let hedged = StageOutcome::Completed {
            summary: "Mostly fine.\nVERDICT: PASS or FAIL".to_owned(),
            structured: None,
        };
        let mut other = settled_stage(running("implement"), hedged, now);
        let note = other.verdict_note;
        assert_ne!(note, Some(VerdictNote::MissingVerdictLine), "{note:?}");
        unnote_missing_verdict(&mut other, false);
        assert_eq!(other.verdict_note, note, "only a missing line is dropped");
    }

    #[test]
    fn only_a_cause_that_clears_with_time_is_waited_out() {
        let login = "model error from claude-code/opus: Auth: token expired";
        assert_eq!(transient_cause(login), None, "a login holds its engine");
        let limit = "model error: RateLimit: slow down";
        assert_eq!(transient_cause(limit), Some("the rate limit to lift"));
        assert_eq!(
            transient_cause("engine unavailable"),
            Some("the engine to come back")
        );
        assert_eq!(transient_cause("engine died"), None);
        assert_eq!(transient_cause("the commit author is unknown"), None);
    }
}
