//! The runner's aggregate file, schema `aether-release-bench-aggregate/1`
//! (aether `evals/release_bench_aggregate.py`), turned into archon-bench's
//! [`Aggregate`]. Both splits write the same schema; only dev adds
//! per-instance verdicts. Aether's own gates, verdict and stop reason are
//! skipped unread: their reasons may name a task.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::runner_arm;
use crate::swe::aggregate::{self, Aggregate, ArmTotals, PairCounts, Split};
use crate::swe::arms::ArmKey;
use crate::swe::manifest;

pub const SCHEMA: &str = "aether-release-bench-aggregate/1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "an IgnoredAny field only requires its key; its value is skipped unread"
)]
struct Output {
    schema: String,
    set: Set,
    settings: IgnoredAny,
    arms: BTreeMap<String, Arm>,
    pairs: Vec<Pair>,
    gates: IgnoredAny,
    overall: IgnoredAny,
    stop_reason: IgnoredAny,
    #[serde(default)]
    instances: Option<Vec<Instance>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "an IgnoredAny field only requires its key; its value is skipped unread"
)]
struct Set {
    name: String,
    split: Split,
    sha256: String,
    tasks: u32,
    excluded: IgnoredAny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "an IgnoredAny field only requires its key; its value is skipped unread"
)]
struct Arm {
    model: String,
    harness: String,
    trials: u32,
    infra: u32,
    n: u32,
    passed: u32,
    pass_rate: IgnoredAny,
    pass_ci: IgnoredAny,
    cost_usd_total: Option<f64>,
    unpriced: IgnoredAny,
    cost_per_solved_usd: IgnoredAny,
    wall_seconds_total: Option<f64>,
    wall_per_solved_seconds: IgnoredAny,
}

/// `b` and `c` are McNemar's: trial pairs only the baseline, or only the
/// candidate, passed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "an IgnoredAny field only requires its key; its value is skipped unread"
)]
struct Pair {
    model: String,
    baseline: String,
    candidate: String,
    tasks: u32,
    pairs: u32,
    b: u32,
    c: u32,
    p_one_sided: IgnoredAny,
    pass_diff: IgnoredAny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    task: String,
    arm: String,
    repeat: u32,
    passed: bool,
}

/// Parse and check the runner's aggregate of a run of `expected` arms with
/// `repeats` repeats, of the first `limit` tasks if a dev smoke. Holdout
/// errors name only a position, never content.
pub fn parse(
    text: &str,
    split: Split,
    repeats: u32,
    limit: Option<u32>,
    expected: &[ArmKey],
) -> anyhow::Result<Aggregate> {
    let output: Output = aggregate::parse_json(text, split, SCHEMA)?;
    ensure!(
        output.schema == SCHEMA,
        "the runner's aggregate is not {SCHEMA}"
    );
    check_set(&output.set, split)?;
    let names: BTreeMap<String, ArmKey> = expected.iter().map(|&k| (runner_arm(k), k)).collect();
    ensure!(
        names.len() == expected.len(),
        "the requested arms are distinct"
    );
    let aggregate = Aggregate {
        schema: aggregate::SCHEMA.to_owned(),
        split,
        tasks: output.set.tasks,
        repeats,
        limit,
        arms: arms(&output.arms, &names)?,
        pairs: pairs(&output.pairs, &names, output.set.tasks)?,
    };
    aggregate.validate()?;
    match (split, &output.instances) {
        (Split::Holdout, Some(_)) => {
            bail!("a holdout aggregate holds per-instance verdicts; refused and not kept")
        }
        (Split::Dev, Some(instances)) => check_instances(&aggregate, instances, &names)?,
        (Split::Dev, None) => bail!("the runner's dev aggregate has no per-instance verdicts"),
        (Split::Holdout, None) => {}
    }
    Ok(aggregate)
}

fn check_set(set: &Set, split: Split) -> anyhow::Result<()> {
    ensure!(
        set.name == manifest::SET_NAME,
        "the runner ran another set than {}",
        manifest::SET_NAME
    );
    ensure!(
        set.split == split,
        "the runner ran the {} split, not {split}",
        set.split
    );
    ensure!(
        set.sha256.len() == 64 && set.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "the runner's aggregate does not pin its set by SHA-256"
    );
    Ok(())
}

