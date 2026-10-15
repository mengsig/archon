//! Table-driven `plan_dispatch` cases for every rule in ARCHITECTURE §7.3.

use std::collections::BTreeMap;

use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::queue::{Caps, PauseState, Priority};
use archon_queue::dispatch::{
    DependencyState, DispatchPlan, DispatchSnapshot, GangWaiter, Hold, NeverFits, PendingSpawn,
    QueuedCandidate, effective_agent_cap, plan_dispatch,
};

/// Stable ids so cases can name items, runs and repositories by index.
struct Fx {
    items: Vec<QueueItemId>,
    runs: Vec<PipelineRunId>,
    repos: [RepoId; 2],
}

impl Fx {
    fn new() -> Self {
        Self {
            items: (0..8).map(|_| QueueItemId::new()).collect(),
            runs: (0..4).map(|_| PipelineRunId::new()).collect(),
            repos: [RepoId::new(), RepoId::new()],
        }
    }

    /// Unlimited caps, nothing running, nothing paused, nothing spent.
    fn base(&self) -> DispatchSnapshot {
        DispatchSnapshot {
            caps: Caps {
                max_parallel_agents: 0,
                ..Caps::default()
            },
            repo_caps: BTreeMap::new(),
            repo_budgets_enabled: BTreeMap::new(),
            repo_max_per_repo: BTreeMap::new(),
            pause: PauseState::default(),
            low_disk: false,
            draining: false,
            running_total: 0,
            running_by_repo: BTreeMap::new(),
            running_by_pipeline: BTreeMap::new(),
            active_runs: 0,
            spend_today_usd: 0.0,
            spend_total_usd: 0.0,
            pending_spawns: vec![],
            queued: vec![],
        }
    }

    /// Queued item `i` in repo `r`, pipeline `direct`, created at `seq`.
    fn item(&self, i: usize, r: usize, priority: Priority, seq: u64) -> QueuedCandidate {
        QueuedCandidate {
            item: self.items[i].clone(),
            repo: Some(self.repos[r].clone()),
            pipeline: "direct".into(),
            priority,
            created_seq: seq,
            deps: DependencyState::AllSucceeded,
            first_stage_agents: 1,
            gang: false,
        }
    }

    /// Run `k` of item `i` wanting `wanted` more agents.
    fn pending(
        &self,
        k: usize,
        i: usize,
        r: usize,
        priority: Priority,
        seq: u64,
        wanted: u32,
    ) -> PendingSpawn {
        PendingSpawn {
            run: self.runs[k].clone(),
            item: self.items[i].clone(),
            repo: Some(self.repos[r].clone()),
            pipeline: "direct".into(),
            priority,
            item_created_seq: seq,
            wanted,
            gang: false,
        }
    }

    fn plan(
        &self,
        spawn: &[(usize, u32)],
        start: &[usize],
        block: &[usize],
        held_by: Option<Hold>,
    ) -> DispatchPlan {
        DispatchPlan {
            spawn: spawn
                .iter()
                .map(|&(k, n)| (self.runs[k].clone(), n))
                .collect(),
            start: start.iter().map(|&i| self.items[i].clone()).collect(),
            block: block.iter().map(|&i| self.items[i].clone()).collect(),
            held_by,
            never_fits: vec![],
        }
    }
}

use Priority::{Critical, High, Low, Normal};

type Build = fn(&Fx) -> DispatchSnapshot;
type Expect = fn(&Fx) -> DispatchPlan;

fn global_pause(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.pause.global = true;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 1)];
    s.queued = vec![fx.item(0, 0, Normal, 1), fx.item(1, 0, Normal, 2)];
    s.queued[1].deps = DependencyState::Broken;
    s
}

fn low_disk(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.low_disk = true;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 1)];
    s.queued = vec![fx.item(0, 0, Normal, 1), fx.item(1, 0, Normal, 2)];
    s.queued[1].deps = DependencyState::Broken;
    s
}

fn draining(fx: &Fx) -> DispatchSnapshot {
    let mut s = low_disk(fx);
    s.low_disk = false;
    s.draining = true;
    s
}

