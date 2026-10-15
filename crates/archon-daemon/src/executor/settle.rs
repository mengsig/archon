//! Settling stage runs: infra retries, persisting outcomes and verdicts,
//! and applying the driver's actions.

use std::time::Instant;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, StageState, Verdict};
use archon_pipeline::{EngineAction, SpawnPlan};

use super::{
    FollowUp, RunTable, descendants, finish, gates, pages, recover, resume, ship, siblings, spawn,
    team,
};
use crate::context::DaemonContext;
use crate::disk::{self, FullDisk};
use crate::resolve;

pub(super) async fn settle(
    ctx: &DaemonContext,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    if !matches!(outcome, StageOutcome::Completed { .. }) {
        let mut table = ctx.runs.lock().await;
        return settle_locked(ctx, &mut table, id, outcome).await;
    }
    let (stage, run, review, daemon_ships) = {
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
        (stage, run, review, daemon_ships)
    };
    // Leave the stage live until verification is persisted. If interrupted,
    // reconnect reconciliation reads the task outcome and verifies it again.
    let outcome = ship::verify(ctx, &run, &stage, daemon_ships, outcome).await?;
    let outcome = super::review::verify(ctx, &run, &stage, review, outcome).await?;
    let mut table = ctx.runs.lock().await;
    let (stage, run, all) = {
        let store = ctx.store.lock().await;
        let current_stage = store.stage_run(id)?;
        let current_run = store.run(&stage.run)?;
        if current_stage.state != stage.state || current_run.state != run.state {
            // Cancellation, another settle or a driver decision won the race.
            // Its outcome takes precedence over this verification result.
            return Ok(FollowUp::poke());
        }
        let all = store.stage_runs(&current_run.id)?;
        assert_eq!(
            current_stage.run, current_run.id,
            "a stage settles into its own run"
        );
        assert!(
            all.iter().any(|s| s.id == current_stage.id),
            "the run lists the stage it settles"
        );
        (current_stage, current_run, all)
    };
    // Verification failures are final stage outcomes, not infrastructure
    // failures from Aether: they must not enter the stage's retry path.
    persist(ctx, &mut table, stage, run, &all, outcome).await
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
    let (stage, run, all) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        let run = store.run(&stage.run)?;
        let all = store.stage_runs(&run.id)?;
        (stage, run, all)
    };
    if stage.state.is_terminal() {
        return Ok(FollowUp::default());
    }
    if let Some(follow) = infra_retry(ctx, table, &stage, &run, &all, &outcome).await? {
        return Ok(follow);
    }
    persist(ctx, table, stage, run, &all, outcome).await
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
    let settled = settled_stage(stage, outcome, archon_core::now());
    let (owes_stop, kept) = {
        let mut store = ctx.store.lock().await;
        let kept = resume::after_settle(&store, &settled)?;
        let owes_stop = descendants::owe(&mut store, &settled)?;
        store.upsert_stage_run(&settled)?;
        (owes_stop, kept)
    };
    table.spawn_errors.remove(&settled.id);
    let mut follow = decide(ctx, table, run, &settled, all).await?;
    follow.kept.extend(kept);
    if owes_stop {
        follow.descendant_stops.push(settled.id);
    }
    Ok(follow)
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
            // A driver is missing only if an earlier rebuild failed; retry it
            // from the store, which already holds this settle.
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
            stage.verdict = Some(Verdict::extract(&summary, structured.as_ref()));
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
    debug_assert!(stage.state.is_terminal());
    stage
}

/// Re-spawns per slot for failures on a full disk. They wait for free space
/// and do not use up `limits.infra_retries` (ARCHITECTURE §6.3).
const MAX_DISK_FULL_RETRIES: usize = 5;

/// Why a failed stage is re-spawned; each kind has its own count per slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    Infra,
    DiskFull,
}