/// Exactly the requested arms, each agreeing with its own model and harness.
fn arms(
    wire: &BTreeMap<String, Arm>,
    names: &BTreeMap<String, ArmKey>,
) -> anyhow::Result<BTreeMap<ArmKey, ArmTotals>> {
    ensure!(
        wire.keys().eq(names.keys()),
        "the runner's arms are not the {} requested",
        names.len()
    );
    let mut arms = BTreeMap::new();
    for (name, arm) in wire {
        let key = names[name];
        ensure!(
            format!("{}/{}", arm.model, arm.harness) == *name,
            "{key}: the runner's arm disagrees with its own name"
        );
        ensure!(
            u64::from(arm.n) + u64::from(arm.infra) == u64::from(arm.trials),
            "{key}: graded and infrastructure trials do not add up"
        );
        let totals = ArmTotals {
            n: arm.n,
            passed: arm.passed,
            infra: arm.infra,
            cost_usd: arm.cost_usd_total,
            wall_s: arm.wall_seconds_total,
        };
        arms.insert(key, totals);
    }
    assert_eq!(arms.len(), names.len(), "one total per requested arm");
    Ok(arms)
}

fn pairs(
    wire: &[Pair],
    names: &BTreeMap<String, ArmKey>,
    tasks: u32,
) -> anyhow::Result<Vec<PairCounts>> {
    let key = |name: &str| {
        names
            .get(name)
            .copied()
            .context("a runner pair names an arm outside the request")
    };
    let mut pairs = Vec::with_capacity(wire.len());
    for pair in wire {
        let (a, b) = (key(&pair.baseline)?, key(&pair.candidate)?);
        ensure!(
            pair.model == a.model.name(),
            "{a} vs {b}: the runner's pair names another model"
        );
        ensure!(
            pair.tasks <= tasks,
            "{a} vs {b}: more paired tasks than the set has"
        );
        pairs.push(PairCounts {
            a,
            b,
            n: pair.pairs,
            a_only: pair.b,
            b_only: pair.c,
        });
    }
    Ok(pairs)
}

/// Each arm's verdicts by (task, repeat) cell.
type Cells<'a> = BTreeMap<ArmKey, BTreeMap<(&'a str, u32), bool>>;

/// Dev only: the verdicts must add up to every arm's totals and every pair.
fn check_instances(
    aggregate: &Aggregate,
    instances: &[Instance],
    names: &BTreeMap<String, ArmKey>,
) -> anyhow::Result<()> {
    let by_arm = cells(instances, names, aggregate.repeats)?;
    let none = BTreeMap::new();
    for (key, totals) in &aggregate.arms {
        let cells = by_arm.get(key).unwrap_or(&none);
        let passed = cells.values().filter(|&&passed| passed).count();
        ensure!(
            (cells.len(), passed) == (totals.n as usize, totals.passed as usize),
            "{key}: the verdicts do not add up to n = {} and passed = {}",
            totals.n,
            totals.passed
        );
    }
    let tasks: BTreeSet<&str> = by_arm
        .values()
        .flat_map(|c| c.keys().map(|k| k.0))
        .collect();
    ensure!(
        tasks.len() <= aggregate.tasks as usize,
        "the verdicts name more tasks than the set has"
    );
    for pair in &aggregate.pairs {
        let a = by_arm.get(&pair.a).unwrap_or(&none);
        let b = by_arm.get(&pair.b).unwrap_or(&none);
        let both: Vec<(bool, bool)> = a
            .iter()
            .filter_map(|(cell, &x)| b.get(cell).map(|&y| (x, y)))
            .collect();
        let a_only = both.iter().filter(|&&(x, y)| x && !y).count();
        let b_only = both.iter().filter(|&&(x, y)| !x && y).count();
        ensure!(
            (both.len(), a_only, b_only)
                == (pair.n as usize, pair.a_only as usize, pair.b_only as usize),
            "{} vs {}: the pair counts disagree with the verdicts",
            pair.a,
            pair.b
        );
    }
    Ok(())
}

/// Each dev verdict in its cell, refusing repeats outside the run and cells graded twice.
fn cells<'a>(
    instances: &'a [Instance],
    names: &BTreeMap<String, ArmKey>,
    repeats: u32,
) -> anyhow::Result<Cells<'a>> {
    let mut by_arm = Cells::new();
    for verdict in instances {
        let key = *names
            .get(&verdict.arm)
            .context("a dev verdict names an arm outside the request")?;
        ensure!(
            verdict.repeat < repeats,
            "{key}: a verdict of repeat {} in a run of {}",
            verdict.repeat,
            repeats
        );
        let cell = (verdict.task.as_str(), verdict.repeat);
        let previous = by_arm.entry(key).or_default().insert(cell, verdict.passed);
        ensure!(
            previous.is_none(),
            "{key}: {} repeat {} is graded twice",
            verdict.task,
            verdict.repeat
        );
    }
    Ok(by_arm)
}

#[cfg(test)]
mod tests;
