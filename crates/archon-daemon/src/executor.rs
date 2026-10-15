//! Executes pipeline engine actions (ARCHITECTURE §5.1, §6).
//! Holds one `RunDriver` per active run; persists before every Aether call
//! (`StageRun{Spawning}` first) so a crash is always retryable under the
//! stage run's idempotency key.
//!
//! Locking: `ctx.runs` is taken before `ctx.store`, and neither is held
//! across an Aether call. Work that needs Aether or git after a state change
//! is returned as a `FollowUp` and run once the run table is released.

pub mod coordination;
mod dependencies;
mod descendants;
mod finish;
pub(crate) mod frozen;
mod gate_news;
mod gates;
pub(crate) mod gh;
pub(crate) mod initiative_ship;
mod lanes;
mod naming;
mod pages;
pub mod pull_requests;
mod recover;
pub(crate) mod refresh;
pub(crate) mod repo_identity;
pub(crate) mod restack;
mod resume;
mod review;
mod revive;
mod roster;
mod run_node;
mod settle;
mod ship;
mod siblings;
mod spawn;
mod start;
pub(crate) mod steer;
mod sweep;
mod team;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::queue::QueueItem;
use archon_core::run::{GateDecision, OperatorGate, StageOutcome};
use archon_pipeline::team::MergePlan;
use archon_pipeline::{RunDriver, ShipPlan, SpawnPlan};

use crate::context::DaemonContext;

pub(crate) use finish::notify;
pub(crate) use recover::prune_finished_run_vars;
pub(crate) use revive::revived;
pub(crate) use roster::prune_settled_rosters;
pub(crate) use run_node::recover_message_node;
pub(crate) use run_node::{is_node_agent, node_run, prune_retired_node_agents};
pub(crate) use ship::{prune_finished_ships, shipped_commit};
pub(crate) use spawn::{load_spawn, prune_settled_requests, runs_awaiting_spawn};
pub(crate) use start::held_by_stack;
pub(crate) use sweep::sweep_live_stages;
pub(crate) use team::prune_finished_merges;

/// In-memory drivers and the spawns they are still owed.
#[derive(Debug, Default)]
pub struct RunTable {
    pub drivers: BTreeMap<PipelineRunId, RunDriver>,
    /// Emitted by drivers, not yet admitted by the dispatcher.
    pub pending: BTreeMap<PipelineRunId, Vec<SpawnPlan>>,
    /// Stage runs whose `create_agent` is being issued right now; nobody
    /// else issues them meanwhile.
    pub in_flight: BTreeSet<StageRunId>,
    /// `create_agent` failures per spawning stage run, for a bounded retry
    /// before the stage fails.
    pub spawn_errors: BTreeMap<StageRunId, SpawnErrors>,
    /// Team merges drivers asked for, not yet recorded.
    pub merges: BTreeMap<PipelineRunId, MergePlan>,
    /// Runs whose team merge is running git right now.
    pub merging: BTreeSet<PipelineRunId>,
    /// Deliveries drivers asked archond for, not yet recorded.
    pub ships: BTreeMap<PipelineRunId, ShipPlan>,
    /// Runs whose delivery is running git or `gh` right now.
    pub shipping: BTreeSet<PipelineRunId>,
    /// Deliveries that failed in a way another try may pass, and their wait.
    pub ship_retries: BTreeMap<PipelineRunId, ShipRetry>,
}

/// The failed tries of one run's delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShipRetry {
    pub failures: u32,
    /// The next try waits for this.
    pub not_before: std::time::Instant,
}

/// The failed `create_agent` calls of one spawning stage run, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpawnErrors {
    /// Transport failures (`Protocol`, `Io`) and Aether `internal` errors.
    pub transport: u32,
    /// Failures on a full disk, each re-issued once a disk hold ends.
    pub disk_full: u32,
}

/// A worktree to remove: its run succeeded, or its start was abandoned; or
/// a snapshot, whose run ended however it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cleanup {
    pub item: QueueItemId,
    pub repo_path: PathBuf,
    pub worktree: PathBuf,
    /// A `repo-readonly` run's snapshot, removed with `--force`.
    pub snapshot: bool,
}

/// An item's cargo target directory, owed deletion when the item ends.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ItemBuild {
    pub item: QueueItemId,
    pub repo_name: String,
}