fn draining_on_low_disk(fx: &Fx) -> DispatchSnapshot {
    let mut s = draining(fx);
    s.low_disk = true;
    s
}

fn low_disk_and_budget(fx: &Fx) -> DispatchSnapshot {
    let mut s = spent(1.0, 100.5, 50.0, 100.0)(fx);
    s.low_disk = true;
    s
}

fn low_disk_and_paused(fx: &Fx) -> DispatchSnapshot {
    let mut s = low_disk(fx);
    s.pause.global = true;
    s
}

fn repo_pause(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.pause.repos = vec![fx.repos[0].clone()];
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 2)];
    s.queued = vec![fx.item(0, 0, Critical, 1), fx.item(1, 1, Low, 2)];
    s
}

fn pipeline_pause(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.pause.pipelines = vec!["adaptive".into()];
    let mut strict = fx.item(0, 0, Critical, 1);
    strict.pipeline = "adaptive".into();
    let mut strict_spawn = fx.pending(0, 7, 0, Normal, 0, 1);
    strict_spawn.pipeline = "adaptive".into();
    s.pending_spawns = vec![strict_spawn];
    s.queued = vec![strict, fx.item(1, 0, Low, 2)];
    s
}

fn spent(
    today: f64,
    total: f64,
    daily_cap: f64,
    total_cap: f64,
) -> impl Fn(&Fx) -> DispatchSnapshot {
    move |fx| {
        let mut s = fx.base();
        s.caps.budget_usd_daily = daily_cap;
        s.caps.budget_usd_total = total_cap;
        s.spend_today_usd = today;
        s.spend_total_usd = total;
        s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 1)];
        s.queued = vec![fx.item(0, 0, Normal, 1)];
        s
    }
}

fn daily_reached(fx: &Fx) -> DispatchSnapshot {
    spent(50.0, 60.0, 50.0, 0.0)(fx)
}
fn daily_below(fx: &Fx) -> DispatchSnapshot {
    spent(49.99, 60.0, 50.0, 100.0)(fx)
}
fn total_reached(fx: &Fx) -> DispatchSnapshot {
    spent(1.0, 100.5, 50.0, 100.0)(fx)
}
fn both_reached(fx: &Fx) -> DispatchSnapshot {
    spent(80.0, 200.0, 50.0, 100.0)(fx)
}
fn budgets_off(fx: &Fx) -> DispatchSnapshot {
    let mut s = spent(80.0, 200.0, 50.0, 100.0)(fx);
    s.caps.budgets_enabled = false;
    s
}
fn budgets_unlimited(fx: &Fx) -> DispatchSnapshot {
    spent(1e9, 1e9, 0.0, 0.0)(fx)
}

fn priority_then_age(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.queued = vec![
        fx.item(0, 0, Normal, 10),
        fx.item(1, 0, Critical, 30),
        fx.item(2, 1, Normal, 5),
        fx.item(3, 1, Low, 1),
        fx.item(4, 0, High, 40),
        fx.item(5, 1, Critical, 20),
    ];
    s
}

fn spawns_before_items(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 2;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Low, 99, 2)];
    s.queued = vec![fx.item(0, 0, Critical, 1)];
    s
}

fn spawns_by_priority_then_age(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 4;
    s.running_total = 1;
    s.pending_spawns = vec![
        fx.pending(0, 4, 0, Normal, 5, 2),
        fx.pending(1, 5, 1, High, 9, 2),
        fx.pending(2, 6, 0, Normal, 3, 2),
    ];
    s
}

fn partial_fan_out(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 3;
    s.running_total = 1;
    let mut reviewers = fx.item(0, 0, High, 1);
    reviewers.first_stage_agents = 3;
    s.queued = vec![reviewers, fx.item(1, 1, Normal, 2)];
    s
}

fn partial_pending_fan_out(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 3;
    s.running_total = 1;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 3)];
    s
}

fn per_repo_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_repo = 1;
    s.running_total = 1;
    s.running_by_repo.insert(fx.repos[0].clone(), 1);
    s.pending_spawns = vec![fx.pending(0, 7, 0, Critical, 0, 1)];
    s.queued = vec![
        fx.item(0, 0, Critical, 1),
        fx.item(1, 1, Low, 2),
        fx.item(2, 1, Low, 3),
    ];
    s
}

