//! Property checks of `plan_dispatch` over thousands of random snapshots.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::queue::{Caps, PauseState, Priority};
use archon_queue::dispatch::{
    DependencyState, DispatchPlan, DispatchSnapshot, Hold, PendingSpawn, QueuedCandidate,
    plan_dispatch,
};
use common::Rng;

const PIPELINES: [&str; 3] = ["direct", "strict", "ask"];
const PRIORITIES: [Priority; 4] = [
    Priority::Low,
    Priority::Normal,
    Priority::High,
    Priority::Critical,
];

fn small_cap(rng: &mut Rng) -> u32 {
    [0, 1, 2, 3, 5][rng.below(5)] as u32
}

fn random_caps(rng: &mut Rng, repos: &[RepoId]) -> (Caps, BTreeMap<RepoId, u32>) {
    let mut caps = Caps {
        max_parallel_agents: small_cap(rng) * 2,
        max_per_repo: small_cap(rng),
        max_active_runs: small_cap(rng) * 2,
        ..Caps::default()
    };
    for pipeline in PIPELINES {
        if rng.below(2) == 0 {
            caps.max_per_pipeline
                .insert(pipeline.into(), small_cap(rng));
        }
    }
    let mut repo_caps = BTreeMap::new();
    for repo in repos {
        if rng.below(3) == 0 {
            repo_caps.insert(repo.clone(), small_cap(rng));
        }
    }
    (caps, repo_caps)
}

fn random_candidate(rng: &mut Rng, repos: &[RepoId], seq: u64) -> QueuedCandidate {
    let deps = [
        DependencyState::AllSucceeded,
        DependencyState::AllSucceeded,
        DependencyState::Waiting,
        DependencyState::Broken,
    ];
    QueuedCandidate {
        item: QueueItemId::new(),
        repo: (rng.below(5) > 0).then(|| repos[rng.below(repos.len())].clone()),
        pipeline: PIPELINES[rng.below(3)].into(),
        priority: PRIORITIES[rng.below(4)],
        created_seq: seq,
        deps: deps[rng.below(4)],
        first_stage_agents: 1 + rng.below(3) as u32,
        gang: rng.below(4) == 0,
        quota_hold: None,
    }
}

fn random_pending(rng: &mut Rng, repos: &[RepoId], seq: u64) -> PendingSpawn {
    PendingSpawn {
        run: PipelineRunId::new(),
        item: QueueItemId::new(),
        repo: (rng.below(5) > 0).then(|| repos[rng.below(repos.len())].clone()),
        pipeline: PIPELINES[rng.below(3)].into(),
        priority: PRIORITIES[rng.below(4)],
        item_created_seq: seq,
        wanted: rng.below(4) as u32,
        gang: rng.below(3) == 0,
        quota_hold: None,
    }
}

/// Running counts are consistent: per-repo and per-pipeline sums never
/// exceed the total.
fn random_running(rng: &mut Rng, s: &mut DispatchSnapshot, repos: &[RepoId]) {
    for _ in 0..rng.below(6) {
        s.running_total += 1;
        *s.running_by_pipeline
            .entry(PIPELINES[rng.below(3)].into())
            .or_default() += 1;
        if rng.below(4) > 0 {
            *s.running_by_repo
                .entry(repos[rng.below(repos.len())].clone())
                .or_default() += 1;
        }
    }
    s.active_runs = rng.below(5) as u32;
}