/// Aether and git work owed after a state change, run without the run
/// table lock by [`run_follow_up`].
#[derive(Debug, Default)]
#[must_use]
pub(crate) struct FollowUp {
    /// `spawning` stage runs to issue `create_agent` for.
    pub issue: Vec<StageRunId>,
    /// Freshly spawned stage runs whose task may have settled before the
    /// stage run recorded its agent (a settle event would have been missed).
    pub check: Vec<(StageRunId, AetherTaskId)>,
    /// Finished runs whose live stage agents must be stopped.
    pub stop: Vec<PipelineRunId>,
    /// `running` stage runs past their deadline.
    pub time_out: Vec<StageRunId>,
    /// Finished runs, with no live stage left, whose run node must be
    /// cancelled and forgotten.
    pub retire: Vec<PipelineRunId>,
    /// Agents that belong to no live stage run.
    pub cancel_agents: Vec<(AetherAgentId, String)>,
    /// Team roles cancelled for a failed sibling whose agent stop is owed.
    pub sibling_stops: Vec<StageRunId>,
    /// Settled stage runs of delegating agents whose descendants to stop.
    pub descendant_stops: Vec<StageRunId>,
    /// Runs whose team roster may be owed to their running roles.
    pub roster: Vec<PipelineRunId>,
    pub cleanup: Vec<Cleanup>,
    /// Items that ended, whose own cargo target directory to delete once
    /// their agents are stopped.
    pub release_builds: Vec<ItemBuild>,
    /// Freshly opened operator gates to show and ask about.
    pub gates: Vec<gates::GateKey>,
    /// Completed stage runs whose listed reports to publish.
    pub publish: Vec<StageRunId>,
    /// Runs whose queued team merge to run.
    pub merge: Vec<PipelineRunId>,
    /// Runs whose queued delivery to make.
    pub ship: Vec<PipelineRunId>,
    /// Attempts resumed on a kept agent, which may have answered (§6.7).
    pub resumed: Vec<StageRunId>,
    /// Kept agents to park or end now that an attempt of theirs settled.
    pub kept: Vec<resume::Kept>,
    /// Live stage runs a steer still owes a patch or a message (§6.6).
    pub steers: Vec<StageRunId>,
    /// Capacity or pending spawns changed: wake the dispatcher.
    pub poke: bool,
}

impl FollowUp {
    pub fn poke() -> Self {
        Self {
            poke: true,
            ..Self::default()
        }
    }

    pub fn merge(&mut self, other: FollowUp) {
        self.issue.extend(other.issue);
        self.check.extend(other.check);
        self.stop.extend(other.stop);
        self.time_out.extend(other.time_out);
        self.retire.extend(other.retire);
        self.cancel_agents.extend(other.cancel_agents);
        self.sibling_stops.extend(other.sibling_stops);
        self.descendant_stops.extend(other.descendant_stops);
        self.roster.extend(other.roster);
        self.cleanup.extend(other.cleanup);
        self.release_builds.extend(other.release_builds);
        self.gates.extend(other.gates);
        self.publish.extend(other.publish);
        self.merge.extend(other.merge);
        self.ship.extend(other.ship);
        self.resumed.extend(other.resumed);
        self.kept.extend(other.kept);
        self.steers.extend(other.steers);
        self.poke |= other.poke;
    }

    fn is_empty(&self) -> bool {
        self.issue.is_empty()
            && self.check.is_empty()
            && self.stop.is_empty()
            && self.time_out.is_empty()
            && self.retire.is_empty()
            && self.cancel_agents.is_empty()
            && self.sibling_stops.is_empty()
            && self.descendant_stops.is_empty()
            && self.roster.is_empty()
            && self.cleanup.is_empty()
            && self.release_builds.is_empty()
            && self.gates.is_empty()
            && self.publish.is_empty()
            && self.merge.is_empty()
            && self.ship.is_empty()
            && self.resumed.is_empty()
            && self.kept.is_empty()
            && self.steers.is_empty()
    }
}

/// `queued → dispatching`: resolve the pipeline and repo, create the
/// worktree, insert the run, build its driver, queue its first spawns,
/// `dispatching → running`. Any failure moves the item to `failed` with a
/// `Blocker` feed entry.
pub async fn start_item(ctx: &DaemonContext, item: &QueueItemId) -> Result<(), ArchonError> {
    let follow = start::start_item(ctx, item).await?;
    run_follow_up(ctx, follow).await
}

