//! What a built-in stage costs before it reads anything: its contract and
//! its prompt template, in bytes. Every stage pays them on every attempt,
//! so the budgets here fail a change that lets them grow back unnoticed.
//! `cargo test -p archon-pipeline --test contract_bytes -- --nocapture`
//! prints the table.

use std::path::Path;

use archon_core::catalog::StageKind;
use archon_pipeline::Catalog;
use archon_pipeline::contract::{self, TeamPart};
use archon_pipeline::spec::{PipelineSpec, StageSpec};

const WORKSPACE: &str = "/worktree";
/// The item's directory for reviewers' experiment copies, as terse as `WORKSPACE`.
const EXPERIMENTS: &str = "/experiments";
/// A model policy in words, about as long as a real allowlist's.
const POLICY: &str = "any model the policy allows";

/// Byte budgets, a little above what the built-ins weigh today. A work
/// stage's carries the check-reuse rule.
const PLAIN_BUDGET: usize = 3_560;
const REVIEW_BUDGET: usize = 3_550;
const RECORD_BUDGET: usize = 1_550;
/// What holding a review adds: the record, the verdict and risk call each
/// reviewer returns, the blocking class and the round cap.
const REVIEW_HOLDER_BUDGET: usize = 2_100;
/// What a work stage of a team pipeline adds: committing where the roles fork.
const RUN_TREE_BUDGET: usize = 550;
/// What judging the change's risk adds: the rubric, to the record stage and
/// to a review stage after it.
const RISK_BUDGET: usize = 1_350;
const TOTAL_BUDGET: usize = 120_000;

/// One built-in stage's fixed input, in bytes.
struct Cost {
    pipeline: String,
    stage: String,
    kind: StageKind,
    contract: usize,
    delegation: usize,
    prompt: usize,
}

fn contract_of(spec: &PipelineSpec, stage: &StageSpec) -> String {
    let team = spec.file.stages.iter().any(|s| s.kind == StageKind::Team);
    let part = match team {
        true => TeamPart::RunTree { branch: "archon/x" },
        false => TeamPart::Solo,
    };
    let mode = spec.file.pipeline.workspace;
    let experiments = Some(Path::new(EXPERIMENTS));
    contract::instructions(stage, mode, WORKSPACE, part, spec.owed(stage), experiments)
}

/// Every built-in stage but a team's roles, whose contract needs a slot.
fn costs() -> Vec<Cost> {
    let catalog = Catalog::builtin();
    let mut out = Vec::new();
    for name in catalog.names() {
        let spec = catalog.get(name).expect("a listed built-in loads");
        let mode = spec.file.pipeline.workspace;
        for stage in spec
            .file
            .stages
            .iter()
            .filter(|s| s.kind != StageKind::Team)
        {
            let delegates = stage.allow_delegation
                || spec
                    .review_duty(stage)
                    .is_some_and(|duty| duty.reviewers > 0);
            let delegation = match delegates {
                true => {
                    contract::delegation_rules(mode, POLICY, 2, Some(Path::new(EXPERIMENTS))).len()
                }
                false => 0,
            };
            out.push(Cost {
                pipeline: name.to_owned(),
                stage: stage.id.clone(),
                kind: stage.kind,
                contract: contract_of(spec, stage).len(),
                delegation,
                prompt: spec.prompts[&stage.prompt].len(),
            });
        }
    }
    assert!(out.len() > 20, "the built-ins have dozens of stages");
    out
}

fn cost_of<'a>(costs: &'a [Cost], pipeline: &str, stage: &str) -> &'a Cost {
    let found = costs
        .iter()
        .find(|c| c.pipeline == pipeline && c.stage == stage);
    found.unwrap_or_else(|| panic!("{pipeline} has no stage {stage}"))
}

