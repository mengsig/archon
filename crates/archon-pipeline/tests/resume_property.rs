//! The resume property over many generated runs: random agent outcomes,
//! random partial admission of fan-out slots (team groups whole or not at
//! all), infra retries, team siblings cancelled by a failed role, occasional
//! cancels, random operator decisions and random team merge outcomes, on
//! every built-in pipeline, a majority-gated one, a team one, and pipelines
//! with both operator gates. `sim::Sim`
//! checks at every
//! step that a driver rebuilt from persisted stage runs owes exactly what the
//! live driver returned, that no slot is spawned twice, nothing is spawned
//! after the run finished, and a run finishes once.

mod sim;
mod support;

use std::sync::Arc;

use archon_core::autonomy::{Autonomy, ProposalMode, ReportMode};
use archon_core::run::GateDecision;
use archon_pipeline::ShipOutcome;
use archon_pipeline::autonomy::{OperatorGates, augment};
use archon_pipeline::builtin::BUILTINS;
use archon_pipeline::spec::PipelineSpec;
use archon_pipeline::team::MergeOutcome;
use sim::{Agent, Sim};

const SEEDS: u64 = 150;
const MAX_STEPS: usize = 200;

/// A small deterministic generator; no dependency needed for this.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// Mostly progress, with every failure shape represented.
fn random_agent(rng: &mut Lcg) -> Agent {
    match rng.below(100) {
        0..=34 => Agent::Pass,
        35..=54 => Agent::Done("done"),
        55..=74 => Agent::Fail("a defect"),
        75..=79 => Agent::SummaryVerdict("fine\nVERDICT: PASS"),
        80..=84 => Agent::NoVerdict,
        85..=89 => Agent::Error("tool crashed"),
        90..=92 => Agent::Blocked("needs a token"),
        93..=97 => Agent::TimedOut,
        _ => Agent::Cancelled,
    }
}

fn majority_spec() -> PipelineSpec {
    support::spec_from(
        r#"
        [pipeline]
        name = "vote"
        description = "majority vote with a loop"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        fan_out = 5
        gate = "majority"
        on_fail = { goto = "build", max_loops = 3 }
        [[stage]]
        id = "report"
        kind = "report"
        prompt = "report.md"
        fan_out = 2
        gate = "any"
        on_fail = { goto = "review", max_loops = 2 }
        "#,
        &[],
    )
}

/// A team stage between a plan and a majority review, with loops both ways.
fn team_spec() -> PipelineSpec {
    let spec = support::spec_from(
        r#"
        [pipeline]
        name = "team"
        description = "three roles, merged, then reviewed"
        version = 1
        [[stage]]
        id = "plan"
        kind = "work"
        prompt = "plan.md"
        [[stage]]
        id = "build"
        kind = "team"
        prompt = "build.md"
        roles = ["api", "ui", "docs"]
        on_fail = { goto = "plan", max_loops = 2 }
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        fan_out = 2
        gate = "majority"
        on_fail = { goto = "build", max_loops = 2 }
        [[stage]]
        id = "ship"
        kind = "ship"
        prompt = "ship.md"
        "#,
        &[("build.md", "{{team.role}} in {{workspace}}\n{{feedback}}")],
    );
    spec.validate("team").expect("valid team spec");
    spec
}

/// Mostly clean merges, with a conflict or a refusal on a random role.
fn random_merge(rng: &mut Lcg, roles: &[String]) -> MergeOutcome {
    let pick = rng.below(10);
    if pick < 7 {
        return MergeOutcome::Merged;
    }
    let role = roles[rng.below(roles.len() as u64) as usize].clone();
    match pick {
        7 => MergeOutcome::Refused {
            role,
            reason: "its worktree is on another branch".into(),
        },
        _ => MergeOutcome::Conflict {
            role,
            files: vec!["src/lib.rs".into()],
        },
    }
}

/// Mostly approvals, with revisions and the odd rejection.
/// How archond's delivery ends: mostly shipped, sometimes refused.
fn random_delivery(rng: &mut Lcg) -> ShipOutcome {
    let commit = sim::COMMIT.to_owned();
    match rng.below(4) {
        0 => ShipOutcome::Failed {
            error: "the remote branch moved".into(),
        },
        1 => ShipOutcome::Shipped {
            commit,
            pr_url: None,
        },
        _ => ShipOutcome::Shipped {
            commit,
            pr_url: Some("https://github.com/o/r/pull/7".into()),
        },
    }
}