/// Start `items` as [`start_item`] does, each repository's git work in order and
/// `lanes::MAX_LANES` repositories at once; a failure stops no other, the first is returned.
pub async fn start_items(ctx: &DaemonContext, items: &[QueueItemId]) -> Result<(), ArchonError> {
    let by_repo = items_by_repository(ctx, items).await?;
    let started = lanes::run(by_repo, |id| async move {
        (id, start::start_item(ctx, id).await)
    })
    .await;
    assert_eq!(started.len(), items.len(), "every item is started once");
    let mut follow = FollowUp::default();
    let mut first_error = None;
    for (id, result) in started {
        match result {
            Ok(more) => follow.merge(more),
            Err(error) => {
                tracing::error!(item = %id, %error, "starting item failed");
                first_error.get_or_insert(error);
            }
        }
    }
    let followed = run_follow_up(ctx, follow).await;
    match (first_error, followed) {
        (Some(error), Err(later)) => {
            tracing::error!(error = %later, "following up started items failed");
            Err(error)
        }
        (Some(error), Ok(())) => Err(error),
        (None, followed) => followed,
    }
}

/// `items` in lanes by repository; one without a repository runs no git
/// and has a lane of its own.
async fn items_by_repository<'a>(
    ctx: &DaemonContext,
    items: &'a [QueueItemId],
) -> Result<BTreeMap<String, Vec<&'a QueueItemId>>, ArchonError> {
    let store = ctx.store.lock().await;
    let mut keyed = Vec::with_capacity(items.len());
    for id in items {
        let lane = match store.item(id)?.repo {
            Some(repo) => repo.as_str().to_owned(),
            None => id.as_str().to_owned(),
        };
        keyed.push((lane, id));
    }
    let lanes = lanes::group(keyed);
    assert_eq!(lanes.values().map(Vec::len).sum::<usize>(), items.len());
    Ok(lanes)
}

/// Start up to `slots` pending spawns of `run` (dispatcher-admitted).
pub async fn spawn_admitted(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    slots: u32,
) -> Result<(), ArchonError> {
    let follow = {
        let mut table = ctx.runs.lock().await;
        spawn::reserve(ctx, &mut table, run, slots).await?
    };
    run_follow_up(ctx, follow).await
}

/// Reserve admitted spawns while the caller holds the run table, so the
/// dispatcher's snapshot and its reservations are one atomic step.
pub(crate) async fn reserve(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: &PipelineRunId,
    slots: u32,
) -> Result<FollowUp, ArchonError> {
    spawn::reserve(ctx, table, run, slots).await
}

/// Called by the event pump when a stage agent's task settles. Idempotent:
/// a stage run already terminal is ignored. Applies infra retries, persists
/// the outcome and verdict, feeds the driver, executes its actions, and on
/// `Finish` settles the item, posts the outcome and triggers the
/// orchestrator.
pub async fn stage_settled(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    outcome: StageOutcome,
) -> Result<(), ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(agent)?;
    let Some(stage) = stage else {
        tracing::debug!(%agent, "settled agent is not a stage agent");
        return Ok(());
    };
    let follow = settle::settle(ctx, &stage.id, outcome).await?;
    run_follow_up(ctx, follow).await
}

/// Called by the event pump when an agent's turn ended: an attempt resumed
/// on a kept agent settles on what it said (§6.7); any other is left alone.
pub async fn turn_ended(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(agent)?;
    let Some(stage) = stage.filter(|s| !s.state.is_terminal() && s.aether_task.is_none()) else {
        return Ok(());
    };
    let follow = resume::check(ctx, &stage.id).await?;
    run_follow_up(ctx, follow).await
}