/// The most a stage's contract may weigh; the record stage's and a review
/// holder's carry more rules than a plain stage's.
fn budget(cost: &Cost, owes_more: bool) -> usize {
    match (cost.kind, owes_more) {
        (StageKind::Review | StageKind::Test, false) => REVIEW_BUDGET,
        (StageKind::Review | StageKind::Test, true) => REVIEW_BUDGET + RECORD_BUDGET,
        (_, false) => PLAIN_BUDGET,
        (_, true) => PLAIN_BUDGET + RECORD_BUDGET,
    }
}

#[test]
fn built_in_contracts_stay_within_their_byte_budgets() {
    let catalog = Catalog::builtin();
    let costs = costs();
    let mut total = 0;
    let mut over = Vec::new();
    for cost in &costs {
        total += cost.contract + cost.delegation;
        println!(
            "BYTES {:<20} {:<10} {:<8} contract={:>5} delegation={:>5} prompt={:>5}",
            cost.pipeline,
            cost.stage,
            format!("{:?}", cost.kind).to_lowercase(),
            cost.contract,
            cost.delegation,
            cost.prompt
        );
        let spec = catalog
            .get(&cost.pipeline)
            .expect("a listed built-in loads");
        let owed = spec.owed(spec.stage(&cost.stage).expect("a listed stage"));
        let holds_review = owed.review.is_some_and(|duty| duty.reviewers > 0);
        let mut budget = budget(cost, owed.ship_record || owed.review.is_some());
        if holds_review {
            budget += REVIEW_HOLDER_BUDGET;
        }
        let team = spec.file.stages.iter().any(|s| s.kind == StageKind::Team);
        if team && cost.kind == StageKind::Work {
            budget += RUN_TREE_BUDGET;
        }
        if owed.ship_record || cost.kind == StageKind::Review {
            budget += RISK_BUDGET;
        }
        if cost.contract > budget {
            over.push(format!(
                "{}/{}: {} B",
                cost.pipeline, cost.stage, cost.contract
            ));
        }
    }
    println!("BYTES total contract+delegation = {total}");
    assert!(over.is_empty(), "contracts over their budget: {over:?}");
    // 139,966 B while every shipping pipeline ended in a ship agent.
    assert!(total <= TOTAL_BUDGET, "built-in contracts total {total} B");
}

/// `direct` is the cheapest path to a shipped change: one leaf agent with
/// no delegation rules, a short prompt, and no later stage.
#[test]
fn direct_is_the_cheapest_shipping_pipeline() {
    let costs = costs();
    let direct = cost_of(&costs, "direct", "implement");
    assert_eq!(direct.delegation, 0, "no reviewer at its own review level");
    // 1,600 B before the prompt carried the PR-body attribution rule.
    assert!(direct.prompt <= 1_700, "prompt is {} B", direct.prompt);
    let fixed = |pipeline: &str| -> usize {
        let stages = costs.iter().filter(|c| c.pipeline == pipeline);
        stages.map(|c| c.contract + c.delegation + c.prompt).sum()
    };
    let shipping = ["adaptive", "refactor", "performance", "swarm"];
    for other in shipping {
        assert!(
            fixed("direct") < fixed(other),
            "direct ({} B) is not cheaper than {other} ({} B)",
            fixed("direct"),
            fixed(other)
        );
    }
}

/// No built-in spends an agent on delivery, and each that delivers says
/// which stage's result archond ships.
#[test]
fn no_built_in_declares_a_ship_stage() {
    let catalog = Catalog::builtin();
    let mut shipping = 0;
    for name in catalog.names() {
        let spec = catalog.get(name).expect("a listed built-in loads");
        let agents = spec
            .file
            .stages
            .iter()
            .filter(|s| s.kind == StageKind::Ship);
        assert_eq!(agents.count(), 0, "{name} still has a ship agent");
        let Some(record) = spec.ship_record_stage() else {
            continue;
        };
        shipping += 1;
        let text = contract_of(spec, record);
        assert!(text.contains("\n## Ship record\n"), "{name}/{}", record.id);
        assert!(
            spec.prompts[&record.prompt].contains("your contract requires"),
            "{name}/{}: its prompt never points at the ship record",
            record.id
        );
    }
    assert_eq!(shipping, 5, "every repository-changing built-in ships");
}
