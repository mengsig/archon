//! The pure dispatch planner (ARCHITECTURE §7.3). No I/O and no clock: the
//! daemon builds a snapshot from the store, calls [`plan_dispatch`], and
//! executes the plan. Deterministic for a given snapshot, so it is tested
//! exhaustively without a database.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::host_load::HostLoadHold;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::protocol::WaitingReasonCode;
use archon_core::queue::{Caps, PauseState, Priority};
use archon_core::quota::QuotaHold;

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchSnapshot {
    pub caps: Caps,
    /// Per-repository `max_per_repo` from `.archon/config.toml`; it can only
    /// lower `caps.max_per_repo` (0 = no override).
    pub repo_caps: BTreeMap<RepoId, u32>,
    /// Operator budget switches override the global switch for this repository.
    pub repo_budgets_enabled: BTreeMap<RepoId, bool>,
    /// Operator overrides may raise or lower the global per-repository cap.
    pub repo_max_per_repo: BTreeMap<RepoId, u32>,
    pub pause: PauseState,
    /// The daemon's disk hold: free space below `limits.min_free_gb`, or a
    /// recent failure on a full disk.
    pub low_disk: bool,
    /// The daemon's host-load hold: load or free memory past its limit.
    pub host_load: Option<HostLoadHold>,
    /// An operator's upgrade drains Archon: running agents finish, and
    /// nothing new starts until archond restarts or the upgrade gives up.
    pub draining: bool,
    /// Running stage agents by repository and by pipeline.
    pub running_total: u32,
    pub running_by_repo: BTreeMap<RepoId, u32>,
    pub running_by_pipeline: BTreeMap<String, u32>,
    /// Live runs, plus items already picked whose run is not created yet.
    pub active_runs: u32,
    pub spend_today_usd: f64,
    pub spend_total_usd: f64,
    /// Stage agents that running runs want and have not started yet.
    pub pending_spawns: Vec<PendingSpawn>,
    /// Queued items with their dependencies' states resolved.
    pub queued: Vec<QueuedCandidate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSpawn {
    pub run: PipelineRunId,
    pub item: QueueItemId,
    pub repo: Option<RepoId>,
    pub pipeline: String,
    pub priority: Priority,
    pub item_created_seq: u64,
    /// Agents still wanted by the run's current stage (fan-out remainder).
    pub wanted: u32,
    /// A team stage: all `wanted` agents are admitted together or none.
    pub gang: bool,
    /// Every provider some wanted stage may use is out of quota: no slot.
    pub quota_hold: Option<QuotaHold>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueuedCandidate {
    pub item: QueueItemId,
    pub repo: Option<RepoId>,
    pub pipeline: String,
    pub priority: Priority,
    /// Monotonic creation order; ties broken oldest first.
    pub created_seq: u64,
    pub deps: DependencyState,
    /// Agents the first stage wants (its fan-out).
    pub first_stage_agents: u32,
    /// The first stage is a team stage: its agents fit together or it waits.
    pub gang: bool,
    /// Set when every provider one of its tier-only stages may use is out
    /// of quota: it waits, like an unmet dependency, until one has quota.
    pub quota_hold: Option<QuotaHold>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyState {
    AllSucceeded,
    Waiting,
    /// A dependency failed or was cancelled: the item must become `blocked`.
    Broken,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchPlan {
    /// Agent slots granted to running runs, in admission order.
    pub spawn: Vec<(PipelineRunId, u32)>,
    /// Queued items to move to `dispatching`, in admission order.
    pub start: Vec<QueueItemId>,
    /// Queued items whose dependencies broke.
    pub block: Vec<QueueItemId>,
    /// Why nothing (more) was admitted, for `Status` and one-shot blockers.
    pub held_by: Option<Hold>,
    /// Teams that want more agents at once than their caps now allow, so
    /// they wait until a cap is raised; the daemon tells the operator.
    pub never_fits: Vec<NeverFits>,
}

/// A team stage (gang) of `wanted` agents over a cap of `cap`: lowered
/// after it was enqueued, as enqueue refuses such a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeverFits {
    pub waiter: GangWaiter,
    pub wanted: u32,
    pub cap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GangWaiter {
    /// A running run's pending team stage.
    Run(PipelineRunId),
    /// A queued item whose first stage is the team.
    Item(QueueItemId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    GlobalPause,
    /// An upgrade is draining Archon before it restarts.
    Draining,
    /// Too little free disk space, or a recent disk-full failure.
    LowDisk,
    /// Host load or available memory is past its `[limits]` bound.
    HostLoad,
    DailyBudget,
    TotalBudget,
    Capacity,
    /// Items that would start wait for a provider's quota to come back.
    ProviderQuota,
}

/// Decide what to admit now (ARCHITECTURE §7.3):
///
/// 1. Items whose dependencies broke are always listed in `block`, even
///    while admission is held, since blocking starts no work.
/// 2. Nothing is admitted while globally paused, while an upgrade drains,
///    while the disk or host-load hold is on, or while total or daily spend has reached
///    its budget (reported in that order; no budget holds while budgets
///    are off).
/// 3. Pending spawns of running runs come first, by item priority then
///    age. Repository and pipeline pauses do not hold them: they stop new
///    items, and finishing work beats starting work. Quota-held ones wait.
/// 4. Then queued items whose dependencies all succeeded, by priority then
///    age, skipping paused repositories and pipelines and items a provider's
///    quota holds (`held_by` is then `ProviderQuota` unless another hold is).
///
/// Every agent slot fits the global, per-repo and per-pipeline caps, and
/// every started item fits `max_active_runs` (0 = unlimited everywhere). A
/// fan-out is admitted partially; a started item reserves its first
/// stage's agents, as many as fit. A team stage (a gang) is admitted whole
/// or not at all. Candidates that do not fit are skipped so smaller or
/// other-repository work still flows, and `held_by` is then `Capacity`;
/// only work under the caps a waiting gang could fit in waits behind it,
/// so the gang is not starved.
pub fn plan_dispatch(snapshot: &DispatchSnapshot) -> DispatchPlan {
    let mut plan = DispatchPlan {
        block: broken_items(&snapshot.queued),
        never_fits: never_fitting(snapshot),
        ..DispatchPlan::default()
    };
    if let Some(hold) = admission_hold(snapshot, None) {
        plan.held_by = Some(hold.clone());
        if matches!(hold, Hold::GlobalPause | Hold::LowDisk | Hold::HostLoad)
            || snapshot.repo_budgets_enabled.is_empty()
        {
            return plan;
        }
    }
    if plan.held_by.is_none() {
        plan.held_by = snapshot
            .queued
            .iter()
            .filter_map(|c| admission_hold(snapshot, c.repo.as_ref()))
            .chain(
                snapshot
                    .pending_spawns
                    .iter()
                    .filter_map(|p| admission_hold(snapshot, p.repo.as_ref())),
            )
            .next();
    }
    let mut slots = Slots::new(snapshot);
    let mut waits = GangWaits::default();
    let spawns_held = grant_pending_spawns(
        &snapshot.pending_spawns,
        &mut slots,
        &mut waits,
        &mut plan.spawn,
    );
    let starts_held = start_items(snapshot, &mut slots, &mut waits, &mut plan.start);
    if spawns_held || starts_held {
        plan.held_by = Some(Hold::Capacity);
    }
    let spawn_waits = snapshot
        .pending_spawns
        .iter()
        .any(|p| p.quota_hold.is_some());
    if plan.held_by.is_none() && (spawn_waits || quota_held(snapshot).next().is_some()) {
        plan.held_by = Some(Hold::ProviderQuota);
    }
    debug_assert!(plan.start.iter().all(|id| !plan.block.contains(id)));
    debug_assert!(plan.spawn.iter().all(|(_, n)| *n > 0));
    plan
}

/// Explain the same admission pass the dispatcher would execute, including
/// pending spawns, reservations for earlier items, and gang fairness.
/// Dependencies take precedence, then global admission holds (pause, drain,
/// disk, total budget, daily budget), slice pauses, provider quota, capacity,
/// and next in line.
pub fn waiting_reasons(snapshot: &DispatchSnapshot) -> BTreeMap<QueueItemId, WaitingReasonCode> {
    let plan = plan_dispatch(snapshot);
    let started: BTreeSet<_> = plan.start.iter().collect();
    snapshot
        .queued
        .iter()
        .map(|item| {
            let reason = match item.deps {
                DependencyState::Broken => WaitingReasonCode::DependencyFailed,
                DependencyState::Waiting => WaitingReasonCode::DependencyWaiting,
                DependencyState::AllSucceeded => admission_hold(snapshot, item.repo.as_ref())
                    .map(|hold| match hold {
                        Hold::GlobalPause => WaitingReasonCode::GlobalPause,
                        Hold::Draining => WaitingReasonCode::Draining,
                        Hold::LowDisk => WaitingReasonCode::LowDisk,
                        Hold::HostLoad => WaitingReasonCode::HostLoad,
                        Hold::TotalBudget => WaitingReasonCode::TotalBudget,
                        Hold::DailyBudget => WaitingReasonCode::DailyBudget,
                        Hold::Capacity | Hold::ProviderQuota => {
                            unreachable!("decided per item by the admission plan")
                        }
                    })
                    .or_else(|| slice_pause(snapshot, item))
                    .or_else(|| {
                        item.quota_hold
                            .as_ref()
                            .map(|_| WaitingReasonCode::ProviderQuota)
                    })
                    .unwrap_or_else(|| {
                        if started.contains(&item.item) {
                            WaitingReasonCode::NextInLine
                        } else {
                            WaitingReasonCode::Capacity
                        }
                    }),
            };
            (item.item.clone(), reason)
        })
        .collect()
}

fn slice_pause(snapshot: &DispatchSnapshot, item: &QueuedCandidate) -> Option<WaitingReasonCode> {
    if item
        .repo
        .as_ref()
        .is_some_and(|r| snapshot.pause.repos.contains(r))
    {
        Some(WaitingReasonCode::RepoPause)
    } else if snapshot.pause.pipelines.contains(&item.pipeline) {
        Some(WaitingReasonCode::PipelinePause)
    } else {
        None
    }
}

fn broken_items(queued: &[QueuedCandidate]) -> Vec<QueueItemId> {
    let mut broken: Vec<&QueuedCandidate> = queued
        .iter()
        .filter(|c| c.deps == DependencyState::Broken)
        .collect();
    broken.sort_by_key(|c| (c.created_seq, c.item.clone()));
    broken.into_iter().map(|c| c.item.clone()).collect()
}

/// Pending team spawns and eligible queued team items that want more
/// agents than the smallest cap over them, runs first, each kind sorted by
/// id; each waits forever unless the operator raises the cap.
fn never_fitting(snapshot: &DispatchSnapshot) -> Vec<NeverFits> {
    let over = |repo: Option<&RepoId>, pipeline: &str, wanted: u32| {
        let cap = gang_cap(snapshot, repo, pipeline);
        (cap != 0 && wanted > cap).then_some(cap)
    };
    let mut found = Vec::new();
    for spawn in snapshot.pending_spawns.iter().filter(|p| p.gang) {
        if let Some(cap) = over(spawn.repo.as_ref(), &spawn.pipeline, spawn.wanted) {
            let waiter = GangWaiter::Run(spawn.run.clone());
            found.push(NeverFits {
                waiter,
                wanted: spawn.wanted,
                cap,
            });
        }
    }
    for item in eligible(snapshot).into_iter().filter(|c| c.gang) {
        let wanted = item.first_stage_agents;
        if let Some(cap) = over(item.repo.as_ref(), &item.pipeline, wanted) {
            let waiter = GangWaiter::Item(item.item.clone());
            found.push(NeverFits {
                waiter,
                wanted,
                cap,
            });
        }
    }
    // Input order must not leak into the plan; `GangWaiter` sorts runs first.
    found.sort_by(|a, b| a.waiter.cmp(&b.waiter));
    found.dedup();
    debug_assert!(found.iter().all(|f| f.cap > 0 && f.wanted > f.cap));
    found
}

/// The most agents a gang in `repo` and `pipeline` could ever get at once;
/// 0 when nothing caps it.
fn gang_cap(snapshot: &DispatchSnapshot, repo: Option<&RepoId>, pipeline: &str) -> u32 {
    let repo_cap = repo.map_or(0, |r| repo_cap(snapshot, r));
    let pipeline_cap = snapshot
        .caps
        .max_per_pipeline
        .get(pipeline)
        .copied()
        .unwrap_or(0);
    [snapshot.caps.max_parallel_agents, repo_cap, pipeline_cap]
        .into_iter()
        .fold(0, lower_cap)
}

/// The repository file may only lower the global per-repo cap.
fn repo_cap(snapshot: &DispatchSnapshot, repo: &RepoId) -> u32 {
    let own = snapshot.repo_caps.get(repo).copied().unwrap_or(0);
    let base = snapshot
        .repo_max_per_repo
        .get(repo)
        .copied()
        .unwrap_or(snapshot.caps.max_per_repo);
    lower_cap(base, own)
}

/// A budget of 0 is unlimited; any other budget holds once spend reaches
/// it, unless budgets are switched off.
fn admission_hold(snapshot: &DispatchSnapshot, repo: Option<&RepoId>) -> Option<Hold> {
    let caps = &snapshot.caps;
    let enabled = repo
        .and_then(|r| snapshot.repo_budgets_enabled.get(r))
        .copied()
        .unwrap_or(caps.budgets_enabled);
    let reached = |budget: f64, spent: f64| enabled && budget > 0.0 && spent >= budget;
    if snapshot.pause.global {
        Some(Hold::GlobalPause)
    } else if snapshot.draining {
        Some(Hold::Draining)
    } else if snapshot.low_disk {
        Some(Hold::LowDisk)
    } else if snapshot.host_load.is_some() {
        Some(Hold::HostLoad)
    } else if reached(caps.budget_usd_total, snapshot.spend_total_usd) {
        Some(Hold::TotalBudget)
    } else if reached(caps.budget_usd_daily, snapshot.spend_today_usd) {
        Some(Hold::DailyBudget)
    } else {
        None
    }
}

/// Free slots under one cap (0 = unlimited). Usage above a lowered cap
/// leaves no room rather than underflowing.
fn room(cap: u32, used: u32) -> u32 {
    if cap == 0 {
        u32::MAX
    } else {
        cap.saturating_sub(used)
    }
}

/// The most agents that can ever run at once for work in `repo` (with its
/// own `.archon/config.toml` override `repo_cap`, 0 = none) and `pipeline`:
/// the smallest non-zero cap, or 0 when nothing caps it.
pub fn effective_agent_cap(caps: &Caps, repo: Option<u32>, pipeline: &str) -> u32 {
    let repo_cap = repo.map_or(0, |own| lower_cap(caps.max_per_repo, own));
    let pipeline_cap = caps.max_per_pipeline.get(pipeline).copied().unwrap_or(0);
    let each = [caps.max_parallel_agents, repo_cap, pipeline_cap];
    let cap = each.into_iter().fold(0, lower_cap);
    debug_assert!(
        each.iter().all(|c| *c == 0 || cap <= *c),
        "no cap is exceeded"
    );
    debug_assert_eq!(
        cap == 0,
        each.iter().all(|c| *c == 0),
        "0 only when uncapped"
    );
    cap
}

/// The lower of two caps where 0 means unlimited.
fn lower_cap(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, other) | (other, 0) => other,
        (a, b) => a.min(b),
    }
}

/// Caps a held gang could fit in once agents finish: later work under
/// them waits too, so a stream of smaller work cannot starve the gang.
#[derive(Debug, Default)]
struct GangWaits {
    global: bool,
    repos: BTreeSet<RepoId>,
    pipelines: BTreeSet<String>,
}

impl GangWaits {
    /// A gang of `wanted` agents did not fit: wait on each cap too full for
    /// it, unless a cap is below `wanted` and it can never fit.
    fn hold(&mut self, slots: &Slots<'_>, repo: Option<&RepoId>, pipeline: &str, wanted: u32) {
        assert!(
            wanted > slots.free(repo, pipeline),
            "only a gang that does not fit waits"
        );
        let cap = gang_cap(slots.snapshot, repo, pipeline);
        if cap != 0 && wanted > cap {
            return;
        }
        if slots.global_room() < wanted {
            self.global = true;
        }
        if slots.pipeline_room(pipeline) < wanted {
            self.pipelines.insert(pipeline.to_owned());
        }
        if let Some(repo) = repo.filter(|r| slots.repo_room(r) < wanted) {
            self.repos.insert(repo.clone());
        }
        debug_assert!(self.global || !self.pipelines.is_empty() || !self.repos.is_empty());
    }

    fn blocks(&self, repo: Option<&RepoId>, pipeline: &str) -> bool {
        self.global
            || self.pipelines.contains(pipeline)
            || repo.is_some_and(|r| self.repos.contains(r))
    }
}

/// Running counts plus what this plan has admitted so far.
struct Slots<'a> {
    snapshot: &'a DispatchSnapshot,
    total: u32,
    by_repo: BTreeMap<RepoId, u32>,
    by_pipeline: BTreeMap<String, u32>,
    active_runs: u32,
}

impl<'a> Slots<'a> {
    fn new(snapshot: &'a DispatchSnapshot) -> Self {
        Self {
            snapshot,
            total: snapshot.running_total,
            by_repo: snapshot.running_by_repo.clone(),
            by_pipeline: snapshot.running_by_pipeline.clone(),
            active_runs: snapshot.active_runs,
        }
    }

    fn global_room(&self) -> u32 {
        room(self.snapshot.caps.max_parallel_agents, self.total)
    }

    fn pipeline_room(&self, pipeline: &str) -> u32 {
        room(
            self.snapshot
                .caps
                .max_per_pipeline
                .get(pipeline)
                .copied()
                .unwrap_or(0),
            self.by_pipeline.get(pipeline).copied().unwrap_or(0),
        )
    }

    fn repo_room(&self, repo: &RepoId) -> u32 {
        room(
            repo_cap(self.snapshot, repo),
            self.by_repo.get(repo).copied().unwrap_or(0),
        )
    }

    fn free(&self, repo: Option<&RepoId>, pipeline: &str) -> u32 {
        let per_repo = repo.map_or(u32::MAX, |r| self.repo_room(r));
        self.global_room()
            .min(self.pipeline_room(pipeline))
            .min(per_repo)
    }

    fn take(&mut self, repo: Option<&RepoId>, pipeline: &str, agents: u32) {
        assert!(
            agents > 0 && agents <= self.free(repo, pipeline),
            "admitting {agents} agents beyond the caps"
        );
        self.total = self.total.saturating_add(agents);
        let in_pipeline = self.by_pipeline.entry(pipeline.to_owned()).or_default();
        *in_pipeline = in_pipeline.saturating_add(agents);
        if let Some(repo) = repo {
            let in_repo = self.by_repo.entry(repo.clone()).or_default();
            *in_repo = in_repo.saturating_add(agents);
        }
    }

    fn run_room(&self) -> bool {
        room(self.snapshot.caps.max_active_runs, self.active_runs) > 0
    }

    fn take_run(&mut self) {
        assert!(self.run_room(), "starting a run beyond max_active_runs");
        self.active_runs += 1;
    }
}

/// Returns whether some wanted agent did not fit.
fn grant_pending_spawns(
    pending: &[PendingSpawn],
    slots: &mut Slots<'_>,
    waits: &mut GangWaits,
    granted: &mut Vec<(PipelineRunId, u32)>,
) -> bool {
    let mut order: Vec<&PendingSpawn> = pending
        .iter()
        .filter(|p| p.wanted > 0 && p.quota_hold.is_none())
        .filter(|p| admission_hold(slots.snapshot, p.repo.as_ref()).is_none())
        .collect();
    order.sort_by(|a, b| {
        (b.priority.cmp(&a.priority))
            .then(a.item_created_seq.cmp(&b.item_created_seq))
            .then(a.run.cmp(&b.run))
    });
    debug_assert!(
        order.windows(2).all(|w| w[0].run != w[1].run),
        "run listed twice"
    );
    debug_assert!(order.iter().all(|p| p.quota_hold.is_none()));
    let mut held = false;
    for spawn in order {
        let (repo, pipeline) = (spawn.repo.as_ref(), spawn.pipeline.as_str());
        let free = if waits.blocks(repo, pipeline) {
            0
        } else {
            slots.free(repo, pipeline)
        };
        let grant = match spawn.gang {
            true if free < spawn.wanted => 0,
            _ => spawn.wanted.min(free),
        };
        if grant < spawn.wanted {
            held = true;
        }
        if spawn.gang && grant == 0 && !waits.blocks(repo, pipeline) {
            waits.hold(slots, repo, pipeline, spawn.wanted);
        }
        debug_assert!(!spawn.gang || grant == 0 || grant == spawn.wanted);
        if grant > 0 {
            slots.take(spawn.repo.as_ref(), &spawn.pipeline, grant);
            granted.push((spawn.run.clone(), grant));
        }
    }
    held
}

/// Items that could start but for a provider's quota.
fn quota_held(snapshot: &DispatchSnapshot) -> impl Iterator<Item = &QueuedCandidate> {
    snapshot.queued.iter().filter(|c| {
        c.deps == DependencyState::AllSucceeded
            && c.quota_hold.is_some()
            && slice_pause(snapshot, c).is_none()
    })
}

fn eligible(snapshot: &DispatchSnapshot) -> Vec<&QueuedCandidate> {
    let mut order: Vec<&QueuedCandidate> = snapshot
        .queued
        .iter()
        .filter(|c| c.deps == DependencyState::AllSucceeded)
        .filter(|c| c.quota_hold.is_none())
        .filter(|c| slice_pause(snapshot, c).is_none())
        .collect();
    order.sort_by(|a, b| {
        (b.priority.cmp(&a.priority))
            .then(a.created_seq.cmp(&b.created_seq))
            .then(a.item.cmp(&b.item))
    });
    order
}

/// Returns whether some eligible item did not fit.
fn start_items(
    snapshot: &DispatchSnapshot,
    slots: &mut Slots<'_>,
    waits: &mut GangWaits,
    started: &mut Vec<QueueItemId>,
) -> bool {
    let order: Vec<_> = eligible(snapshot)
        .into_iter()
        .filter(|c| admission_hold(snapshot, c.repo.as_ref()).is_none())
        .collect();
    for item in &order {
        if !slots.run_room() {
            return true;
        }
        let (repo, pipeline) = (item.repo.as_ref(), item.pipeline.as_str());
        if waits.blocks(repo, pipeline) {
            continue;
        }
        debug_assert!(item.first_stage_agents > 0, "{} wants no agents", item.item);
        let wanted = item.first_stage_agents.max(1);
        let free = slots.free(repo, pipeline);
        if item.gang && free < wanted {
            waits.hold(slots, repo, pipeline, wanted);
            continue;
        }
        if free == 0 {
            continue;
        }
        let reserve = wanted.min(free);
        slots.take(repo, pipeline, reserve);
        slots.take_run();
        started.push(item.item.clone());
    }
    started.len() < order.len()
}