/// Reuse the item grant and ledger checks on usage events, including usage
/// by delegated children. The run stops visibly if no dollar accounting is
/// available, rather than treating unknown subscription spend as zero.
pub(crate) async fn check_item_budget(
    ctx: &DaemonContext,
    item: &QueueItemId,
    unaccounted_usage: bool,
    stage: Option<&StageRunId>,
) -> Result<(), ArchonError> {
    let caps = crate::resolve::caps(ctx).await;
    let follow = {
        let mut table = ctx.runs.lock().await;
        let (run, left, unaccounted_usage) = {
            let store = ctx.store.lock().await;
            let item = store.item(item)?;
            let left =
                spawn::budget_left(&store, &item, &caps, ctx.config.limits.count_estimated_cost)?;
            let run = store
                .runs_for_item(&item.id)?
                .into_iter()
                .find(|run| !run.state.is_terminal());
            // A hard cap can account at the end of a turn. Do not reject its
            // interim, incomplete usage while Aether still enforces that cap.
            let unaccounted_usage = if unaccounted_usage && left.is_some() && run.is_some() {
                match stage {
                    Some(stage) => {
                        match spawn::load_spawn(&store, stage) {
                            Ok(spawn) => {
                                spawn.request.cost_budget_usd.is_none()
                                    || spawn.request.aether_cost_budget == Some(false)
                            }
                            // Settled requests may already have been pruned.
                            Err(ArchonError::NotFound(_)) => true,
                            Err(error) => return Err(error),
                        }
                    }
                    None => true,
                }
            } else {
                false
            };
            (run, left, unaccounted_usage)
        };
        let (Some(run), Some(left)) = (run, left) else {
            return Ok(());
        };
        let error = if unaccounted_usage {
            "BLOCKED: cannot enforce the item's dollar budget: Aether reported usage \
             without complete dollar cost or a usable subscription estimate; configure \
             model pricing in Aether or choose a cost-reporting model"
                .to_owned()
        } else if left <= 0.0 {
            format!(
                "BLOCKED: the item's budget is used up ({} over); raise it with update_item or retry",
                archon_core::cost::format_usd(-left)
            )
        } else {
            return Ok(());
        };
        finish::fail_run(ctx, &mut table, run, "budget".into(), error, true).await?
    };
    run_follow_up(ctx, follow).await
}

/// Cancel an item's live run (operator or orchestrator request). Returns
/// the item as cancelled, read before a racing retry can queue it again.
pub async fn cancel_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
    reason: &str,
) -> Result<QueueItem, ArchonError> {
    let (follow, item) = {
        let mut table = ctx.runs.lock().await;
        let follow = finish::cancel_item(ctx, &mut table, id, reason).await?;
        (follow, ctx.store.lock().await.item(id)?)
    };
    assert_eq!(&item.id, id, "the store returns the item asked for");
    assert!(item.state.is_terminal(), "a cancelled item is settled");
    run_follow_up(ctx, follow).await?;
    Ok(item)
}

/// The operator decided `gate`, through its chat question or its Lavish
/// page: persist the decision, then go on, loop back, or cancel the item.
pub async fn decide_gate(
    ctx: &DaemonContext,
    gate: &OperatorGate,
    decision: GateDecision,
) -> Result<(), ArchonError> {
    let follow = {
        let mut table = ctx.runs.lock().await;
        gates::decide(ctx, &mut table, &gates::key_of(gate), decision).await?
    };
    run_follow_up(ctx, follow).await
}

/// On daemon start: rebuild drivers for active runs with `RunDriver::resume`
/// and requeue `dispatching` items that never got a run.
pub async fn recover(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let follow = recover::recover(ctx).await?;
    run_follow_up(ctx, follow).await
}

/// Run follow-ups until none are left. Every step is attempted even when
/// an earlier one fails; the first error is returned, the others logged.
pub(crate) async fn run_follow_up(
    ctx: &DaemonContext,
    mut follow: FollowUp,
) -> Result<(), ArchonError> {
    let mut first_error: Option<ArchonError> = None;
    let mut poke = follow.poke;
    // Each round settles or spawns something, so chains end (a spawn's check
    // can settle it, whose driver only queues pending spawns).
    while !follow.is_empty() {
        let (next, error) = follow_up_round(ctx, follow).await;
        poke |= next.poke;
        if let Some(error) = error {
            match first_error {
                None => first_error = Some(error),
                Some(_) => tracing::error!(%error, "follow-up failed"),
            }
        }
        follow = next;
    }
    if poke {
        ctx.dispatch.notify_one();
    }
    first_error.map_or(Ok(()), Err)
}

/// What one round of follow-ups leaves: the work it found and its first
/// error. Later errors are logged, so every step is still attempted.
#[derive(Default)]
struct Round {
    next: FollowUp,
    first_error: Option<ArchonError>,
}

impl Round {
    fn record(&mut self, result: Result<FollowUp, ArchonError>) {
        match result {
            Ok(more) => self.next.merge(more),
            Err(error) if self.first_error.is_none() => self.first_error = Some(error),
            Err(error) => tracing::error!(%error, "follow-up failed"),
        }
    }
}

async fn follow_up_round(
    ctx: &DaemonContext,
    mut follow: FollowUp,
) -> (FollowUp, Option<ArchonError>) {
    let mut round = Round::default();
    stop_and_spawn(ctx, &mut round, &mut follow).await;
    deliver_and_tidy(ctx, &mut round, follow).await;
    (round.next, round.first_error)
}