impl RetryKind {
    /// Why `error` is retried, if it is. Only infrastructure (`infra`) is
    /// trusted to report a full disk: a failure the agent wrote is one only
    /// when the free space confirms it, else it is the agent's own.
    async fn of_failure(ctx: &DaemonContext, error: &str, infra: bool) -> Option<Self> {
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
    fn of_replaced(stage: &StageRun) -> Self {
        match &stage.state {
            StageState::Failed { error } if disk::is_disk_full(error) => Self::DiskFull,
            _ => Self::Infra,
        }
    }

    fn limit(self, ctx: &DaemonContext) -> usize {
        match self {
            Self::Infra => ctx.config.limits.infra_retries as usize,
            Self::DiskFull => MAX_DISK_FULL_RETRIES,
        }
    }
}

/// Re-spawn the slot of a live run with its original plan
/// (`RunDriver::respawn`): `limits.infra_retries` times for infrastructure
/// failures, and for a failure on a full disk, which holds dispatch until
/// space is free. The failure never reaches the driver, so the slot keeps
/// its capacity.
async fn infra_retry(
    ctx: &DaemonContext,
    table: &RunTable,
    stage: &StageRun,
    run: &PipelineRun,
    all: &[StageRun],
    outcome: &StageOutcome,
) -> Result<Option<FollowUp>, ArchonError> {
    let StageOutcome::Failed { error, infra } = outcome else {
        return Ok(None);
    };
    let Some(kind) = RetryKind::of_failure(ctx, error, *infra).await else {
        return Ok(None);
    };
    if kind == RetryKind::DiskFull {
        ctx.disk
            .report_full(error, FullDisk::Aether, Instant::now());
    }
    if run.state.is_terminal() {
        return Ok(None);
    }
    let slot: Vec<&StageRun> = all
        .iter()
        .filter(|s| spawn::same_slot_as(s, stage))
        .collect();
    assert!(
        slot.iter().any(|s| s.id == stage.id),
        "the failed stage run belongs to its run"
    );
    let earlier = slot.iter().filter(|s| s.id != stage.id);
    let retry = 1 + earlier
        .filter(|s| RetryKind::of_replaced(s) == kind)
        .count();
    let limit = kind.limit(ctx);
    if retry > limit {
        return Ok(None);
    }
    let failed = settled_stage(stage.clone(), outcome.clone(), archon_core::now());
    let Some(replacement) = replace(ctx, table, run, all, &failed, slot.len()).await? else {
        return Ok(None);
    };
    let (what, when) = match kind {
        RetryKind::Infra => ("an infrastructure failure", ""),
        RetryKind::DiskFull => ("a full disk", " once disk space is free"),
    };
    let text = format!(
        "Stage `{}` of run {} hit {what} ({error}); retrying{when} ({retry} of {limit}).",
        stage.stage_id, run.id,
    );
    progress(ctx, run, text).await?;
    // Issued at once, or by the sweep once a disk hold ends; in either
    // case after what the failed agent delegated is stopped.
    Ok(Some(FollowUp {
        issue: vec![replacement],
        descendant_stops: vec![stage.id.clone()],
        poke: kind == RetryKind::DiskFull,
        ..FollowUp::default()
    }))
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

/// Reserve the replacement of `failed`, the `nth` run of its slot, and then
/// persist the failure; `None` when the driver or the budget allows none.
async fn replace(
    ctx: &DaemonContext,
    table: &RunTable,
    run: &PipelineRun,
    all: &[StageRun],
    failed: &StageRun,
    nth: usize,
) -> Result<Option<StageRunId>, ArchonError> {
    assert!(
        failed.state.is_terminal(),
        "only a failed stage run is replaced"
    );
    assert!(nth >= 1, "the slot holds the failed stage run");
    let now_all = replaced(all, failed);
    let plan = table
        .drivers
        .get(&run.id)
        .and_then(|driver| driver.respawn(failed, &now_all));
    let Some(plan) = plan else {
        return Ok(None);
    };
    let caps = resolve::caps(ctx).await;
    let mut store = ctx.store.lock().await;
    // Owed before the replacement exists, so it never starts beside them.
    descendants::owe(&mut store, failed)?;
    let Some(id) = spawn::reserve_replacement(ctx, &mut store, failed, &plan, nth, &caps)? else {
        return Ok(None);
    };
    // Replacement first: a crash in between leaves a superseded live
    // stage run, never a failure that looks final on resume.
    store.upsert_stage_run(failed)?;
    Ok(Some(id))
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
            let settled = settled_stage(stage, outcome, now);
            store.upsert_stage_run(&settled)?;
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
        return gates::open(ctx, run, wait).await;
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