fn repo_override_lowers(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_repo = 3;
    s.repo_caps.insert(fx.repos[0].clone(), 1);
    s.repo_caps.insert(fx.repos[1].clone(), 5);
    s.queued = (0..6)
        .map(|i| fx.item(i, i % 2, Normal, i as u64))
        .collect();
    s
}

fn repo_override_without_global(fx: &Fx) -> DispatchSnapshot {
    let mut s = repo_override_lowers(fx);
    s.caps.max_per_repo = 0;
    s.repo_caps.insert(fx.repos[1].clone(), 0);
    s
}

fn per_pipeline_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_pipeline.insert("strict".into(), 2);
    s.caps.max_per_pipeline.insert("direct".into(), 0);
    s.running_by_pipeline.insert("strict".into(), 1);
    s.running_total = 1;
    let mut spawn = fx.pending(0, 7, 0, Normal, 0, 2);
    spawn.pipeline = "strict".into();
    let mut strict = fx.item(0, 0, Critical, 1);
    strict.pipeline = "strict".into();
    s.pending_spawns = vec![spawn];
    s.queued = vec![strict, fx.item(1, 0, Normal, 2), fx.item(2, 1, Normal, 3)];
    s
}

fn active_run_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_active_runs = 2;
    s.active_runs = 1;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 4)];
    s.queued = vec![fx.item(0, 0, Normal, 1), fx.item(1, 1, Normal, 2)];
    s
}

fn deps_block_and_wait(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.queued = vec![
        fx.item(0, 0, Critical, 5),
        fx.item(1, 0, Normal, 3),
        fx.item(2, 1, Low, 4),
        fx.item(3, 1, Low, 1),
    ];
    s.queued[0].deps = DependencyState::Waiting;
    s.queued[1].deps = DependencyState::Broken;
    s.queued[3].deps = DependencyState::Broken;
    s
}

fn repoless_items(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_repo = 1;
    s.running_by_repo.insert(fx.repos[0].clone(), 1);
    s.running_total = 1;
    let mut ask = fx.item(0, 0, Normal, 1);
    ask.repo = None;
    ask.pipeline = "ask".into();
    s.queued = vec![ask, fx.item(1, 0, Normal, 2)];
    s
}

fn over_a_lowered_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 3;
    s.running_total = 5;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Critical, 0, 1)];
    s.queued = vec![fx.item(0, 0, Critical, 1)];
    s
}

fn zero_wanted(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 1;
    s.pending_spawns = vec![fx.pending(0, 7, 0, Critical, 0, 0)];
    s.queued = vec![fx.item(0, 0, Normal, 1)];
    s
}

/// A team stage of `agents` roles for item `i` in repo `r`.
fn team(fx: &Fx, i: usize, r: usize, priority: Priority, seq: u64, agents: u32) -> QueuedCandidate {
    QueuedCandidate {
        first_stage_agents: agents,
        gang: true,
        ..fx.item(i, r, priority, seq)
    }
}

fn gang_fits(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 4;
    s.queued = vec![team(fx, 0, 0, High, 1, 3), fx.item(1, 1, Normal, 2)];
    s
}

fn gang_item_waits_whole(fx: &Fx) -> DispatchSnapshot {
    let mut s = gang_fits(fx);
    s.caps.max_parallel_agents = 3;
    s.running_total = 1;
    s
}

fn gang_spawn_waits_whole(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_parallel_agents = 3;
    s.running_total = 1;
    let mut gang = fx.pending(0, 7, 0, High, 0, 3);
    gang.gang = true;
    s.pending_spawns = vec![gang, fx.pending(1, 6, 1, Normal, 1, 1)];
    s.queued = vec![fx.item(0, 1, Normal, 2)];
    s
}

fn gang_waits_on_its_full_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_repo = 2;
    s.running_total = 1;
    s.running_by_repo.insert(fx.repos[0].clone(), 1);
    s.queued = vec![
        team(fx, 0, 0, High, 1, 2),
        fx.item(1, 0, Normal, 2),
        fx.item(2, 1, Normal, 3),
    ];
    s
}