/// The agents a round ends, starts and asks about, taken out of `follow`.
async fn stop_and_spawn(ctx: &DaemonContext, round: &mut Round, follow: &mut FollowUp) {
    use std::mem::take;
    // First: a spawn of the same run waits for these.
    for stage in dedup(take(&mut follow.descendant_stops)) {
        round.record(descendants::stop(ctx, &stage).await);
    }
    match issuable(ctx, take(&mut follow.issue)).await {
        Ok(issue) => issue_by_run(ctx, round, issue).await,
        Err(error) => round.record(Err(error)),
    }
    for (stage, task) in take(&mut follow.check) {
        round.record(sweep::check_outcome(ctx, &stage, &task).await);
    }
    for run in dedup(take(&mut follow.stop)) {
        round.record(sweep::stop_run(ctx, &run).await);
    }
    for stage in dedup(take(&mut follow.time_out)) {
        round.record(sweep::time_out(ctx, &stage).await);
    }
    for run in dedup(take(&mut follow.retire)) {
        let retired = run_node::retire(ctx, &run).await;
        round.record(retired.map(|()| FollowUp::default()));
    }
    for (agent, reason) in take(&mut follow.cancel_agents) {
        round.record(sweep::cancel_stray(ctx, &agent, &reason).await);
    }
    for stage in dedup(take(&mut follow.sibling_stops)) {
        round.record(siblings::stop(ctx, &stage).await);
    }
}

/// What a round owes once its stops and spawns are made: steers, rosters,
/// gates, pages, merges, kept agents and the worktrees of ended runs.
async fn deliver_and_tidy(ctx: &DaemonContext, round: &mut Round, follow: FollowUp) {
    assert!(
        follow.descendant_stops.is_empty() && follow.issue.is_empty(),
        "stops and spawns are made first"
    );
    assert!(
        follow.stop.is_empty() && follow.sibling_stops.is_empty(),
        "stops and spawns are made first"
    );
    for stage in dedup(follow.steers) {
        round.record(steer::deliver(ctx, &stage).await);
    }
    for run in dedup(follow.roster) {
        round.record(roster::deliver(ctx, &run).await);
    }
    for key in dedup(follow.gates) {
        round.record(gates::announce(ctx, &key).await);
    }
    for stage in dedup(follow.publish) {
        round.record(pages::publish_stage(ctx, &stage).await);
    }
    for run in dedup(follow.merge) {
        round.record(team::merge(ctx, &run).await);
    }
    for kept in dedup(follow.kept) {
        round.record(resume::settle_kept(ctx, kept).await);
    }
    for stage in dedup(follow.resumed) {
        round.record(resume::check(ctx, &stage).await);
    }
    let ships = runs_by_repository(ctx, round, dedup(follow.ship)).await;
    for shipped in lanes::run(ships, |run| async move { ship::deliver(ctx, &run).await }).await {
        round.record(shipped);
    }
    for build in dedup(follow.release_builds) {
        round.record(release_item_build(ctx, build).await);
    }
    let cleanups = lanes::group(follow.cleanup.into_iter().map(|c| (c.repo_path.clone(), c)));
    for removed in lanes::run(cleanups, |cleanup| remove_ended_worktree(ctx, cleanup)).await {
        round.record(removed);
    }
}

/// Issue `spawns`: one run's in order, as its first may create the run's
/// node, and up to `lanes::MAX_LANES` runs at once.
async fn issue_by_run(ctx: &DaemonContext, round: &mut Round, spawns: BTreeSet<StageRunId>) {
    let by_run = {
        let store = ctx.store.lock().await;
        let mut keyed = Vec::with_capacity(spawns.len());
        for id in spawns {
            match store.stage_run(&id) {
                Ok(stage) => keyed.push((stage.run, id)),
                Err(error) => round.record(Err(error)),
            }
        }
        lanes::group(keyed)
    };
    for issued in lanes::run(by_run, |id| async move { spawn::issue(ctx, &id).await }).await {
        round.record(issued);
    }
}

/// `runs` in lanes by repository: one repository's deliveries go one at a
/// time, so its remote and GitHub see one push or pull request at once.
async fn runs_by_repository(
    ctx: &DaemonContext,
    round: &mut Round,
    runs: BTreeSet<PipelineRunId>,
) -> BTreeMap<String, Vec<PipelineRunId>> {
    let store = ctx.store.lock().await;
    let mut keyed = Vec::with_capacity(runs.len());
    for run in runs {
        let repo = store.run(&run).and_then(|r| store.item(&r.item));
        match repo.map(|item| item.repo) {
            Ok(Some(repo)) => keyed.push((repo.as_str().to_owned(), run)),
            Ok(None) => keyed.push((run.as_str().to_owned(), run)),
            Err(error) => round.record(Err(error)),
        }
    }
    lanes::group(keyed)
}