fn random_decision(rng: &mut Lcg) -> GateDecision {
    match rng.below(100) {
        0..=54 => GateDecision::Approve { note: None },
        55..=64 => GateDecision::Approve {
            note: Some("keep it small".into()),
        },
        65..=91 => GateDecision::Revise {
            feedback: "split it in two".into(),
        },
        _ => GateDecision::Reject {
            reason: "not now".into(),
        },
    }
}

/// `base` with both operator gates inserted by the knobs.
fn gated(base: &PipelineSpec) -> (Arc<PipelineSpec>, OperatorGates) {
    let knobs = Autonomy {
        proposal: ProposalMode::Lavish,
        report: ReportMode::BeforePr,
        ..Autonomy::default()
    };
    let spec = augment(base, &knobs).expect("ships");
    let gates = OperatorGates::for_run(&spec, &knobs);
    assert!(gates.proposal.is_some() && gates.report.is_some());
    (Arc::new(spec), gates)
}

/// Drive one run to its end with random events; returns the step count,
/// how many team merges it took and whether archond delivered it.
fn run_random(spec: Arc<PipelineSpec>, gates: OperatorGates, seed: u64) -> (usize, usize, bool) {
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    let mut sim = Sim::with_gates(spec, plan, gates);
    let mut rng = Lcg(seed);
    sim.start_partial(1 + rng.below(3) as usize);
    for step in 0..MAX_STEPS {
        if sim.finished().is_some() {
            return (step, sim.merges.len(), sim.shipped.is_some());
        }
        if sim.open_ship.is_some() {
            sim.ship(random_delivery(&mut rng));
            continue;
        }
        if sim.open_gate.is_some() {
            sim.decide(random_decision(&mut rng));
            continue;
        }
        if let Some(plan) = sim.open_merge.clone() {
            let roles: Vec<String> = plan.roles.into_iter().map(|r| r.role).collect();
            sim.merge(random_merge(&mut rng, &roles));
            continue;
        }
        if !sim.pending.is_empty() && (sim.live().is_empty() || rng.below(3) == 0) {
            let mut count = 1 + rng.below(2) as usize;
            if sim.live().is_empty() && sim.pending_team() {
                count = sim.pending.len();
            }
            sim.admit(count);
        }
        if rng.below(60) == 0 {
            sim.cancel("random cancel");
            assert!(sim.finished().is_some(), "cancel finishes the run");
            return (step, sim.merges.len(), false);
        }
        let live = sim.live();
        assert!(
            !live.is_empty(),
            "an unfinished run always has live or pending work"
        );
        let pick = live[rng.below(live.len() as u64) as usize];
        let (stage, fan) = (pick.stage_id.clone(), pick.fan_index);
        if rng.below(15) == 0 {
            sim.infra_respawn(&stage, fan);
            continue;
        }
        let agent = random_agent(&mut rng);
        let admit = rng.below(4) as usize;
        sim.settle_admitting(&stage, fan, agent, admit);
    }
    panic!("run did not finish within {MAX_STEPS} steps");
}

#[test]
fn resume_matches_the_live_driver_on_generated_runs() {
    let mut specs: Vec<(Arc<PipelineSpec>, OperatorGates)> = BUILTINS
        .iter()
        .map(|b| (support::builtin(b.name), OperatorGates::default()))
        .collect();
    specs.push((Arc::new(majority_spec()), OperatorGates::default()));
    specs.push((Arc::new(team_spec()), OperatorGates::default()));
    specs.push(gated(&support::builtin("default-pr-review")));
    specs.push(gated(&support::builtin("strict-pr-review")));
    specs.push(gated(&team_spec()));
    let (mut total_steps, mut total_merges, mut deliveries) = (0, 0, 0);
    for (spec, gates) in &specs {
        for seed in 0..SEEDS {
            let (steps, merges, delivered) =
                run_random(spec.clone(), gates.clone(), seed.wrapping_mul(7919) + 1);
            total_steps += steps;
            total_merges += merges;
            deliveries += usize::from(delivered);
        }
    }
    assert!(deliveries > SEEDS as usize, "runs reach archond's delivery");
    assert!(
        total_merges > SEEDS as usize,
        "team runs reach their merges"
    );
    assert!(
        total_steps > specs.len() * SEEDS as usize,
        "generated runs should take more than one step on average"
    );
}