fn gang_over_its_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = gang_fits(fx);
    s.caps.max_parallel_agents = 2;
    s
}

/// A running team's stage over a per-pipeline cap lowered after enqueue.
fn gang_spawn_over_its_cap(fx: &Fx) -> DispatchSnapshot {
    let mut s = fx.base();
    s.caps.max_per_pipeline.insert("direct".into(), 2);
    let mut gang = fx.pending(0, 7, 0, High, 0, 3);
    gang.gang = true;
    s.pending_spawns = vec![gang, fx.pending(1, 6, 1, Normal, 1, 1)];
    s
}

/// `plan`, with the gangs that can never fit.
fn never_fits(
    mut plan: DispatchPlan,
    waiters: Vec<GangWaiter>,
    wanted: u32,
    cap: u32,
) -> DispatchPlan {
    plan.never_fits = waiters
        .into_iter()
        .map(|waiter| NeverFits {
            waiter,
            wanted,
            cap,
        })
        .collect();
    plan
}

type Case = (&'static str, Build, Expect);

/// Pauses, budgets and dependency states.
fn hold_cases() -> Vec<Case> {
    vec![
        ("empty", |fx| fx.base(), |fx| fx.plan(&[], &[], &[], None)),
        ("global pause holds all but blocking", global_pause, |fx| {
            fx.plan(&[], &[], &[1], Some(Hold::GlobalPause))
        }),
        ("low disk holds all but blocking", low_disk, |fx| {
            fx.plan(&[], &[], &[1], Some(Hold::LowDisk))
        }),
        (
            "a draining upgrade holds all but blocking",
            draining,
            |fx| fx.plan(&[], &[], &[1], Some(Hold::Draining)),
        ),
        (
            "a draining upgrade reported before low disk",
            draining_on_low_disk,
            |fx| fx.plan(&[], &[], &[1], Some(Hold::Draining)),
        ),
        (
            "low disk reported before budgets",
            low_disk_and_budget,
            |fx| fx.plan(&[], &[], &[], Some(Hold::LowDisk)),
        ),
        (
            "global pause reported before low disk",
            low_disk_and_paused,
            |fx| fx.plan(&[], &[], &[1], Some(Hold::GlobalPause)),
        ),
        ("repo pause skips new items only", repo_pause, |fx| {
            fx.plan(&[(0, 2)], &[1], &[], None)
        }),
        (
            "pipeline pause skips new items only",
            pipeline_pause,
            |fx| fx.plan(&[(0, 1)], &[1], &[], None),
        ),
        ("daily budget reached", daily_reached, |fx| {
            fx.plan(&[], &[], &[], Some(Hold::DailyBudget))
        }),
        ("daily budget not yet reached", daily_below, |fx| {
            fx.plan(&[(0, 1)], &[0], &[], None)
        }),
        ("total budget reached", total_reached, |fx| {
            fx.plan(&[], &[], &[], Some(Hold::TotalBudget))
        }),
        ("total budget reported before daily", both_reached, |fx| {
            fx.plan(&[], &[], &[], Some(Hold::TotalBudget))
        }),
        ("budgets off hold nothing", budgets_off, |fx| {
            fx.plan(&[(0, 1)], &[0], &[], None)
        }),
        ("zero budgets are unlimited", budgets_unlimited, |fx| {
            fx.plan(&[(0, 1)], &[0], &[], None)
        }),
        (
            "broken deps block, waiting deps wait",
            deps_block_and_wait,
            |fx| fx.plan(&[], &[2], &[3, 1], None),
        ),
    ]
}

/// Admission order: spawns first, then priority, then age.
fn order_cases() -> Vec<Case> {
    vec![
        ("priority then age", priority_then_age, |fx| {
            fx.plan(&[], &[5, 1, 4, 2, 0, 3], &[], None)
        }),
        (
            "pending spawns before new items",
            spawns_before_items,
            |fx| fx.plan(&[(0, 2)], &[], &[], Some(Hold::Capacity)),
        ),
        (
            "pending spawns by priority then age",
            spawns_by_priority_then_age,
            |fx| fx.plan(&[(1, 2), (2, 1)], &[], &[], Some(Hold::Capacity)),
        ),
        ("spawns wanting nothing are ignored", zero_wanted, |fx| {
            fx.plan(&[], &[0], &[], None)
        }),
    ]
}

/// Global, per-repo, per-pipeline and active-run caps.
fn cap_cases() -> Vec<Case> {
    vec![
        ("fan-out item starts partially", partial_fan_out, |fx| {
            fx.plan(&[], &[0], &[], Some(Hold::Capacity))
        }),
        (
            "fan-out spawn fills partially",
            partial_pending_fan_out,
            |fx| fx.plan(&[(0, 2)], &[], &[], Some(Hold::Capacity)),
        ),
        ("per-repo cap skips to other repos", per_repo_cap, |fx| {
            fx.plan(&[], &[1], &[], Some(Hold::Capacity))
        }),
        (
            "repo file lowers the per-repo cap",
            repo_override_lowers,
            |fx| fx.plan(&[], &[0, 1, 3, 5], &[], Some(Hold::Capacity)),
        ),
        (
            "repo file cap with unlimited global",
            repo_override_without_global,
            |fx| fx.plan(&[], &[0, 1, 3, 5], &[], Some(Hold::Capacity)),
        ),
        ("per-pipeline cap", per_pipeline_cap, |fx| {
            fx.plan(&[(0, 1)], &[1, 2], &[], Some(Hold::Capacity))
        }),
        (
            "active-run cap limits new runs only",
            active_run_cap,
            |fx| fx.plan(&[(0, 4)], &[0], &[], Some(Hold::Capacity)),
        ),
        (
            "repo-less items ignore per-repo caps",
            repoless_items,
            |fx| fx.plan(&[], &[0], &[], Some(Hold::Capacity)),
        ),
        (
            "usage above a lowered cap admits nothing",
            over_a_lowered_cap,
            |fx| fx.plan(&[], &[], &[], Some(Hold::Capacity)),
        ),
    ]
}

/// Team stages (gangs): admitted whole or not at all, and not starved.
fn gang_cases() -> Vec<Case> {
    vec![
        ("a gang that fits starts whole", gang_fits, |fx| {
            fx.plan(&[], &[0, 1], &[], None)
        }),
        (
            "a gang item never starts partially; later work waits",
            gang_item_waits_whole,
            |fx| fx.plan(&[], &[], &[], Some(Hold::Capacity)),
        ),
        (
            "a gang spawn is never granted partially; later work waits",
            gang_spawn_waits_whole,
            |fx| fx.plan(&[], &[], &[], Some(Hold::Capacity)),
        ),
        (
            "a gang holds only the caps too full for it",
            gang_waits_on_its_full_cap,
            |fx| fx.plan(&[], &[2], &[], Some(Hold::Capacity)),
        ),
        (
            "a gang bigger than its cap holds nothing back and is reported",
            gang_over_its_cap,
            |fx| {
                let plan = fx.plan(&[], &[1], &[], Some(Hold::Capacity));
                never_fits(plan, vec![GangWaiter::Item(fx.items[0].clone())], 3, 2)
            },
        ),
        (
            "a running gang over a lowered cap is reported",
            gang_spawn_over_its_cap,
            |fx| {
                let plan = fx.plan(&[(1, 1)], &[], &[], Some(Hold::Capacity));
                never_fits(plan, vec![GangWaiter::Run(fx.runs[0].clone())], 3, 2)
            },
        ),
    ]
}

#[test]
fn plan_dispatch_table() {
    let fx = Fx::new();
    let all = [hold_cases(), order_cases(), cap_cases(), gang_cases()].concat();
    assert_eq!(all.len(), 35);
    for (name, build, expect) in all {
        let snapshot = build(&fx);
        assert_eq!(plan_dispatch(&snapshot), expect(&fx), "case: {name}");
    }
}

#[test]
fn zero_caps_are_unlimited() {
    let fx = Fx::new();
    let mut s = fx.base();
    s.caps = Caps {
        max_parallel_agents: 0,
        max_per_repo: 0,
        max_active_runs: 0,
        ..Caps::default()
    };
    s.caps.max_per_pipeline.insert("direct".into(), 0);
    s.running_total = 1_000;
    s.active_runs = 1_000;
    s.queued = (0..200u64)
        .map(|n| QueuedCandidate {
            item: QueueItemId::new(),
            first_stage_agents: 7,
            ..fx.item(0, (n % 2) as usize, Normal, n)
        })
        .collect();
    s.pending_spawns = vec![fx.pending(0, 7, 0, Normal, 0, 50)];
    let plan = plan_dispatch(&s);
    assert_eq!(plan.start.len(), 200);
    assert_eq!(plan.spawn, vec![(fx.runs[0].clone(), 50)]);
    assert_eq!(plan.held_by, None);
}

#[test]
fn the_effective_agent_cap_is_the_smallest_set_cap() {
    let mut caps = Caps {
        max_parallel_agents: 0,
        max_per_repo: 0,
        ..Caps::default()
    };
    assert_eq!(effective_agent_cap(&caps, None, "direct"), 0, "uncapped");
    assert_eq!(effective_agent_cap(&caps, Some(0), "direct"), 0);
    caps.max_parallel_agents = 8;
    assert_eq!(effective_agent_cap(&caps, Some(0), "direct"), 8);
    caps.max_per_repo = 5;
    assert_eq!(effective_agent_cap(&caps, Some(0), "direct"), 5);
    assert_eq!(
        effective_agent_cap(&caps, None, "direct"),
        8,
        "no repository"
    );
    assert_eq!(
        effective_agent_cap(&caps, Some(3), "direct"),
        3,
        "the repo file lowers it"
    );
    assert_eq!(
        effective_agent_cap(&caps, Some(9), "direct"),
        5,
        "but never raises it"
    );
    caps.max_per_pipeline.insert("refactor".into(), 2);
    assert_eq!(effective_agent_cap(&caps, Some(3), "refactor"), 2);
    assert_eq!(effective_agent_cap(&caps, Some(3), "direct"), 3);
}

fn waiting_case(
    change: impl FnOnce(&mut DispatchSnapshot),
    expected: archon_core::protocol::WaitingReasonCode,
) {
    let fx = Fx::new();
    let mut snapshot = fx.base();
    snapshot.queued.push(fx.item(0, 0, Priority::Normal, 0));
    change(&mut snapshot);
    let reasons = archon_queue::dispatch::waiting_reasons(&snapshot);
    assert_eq!(reasons[&fx.items[0]], expected);
    assert_eq!(reasons.len(), 1);
    let plan = plan_dispatch(&snapshot);
    assert_eq!(
        plan.start.contains(&fx.items[0]),
        expected == archon_core::protocol::WaitingReasonCode::NextInLine
    );
}

#[test]
fn waiting_reason_next_in_line() {
    waiting_case(|_| {}, archon_core::protocol::WaitingReasonCode::NextInLine);
}

#[test]
fn waiting_reason_dependency_waiting() {
    waiting_case(
        |s| s.queued[0].deps = DependencyState::Waiting,
        archon_core::protocol::WaitingReasonCode::DependencyWaiting,
    );
}

#[test]
fn waiting_reason_dependency_failed() {
    waiting_case(
        |s| s.queued[0].deps = DependencyState::Broken,
        archon_core::protocol::WaitingReasonCode::DependencyFailed,
    );
}

#[test]
fn waiting_reason_global_pause() {
    waiting_case(
        |s| s.pause.global = true,
        archon_core::protocol::WaitingReasonCode::GlobalPause,
    );
}

#[test]
fn waiting_reason_repo_pause() {
    waiting_case(
        |s| s.pause.repos.push(s.queued[0].repo.clone().unwrap()),
        archon_core::protocol::WaitingReasonCode::RepoPause,
    );
}

#[test]
fn waiting_reason_pipeline_pause() {
    waiting_case(
        |s| s.pause.pipelines.push("direct".into()),
        archon_core::protocol::WaitingReasonCode::PipelinePause,
    );
}

#[test]
fn waiting_reason_total_budget() {
    waiting_case(
        |s| {
            s.caps.budget_usd_total = 5.0;
            s.spend_total_usd = 5.0;
        },
        archon_core::protocol::WaitingReasonCode::TotalBudget,
    );
}

#[test]
fn waiting_reason_daily_budget() {
    waiting_case(
        |s| {
            s.caps.budget_usd_daily = 5.0;
            s.spend_today_usd = 5.0;
        },
        archon_core::protocol::WaitingReasonCode::DailyBudget,
    );
}

#[test]
fn waiting_reason_ignores_reached_budgets_when_off() {
    waiting_case(
        |s| {
            s.caps.budgets_enabled = false;
            s.caps.budget_usd_total = 5.0;
            s.caps.budget_usd_daily = 5.0;
            s.spend_total_usd = 9.0;
            s.spend_today_usd = 9.0;
        },
        archon_core::protocol::WaitingReasonCode::NextInLine,
    );
}

#[test]
fn waiting_reason_low_disk() {
    waiting_case(
        |s| s.low_disk = true,
        archon_core::protocol::WaitingReasonCode::LowDisk,
    );
}

#[test]
fn waiting_reason_agent_capacity() {
    waiting_case(
        |s| {
            s.caps.max_parallel_agents = 1;
            s.running_total = 1;
        },
        archon_core::protocol::WaitingReasonCode::Capacity,
    );
}

#[test]
fn waiting_reason_repo_override_and_pipeline_capacity() {
    use archon_core::protocol::WaitingReasonCode::Capacity;
    waiting_case(
        |s| {
            let repo = s.queued[0].repo.clone().unwrap();
            s.repo_caps.insert(repo.clone(), 1);
            s.running_by_repo.insert(repo, 1);
        },
        Capacity,
    );
    waiting_case(
        |s| {
            s.caps.max_per_pipeline.insert("direct".into(), 1);
            s.running_by_pipeline.insert("direct".into(), 1);
        },
        Capacity,
    );
    waiting_case(
        |s| {
            s.caps.max_active_runs = 1;
            s.active_runs = 1;
        },
        Capacity,
    );
}

#[test]
fn waiting_reason_precedence_matches_admission_holds() {
    use archon_core::protocol::WaitingReasonCode as Code;
    let fx = Fx::new();
    let mut s = fx.base();
    s.queued.push(fx.item(0, 0, Priority::Normal, 0));
    s.queued[0].deps = DependencyState::Broken;
    s.pause.global = true;
    s.pause.repos.push(fx.repos[0].clone());
    s.pause.pipelines.push("direct".into());
    s.low_disk = true;
    s.caps.budget_usd_total = 1.0;
    s.caps.budget_usd_daily = 1.0;
    s.spend_total_usd = 2.0;
    s.spend_today_usd = 2.0;
    let reason = |s: &DispatchSnapshot| archon_queue::dispatch::waiting_reasons(s)[&fx.items[0]];
    assert_eq!(reason(&s), Code::DependencyFailed);
    s.queued[0].deps = DependencyState::Waiting;
    assert_eq!(reason(&s), Code::DependencyWaiting);
    s.queued[0].deps = DependencyState::AllSucceeded;
    assert_eq!(reason(&s), Code::GlobalPause);
    s.pause.global = false;
    assert_eq!(reason(&s), Code::LowDisk);
    s.low_disk = false;
    assert_eq!(reason(&s), Code::TotalBudget);
    s.caps.budget_usd_total = 0.0;
    assert_eq!(reason(&s), Code::DailyBudget);
    s.caps.budget_usd_daily = 0.0;
    assert_eq!(reason(&s), Code::RepoPause);
    s.pause.repos.clear();
    assert_eq!(reason(&s), Code::PipelinePause);
}

#[test]
fn waiting_capacity_accounts_for_pending_spawns_and_earlier_items() {
    use archon_core::protocol::WaitingReasonCode as Code;
    let fx = Fx::new();
    let mut s = fx.base();
    s.caps.max_parallel_agents = 1;
    s.queued = vec![
        fx.item(0, 0, Priority::Normal, 0),
        fx.item(1, 1, Priority::Low, 1),
    ];
    let reasons = archon_queue::dispatch::waiting_reasons(&s);
    assert_eq!(reasons[&fx.items[0]], Code::NextInLine);
    assert_eq!(reasons[&fx.items[1]], Code::Capacity);
    s.pending_spawns
        .push(fx.pending(0, 2, 0, Priority::Normal, 0, 1));
    let reasons = archon_queue::dispatch::waiting_reasons(&s);
    assert!(reasons.values().all(|r| *r == Code::Capacity));
}

#[test]
fn waiting_capacity_accounts_for_gang_fairness_and_never_fitting_teams() {
    use archon_core::protocol::WaitingReasonCode as Code;
    let fx = Fx::new();
    let mut s = fx.base();
    s.caps.max_parallel_agents = 2;
    s.running_total = 1;
    s.queued = vec![
        fx.item(0, 0, Priority::High, 0),
        fx.item(1, 1, Priority::Normal, 1),
    ];
    s.queued[0].gang = true;
    s.queued[0].first_stage_agents = 2;
    let reasons = archon_queue::dispatch::waiting_reasons(&s);
    assert!(
        reasons.values().all(|r| *r == Code::Capacity),
        "gang holds later work"
    );
    s.queued[0].first_stage_agents = 3;
    let reasons = archon_queue::dispatch::waiting_reasons(&s);
    assert_eq!(reasons[&fx.items[0]], Code::Capacity);
    assert_eq!(
        reasons[&fx.items[1]],
        Code::NextInLine,
        "impossible gang holds no slots"
    );
    s.queued.clear();
    assert!(archon_queue::dispatch::waiting_reasons(&s).is_empty());
}

#[test]
fn repository_budget_switch_controls_starts_and_pending_spawns() {
    let fx = Fx::new();
    let mut snapshot = fx.base();
    snapshot.caps.budget_usd_total = 1.0;
    snapshot.spend_total_usd = 2.0;
    snapshot.queued = vec![
        fx.item(0, 0, Priority::Normal, 0),
        fx.item(1, 1, Priority::Normal, 1),
    ];
    snapshot.pending_spawns = vec![
        fx.pending(2, 0, 0, Priority::Normal, 0, 1),
        fx.pending(3, 1, 1, Priority::Normal, 1, 1),
    ];
    snapshot
        .repo_budgets_enabled
        .insert(fx.repos[0].clone(), false);
    let plan = plan_dispatch(&snapshot);
    assert_eq!(plan.start, vec![fx.items[0].clone()]);
    assert_eq!(plan.spawn, vec![(fx.runs[2].clone(), 1)]);
    let reasons = archon_queue::dispatch::waiting_reasons(&snapshot);
    assert_eq!(
        reasons[&fx.items[1]],
        archon_core::protocol::WaitingReasonCode::TotalBudget
    );
    snapshot.caps.budgets_enabled = false;
    snapshot
        .repo_budgets_enabled
        .insert(fx.repos[1].clone(), true);
    assert_eq!(plan_dispatch(&snapshot).start, vec![fx.items[0].clone()]);
    snapshot.pause.global = true;
    assert!(plan_dispatch(&snapshot).start.is_empty());
    assert!(plan_dispatch(&snapshot).spawn.is_empty());
}

#[test]
fn operator_repo_cap_can_raise_the_default_but_the_file_can_still_lower_it() {
    let fx = Fx::new();
    let mut snapshot = fx.base();
    snapshot.caps.max_per_repo = 1;
    snapshot.running_by_repo.insert(fx.repos[0].clone(), 1);
    snapshot.queued = vec![fx.item(0, 0, Priority::Normal, 0)];
    assert!(plan_dispatch(&snapshot).start.is_empty());
    snapshot.repo_max_per_repo.insert(fx.repos[0].clone(), 2);
    assert_eq!(plan_dispatch(&snapshot).start, vec![fx.items[0].clone()]);
    snapshot.repo_caps.insert(fx.repos[0].clone(), 1);
    assert!(plan_dispatch(&snapshot).start.is_empty());
    snapshot.repo_caps.clear();
    snapshot.running_by_repo.clear();
    snapshot.queued[0].first_stage_agents = 2;
    snapshot.queued[0].gang = true;
    let plan = plan_dispatch(&snapshot);
    assert_eq!(plan.start, vec![fx.items[0].clone()]);
    assert!(plan.never_fits.is_empty());
}