fn random_snapshot(rng: &mut Rng) -> DispatchSnapshot {
    let repos: Vec<RepoId> = (0..3).map(|_| RepoId::new()).collect();
    let (caps, repo_caps) = random_caps(rng, &repos);
    let mut s = DispatchSnapshot {
        caps,
        repo_caps,
        repo_budgets_enabled: BTreeMap::new(),
        repo_max_per_repo: BTreeMap::new(),
        pause: PauseState {
            global: rng.below(10) == 0,
            repos: repos
                .iter()
                .filter(|_| rng.below(5) == 0)
                .cloned()
                .collect(),
            pipelines: PIPELINES
                .iter()
                .filter(|_| rng.below(6) == 0)
                .map(|p| p.to_string())
                .collect(),
        },
        low_disk: rng.below(12) == 0,
        draining: rng.below(12) == 0,
        running_total: 0,
        running_by_repo: BTreeMap::new(),
        running_by_pipeline: BTreeMap::new(),
        active_runs: 0,
        spend_today_usd: 0.0,
        spend_total_usd: 0.0,
        pending_spawns: (0..rng.below(4))
            .map(|n| random_pending(rng, &repos, n as u64))
            .collect(),
        queued: (0..rng.below(10))
            .map(|n| random_candidate(rng, &repos, 100 + n as u64))
            .collect(),
    };
    random_running(rng, &mut s, &repos);
    if rng.below(8) == 0 {
        s.caps.budget_usd_daily = 10.0;
        s.spend_today_usd = [9.0, 10.0, 11.0][rng.below(3)];
    }
    s
}

fn eligible(s: &DispatchSnapshot, c: &QueuedCandidate) -> bool {
    c.deps == DependencyState::AllSucceeded
        && !s.pause.pipelines.contains(&c.pipeline)
        && c.repo.as_ref().is_none_or(|r| !s.pause.repos.contains(r))
}

fn repo_cap(s: &DispatchSnapshot, repo: &RepoId) -> u32 {
    match (
        s.caps.max_per_repo,
        s.repo_caps.get(repo).copied().unwrap_or(0),
    ) {
        (global, 0) => global,
        (0, own) => own,
        (global, own) => global.min(own),
    }
}

/// Every admitted agent counts once; a started item counts at least one,
/// and a team (gang) all of its agents. A gang's spawns are granted whole.
/// Where usage started within a cap, it stays within it.
fn check_caps(s: &DispatchSnapshot, plan: &DispatchPlan) {
    let runs: BTreeMap<_, _> = s.pending_spawns.iter().map(|p| (&p.run, p)).collect();
    let items: BTreeMap<_, _> = s.queued.iter().map(|c| (&c.item, c)).collect();
    let mut total = s.running_total;
    let mut by_repo = s.running_by_repo.clone();
    let mut by_pipeline = s.running_by_pipeline.clone();
    let mut add = |repo: &Option<RepoId>, pipeline: &str, n: u32| {
        total += n;
        *by_pipeline.entry(pipeline.to_owned()).or_default() += n;
        if let Some(r) = repo {
            *by_repo.entry(r.clone()).or_default() += n;
        }
    };
    for (run, n) in &plan.spawn {
        let p = runs[run];
        assert!(*n > 0 && *n <= p.wanted, "grant {n} of {}", p.wanted);
        assert!(
            !p.gang || *n == p.wanted,
            "a gang granted {n} of {}",
            p.wanted
        );
        add(&p.repo, &p.pipeline, *n);
    }
    for id in &plan.start {
        let item = items[id];
        let agents = if item.gang {
            item.first_stage_agents
        } else {
            1
        };
        add(&item.repo, &item.pipeline, agents);
    }
    let within = |cap: u32, before: u32, after: u32| cap == 0 || after <= cap.max(before);
    assert!(
        within(s.caps.max_parallel_agents, s.running_total, total),
        "global cap"
    );
    for (repo, used) in &by_repo {
        let before = s.running_by_repo.get(repo).copied().unwrap_or(0);
        assert!(within(repo_cap(s, repo), before, *used), "repo cap");
    }
    for (pipeline, used) in &by_pipeline {
        let cap = s.caps.max_per_pipeline.get(pipeline).copied().unwrap_or(0);
        let before = s.running_by_pipeline.get(pipeline).copied().unwrap_or(0);
        assert!(within(cap, before, *used), "pipeline cap {pipeline}");
    }
    let runs_after = s.active_runs + plan.start.len() as u32;
    assert!(
        within(s.caps.max_active_runs, s.active_runs, runs_after),
        "active-run cap"
    );
}

