//! Executes pipeline engine actions (ARCHITECTURE §5.1, §6).
//! Holds one `RunDriver` per active run; persists before every Aether call
//! (`StageRun{Spawning}` first) so a crash is always retryable under the
//! stage run's idempotency key.
//!
//! Locking: `ctx.runs` is taken before `ctx.store`, and neither is held
//! across an Aether call. Work that needs Aether or git after a state change
//! is returned as a `FollowUp` and run once the run table is released.

mod dependencies;
mod descendants;
mod finish;
pub(crate) mod frozen;
mod gate_news;
mod gates;
mod naming;
mod pages;
mod recover;
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
use archon_pipeline::{RunDriver, SpawnPlan};

use crate::context::DaemonContext;

pub(crate) use finish::notify;
pub(crate) use recover::prune_finished_run_vars;
pub(crate) use revive::revived;
pub(crate) use roster::prune_settled_rosters;
pub(crate) use run_node::recover_message_node;
pub(crate) use run_node::{is_node_agent, node_run, prune_retired_node_agents};
pub(crate) use spawn::{load_spawn, prune_settled_requests, runs_awaiting_spawn};
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
    pub repo_path: PathBuf,
    pub worktree: PathBuf,
    /// A `repo-readonly` run's snapshot, removed with `--force`.
    pub snapshot: bool,
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
    /// Freshly opened operator gates to show and ask about.
    pub gates: Vec<gates::GateKey>,
    /// Completed stage runs whose listed reports to publish.
    pub publish: Vec<StageRunId>,
    /// Runs whose queued team merge to run.
    pub merge: Vec<PipelineRunId>,
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
        self.gates.extend(other.gates);
        self.publish.extend(other.publish);
        self.merge.extend(other.merge);
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
            && self.gates.is_empty()
            && self.publish.is_empty()
            && self.merge.is_empty()
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
    let cap = crate::resolve::caps(ctx).await.budget_usd_per_item;
    let follow = {
        let mut table = ctx.runs.lock().await;
        let (run, left, unaccounted_usage) = {
            let store = ctx.store.lock().await;
            let item = store.item(item)?;
            let left =
                spawn::budget_left(&store, &item, cap, ctx.config.limits.count_estimated_cost)?;
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
        Ok(issue) => {
            for id in issue {
                round.record(spawn::issue(ctx, &id).await);
            }
        }
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
    for cleanup in follow.cleanup {
        round.record(remove_ended_worktree(ctx, cleanup).await);
    }
}

/// Remove the worktree a run left behind; nothing follows from it.
async fn remove_ended_worktree(
    ctx: &DaemonContext,
    cleanup: Cleanup,
) -> Result<FollowUp, ArchonError> {
    let when = match cleanup.snapshot {
        true => "after its run ended",
        false => "after its run succeeded",
    };
    finish::remove_worktree(ctx, cleanup, when).await?;
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
