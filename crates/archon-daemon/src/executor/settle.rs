//! Settling stage runs: infra retries, persisting outcomes and verdicts,
//! and applying the driver's actions.

use std::time::Instant;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, StageState, Verdict};
use archon_core::steer::RunEdit;
use archon_pipeline::{EngineAction, SpawnPlan};

use super::{
    FollowUp, RunTable, descendants, finish, gates, pages, recover, resume, ship, siblings, spawn,
    stall, swarm, team,
};
use crate::context::DaemonContext;
use crate::disk::{self, FullDisk};
use crate::resolve::RepoEnv;

pub(super) async fn settle(
    ctx: &DaemonContext,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    if !matches!(outcome, StageOutcome::Completed { .. }) {
        let mut table = ctx.runs.lock().await;
        let outcome = stall::intended(&*ctx.store.lock().await, id, outcome)?;
        return settle_locked(ctx, &mut table, id, outcome).await;
    }
    let (stage, run, review, daemon_ships, planner, record_stage) = {
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
        (stage, run, review, daemon_ships, planner, record_stage)
    };
    // Leave the stage live until verification is persisted. If interrupted,
    // reconnect reconciliation reads the task outcome and verifies it again.
    let outcome = ship::verify(ctx, &run, &stage, daemon_ships, outcome).await?;
    let outcome = match (review.records_ship, &record_stage) {
        (true, _) => ship::verify_no_change(ctx, &run, outcome).await?,
        (false, Some(record)) => {
            ship::verify_standing_no_change(ctx, &run, record, &stage, outcome).await?
        }
        (false, None) => outcome,
    };
    let outcome = super::review::verify(ctx, &run, &stage, review, outcome).await?;
    let planned = planner.is_some();
    let was_completed = matches!(outcome, StageOutcome::Completed { .. });
    let (outcome, assigned) = swarm::verify(ctx, &run, &stage, planner, outcome).await?;
    // Only the swarm check turns a completed planner into a refusal.
    let refused = planned && was_completed && matches!(outcome, StageOutcome::Failed { .. });
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
    if refused
        && let Some(follow) = infra_retry(ctx, &table, &stage, &run, &all, &outcome, true).await?
    {
        return Ok(follow);
    }
    if let Some(edit) = assigned {
        assign_roles(ctx, &mut table, &run, edit).await?;
    }
    // Other verification failures are final stage outcomes, not infrastructure
    // failures from Aether: they must not enter the stage's retry path.
    persist(ctx, &mut table, stage, run, &all, outcome).await
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
    let (stage, run, all, outcome) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        let run = store.run(&stage.run)?;
        let all = store.stage_runs(&run.id)?;
        let outcome = blame_missing_workspace(&store, &stage, &run, outcome)?;
        (stage, run, all, outcome)
    };
    if stage.state.is_terminal() {
        return Ok(FollowUp::default());
    }
    if let Some(follow) = infra_retry(ctx, table, &stage, &run, &all, &outcome, false).await? {
        return Ok(follow);
    }
    persist(ctx, table, stage, run, &all, outcome).await
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
        "{} {}; the engine said: {error}",
        spawn::WORKSPACE_MISSING,
        workspace.display()
    );
    Ok(StageOutcome::Failed {
        error,
        infra: false,
    })
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
    /// A planner whose plan Archon refused, sent back with the problems.
    Refused,
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
    fn of_replaced(stage: &StageRun, planner: bool) -> Self {
        match &stage.state {
            StageState::Failed { error }
                if planner && !stage.infrastructure_failure && swarm::is_refusal(error) =>
            {
                Self::Refused
            }
            StageState::Failed { error } if disk::is_disk_full(error) => Self::DiskFull,
            _ => Self::Infra,
        }
    }

    fn limit(self, ctx: &DaemonContext) -> usize {
        match self {
            Self::Infra => ctx.config.limits.infra_retries as usize,
            Self::DiskFull => MAX_DISK_FULL_RETRIES,
            Self::Refused => archon_pipeline::swarm::MAX_CORRECTIONS,
        }
    }
}

/// Re-spawn the slot of a live run with its original plan
/// (`RunDriver::respawn`): `limits.infra_retries` times for infrastructure
/// failures, and for a failure on a full disk, which holds dispatch until
/// space is free. A planner whose plan Archon `refused` is sent back the
/// same way, `MAX_CORRECTIONS` times. The failure never reaches the driver, so the slot keeps
/// its capacity.
async fn infra_retry(
    ctx: &DaemonContext,
    table: &RunTable,
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
    let planner = table
        .drivers
        .get(&run.id)
        .is_some_and(|d| d.spec().assigned_by(&stage.stage_id).is_some());
    let earlier = slot.iter().filter(|s| s.id != stage.id);
    let retry = 1 + earlier
        .filter(|s| RetryKind::of_replaced(s, planner) == kind)
        .count();
    let limit = kind.limit(ctx);
    if retry > limit {
        return Ok(None);
    }
    let failed = settled_stage(stage.clone(), outcome.clone(), archon_core::now());
    let refusal = (kind == RetryKind::Refused).then_some((error.as_str(), retry));
    let Some(replacement) = replace(ctx, table, run, all, &failed, slot.len(), refusal).await?
    else {
        return Ok(None);
    };
    let (what, when) = match kind {
        RetryKind::Infra => ("an infrastructure failure", ""),
        RetryKind::DiskFull => ("a full disk", " once disk space is free"),
        RetryKind::Refused => ("a refused swarm plan", ""),
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
    refusal: Option<(&str, usize)>,
) -> Result<Option<StageRunId>, ArchonError> {
    assert!(
        failed.state.is_terminal(),
        "only a failed stage run is replaced"
    );
    assert!(nth >= 1, "the slot holds the failed stage run");
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
    let mut store = ctx.store.lock().await;
    // Owed before the replacement exists, so it never starts beside them.
    descendants::owe(&mut store, failed)?;
    let Some(id) = spawn::reserve_replacement(ctx, &mut store, failed, &plan, nth)? else {
        return Ok(None);
    };
    // Replacement first: a crash in between leaves a superseded live
    // stage run, never a failure that looks final on resume.
    store.upsert_stage_run(failed)?;
    Ok(Some(id))
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