fn check_rules(s: &DispatchSnapshot, plan: &DispatchPlan) {
    let broken: BTreeSet<_> = s
        .queued
        .iter()
        .filter(|c| c.deps == DependencyState::Broken)
        .map(|c| &c.item)
        .collect();
    assert_eq!(plan.block.iter().collect::<BTreeSet<_>>(), broken);
    let items: BTreeMap<_, _> = s.queued.iter().map(|c| (&c.item, c)).collect();
    assert!(
        plan.start.iter().all(|id| eligible(s, items[id])),
        "started an ineligible item"
    );
    let unique: BTreeSet<_> = plan.start.iter().collect();
    assert_eq!(unique.len(), plan.start.len());
    let order: Vec<_> = plan
        .start
        .iter()
        .map(|id| (std::cmp::Reverse(items[id].priority), items[id].created_seq))
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "starts out of order"
    );
    match plan.held_by {
        Some(
            Hold::GlobalPause
            | Hold::Draining
            | Hold::LowDisk
            | Hold::DailyBudget
            | Hold::TotalBudget,
        ) => {
            assert!(plan.spawn.is_empty() && plan.start.is_empty());
        }
        None => {
            let all_eligible = s.queued.iter().filter(|c| eligible(s, c)).count();
            assert_eq!(
                plan.start.len(),
                all_eligible,
                "unheld plan left an item behind"
            );
            let wanted: u32 = s.pending_spawns.iter().map(|p| p.wanted).sum();
            assert_eq!(plan.spawn.iter().map(|(_, n)| n).sum::<u32>(), wanted);
        }
        Some(Hold::Capacity) => {}
        Some(Hold::ProviderQuota) => unreachable!("random snapshots hold nothing for quota"),
    }
    assert_eq!(plan.held_by == Some(Hold::GlobalPause), s.pause.global);
    assert_eq!(
        plan.held_by == Some(Hold::Draining),
        s.draining && !s.pause.global
    );
    assert_eq!(
        plan.held_by == Some(Hold::LowDisk),
        s.low_disk && !s.draining && !s.pause.global
    );
}

#[test]
fn random_plans_respect_caps_and_rules() {
    let mut rng = Rng::new(0xd15_7a7c);
    let mut held = BTreeMap::new();
    for _ in 0..5_000 {
        let s = random_snapshot(&mut rng);
        let plan = plan_dispatch(&s);
        check_caps(&s, &plan);
        check_rules(&s, &plan);
        *held.entry(format!("{:?}", plan.held_by)).or_insert(0) += 1;
    }
    assert_eq!(held.len(), 6, "every outcome is exercised: {held:?}");
}

/// Admission order depends on priority and age, never on input order.
#[test]
fn plans_do_not_depend_on_input_order() {
    let mut rng = Rng::new(42);
    for _ in 0..2_000 {
        let s = random_snapshot(&mut rng);
        let mut shuffled = s.clone();
        shuffled.queued.reverse();
        shuffled.pending_spawns.reverse();
        let n = shuffled.queued.len();
        if n > 1 {
            shuffled.queued.swap(0, rng.below(n));
        }
        assert_eq!(plan_dispatch(&s), plan_dispatch(&shuffled));
    }
}

/// With only the global and active-run caps, admission is strictly by
/// rank: the started items are a prefix of the eligible items' order,
/// leaving out teams bigger than the global cap, which never fit.
#[test]
fn without_slice_caps_starts_are_a_rank_prefix() {
    let mut rng = Rng::new(7);
    for _ in 0..2_000 {
        let mut s = random_snapshot(&mut rng);
        s.caps.max_per_repo = 0;
        s.caps.max_per_pipeline.clear();
        s.repo_caps.clear();
        let plan = plan_dispatch(&s);
        let global = s.caps.max_parallel_agents;
        let never_fits =
            |c: &QueuedCandidate| c.gang && global != 0 && c.first_stage_agents > global;
        let mut ranked: Vec<&QueuedCandidate> = s
            .queued
            .iter()
            .filter(|c| eligible(&s, c) && !never_fits(c))
            .collect();
        ranked.sort_by_key(|c| (std::cmp::Reverse(c.priority), c.created_seq));
        let prefix: Vec<_> = ranked
            .iter()
            .take(plan.start.len())
            .map(|c| c.item.clone())
            .collect();
        assert_eq!(plan.start, prefix);
    }
}