/// Whether `stage`'s agent, or what it delegated, still owes a stop: until
/// Aether confirms it, agents of the stage may still build.
pub(crate) fn stop_owed(
    store: &archon_queue::Store,
    stage: &StageRunId,
) -> Result<bool, ArchonError> {
    Ok(descendants::owes(store, stage)? || siblings::owes(store, stage)?)
}

/// Delete the cargo target directory of an item that ended, unless it was
/// queued again meanwhile (a retry) or a stack fix still builds in it.
async fn release_item_build(
    ctx: &DaemonContext,
    build: ItemBuild,
) -> Result<FollowUp, ArchonError> {
    // Claimed under the run table: a retry starting meanwhile builds into a
    // fresh directory. While an agent of the item lives (a cancel Aether has
    // not confirmed) the directory stays for gc to sweep once it is gone.
    let claimed = {
        let _table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        if !crate::build_cache::is_idle(&store, &build.item)? {
            return Ok(FollowUp::default());
        }
        crate::build_cache::claim_item(&ctx.paths, &build.repo_name, &build.item)?
    };
    let Some(claimed) = claimed else {
        return Ok(FollowUp::default());
    };
    let freed = tokio::task::spawn_blocking(move || crate::build_cache::delete_claimed(claimed))
        .await
        .map_err(|e| {
            ArchonError::io("build directory deletion task", std::io::Error::other(e))
        })??;
    crate::gc::announce_item_release(ctx, &build.item, &freed).await?;
    Ok(FollowUp::default())
}

/// Remove the worktree a run left behind; nothing follows from it.
async fn remove_ended_worktree(
    ctx: &DaemonContext,
    cleanup: Cleanup,
) -> Result<FollowUp, ArchonError> {
    let mark = {
        let _table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        if !store.item(&cleanup.item)?.state.is_terminal()
            || store.has_pending_stack_fix(&cleanup.item)?
        {
            return Ok(FollowUp::default());
        }
        let Some(mark) = ctx.collecting.mark(cleanup.item.clone()) else {
            return Ok(FollowUp::default());
        };
        mark
    };
    assert!(ctx.collecting.contains(&cleanup.item));
    assert!(cleanup.worktree.starts_with(&ctx.paths.worktrees));
    let when = match cleanup.snapshot {
        true => "after its run ended",
        false => "after its run succeeded",
    };
    finish::remove_worktree(ctx, cleanup, when, Some(mark)).await?;
    Ok(FollowUp::default())
}

/// The spawns to issue now. While dispatch is held for disk space only those
/// of finished runs are, so a run cancelled meanwhile retires at once. On a
/// full disk only their never-sent ones are, which settle without Aether: a
/// spawn that may have reached Aether would hit the full disk again, so it
/// waits for the hold to end. Low space alone sends it, to stop its agent.
async fn issuable(
    ctx: &DaemonContext,
    issue: Vec<StageRunId>,
) -> Result<BTreeSet<StageRunId>, ArchonError> {
    let issue = dedup(issue);
    if issue.is_empty() || !ctx.disk.holding() {
        return Ok(issue);
    }
    let full = ctx.disk.disk_full().is_some();
    let store = ctx.store.lock().await;
    let mut finished = BTreeSet::new();
    for id in &issue {
        let stage = store.stage_run(id)?;
        assert_eq!(&stage.id, id, "the store returns the stage run asked for");
        if !store.run(&stage.run)?.state.is_terminal() {
            continue;
        }
        if !full || spawn::never_sent(&store, id)? {
            finished.insert(id.clone());
        }
    }
    let waiting = issue.len() - finished.len();
    tracing::debug!(
        spawns = waiting,
        disk_full = full,
        "spawns wait for disk space"
    );
    debug_assert!(finished.is_subset(&issue));
    Ok(finished)
}

fn dedup<T: Ord>(items: Vec<T>) -> BTreeSet<T> {
    items.into_iter().collect()
}

/// At most `max_chars` characters, marked when cut.
pub(crate) fn clip(text: &str, max_chars: usize) -> String {
    assert!(max_chars > 1, "a clip keeps at least one character");
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    debug_assert_eq!(out.chars().count(), max_chars);
    out
}
