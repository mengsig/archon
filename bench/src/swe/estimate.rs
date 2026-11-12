//! The cost, time and disk estimate printed before every SWE run.
//!
//! Without measurements it uses stated priors per arm. Given an earlier
//! aggregate of the same split, it uses that run's mean cost and wall time
//! per trial instead.

use std::fmt::Write as _;

use super::aggregate::Aggregate;
use super::arms::{ArmKey, Harness, Model};
use super::runner::{MAX_JOBS, MAX_REPEATS};
use super::stats;

/// Grading one trial in its instance container.
const GRADE_MINUTES: f64 = 3.0;
/// The runner's cap on its labelled image cache (archon-bench-swe).
pub const IMAGE_CACHE_GB: f64 = 30.0;
/// One trial's workspace: a checked-out repository and its build products.
const WORKSPACE_GB: f64 = 1.5;

/// Mean minutes and dollars of one trial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerTrial {
    pub minutes: f64,
    pub usd: f64,
}

/// Priors for an SWE-bench Verified instance of 15 min to 4 h human time.
/// Archon runs a pipeline over Aether; Luna's dollars are API-equivalent.
fn prior(key: ArmKey) -> PerTrial {
    match (key.model, key.harness) {
        (Model::Haiku, Harness::Old | Harness::New) => PerTrial {
            minutes: 20.0,
            usd: 0.80,
        },
        (Model::Haiku, Harness::Native) => PerTrial {
            minutes: 12.0,
            usd: 0.40,
        },
        (Model::Luna, Harness::Old | Harness::New) => PerTrial {
            minutes: 25.0,
            usd: 0.30,
        },
        (Model::Luna, Harness::Native) => PerTrial {
            minutes: 15.0,
            usd: 0.15,
        },
    }
}

/// The arm's mean per graded trial, when the earlier run timed and priced every one.
fn measured(key: ArmKey, earlier: &Aggregate) -> Option<PerTrial> {
    let totals = earlier.arms.get(&key).filter(|t| t.n > 0)?;
    let n = f64::from(totals.n);
    Some(PerTrial {
        minutes: totals.wall_s? / n / 60.0,
        usd: totals.cost_usd? / n,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub trials: u32,
    pub hours: f64,
    pub usd: f64,
    pub disk_gb: f64,
    /// Whether the figures come from an earlier run rather than the priors.
    pub measured: bool,
}

pub fn estimate(
    arms: &[ArmKey],
    instances: u32,
    repeats: u32,
    jobs: u32,
    earlier: Option<&Aggregate>,
) -> Estimate {
    assert!(
        (1..=Model::ALL.len() * Harness::ALL.len()).contains(&arms.len())
            && (1..=stats::MAX_PAIRS).contains(&instances),
        "an estimate covers some arms and validated instances"
    );
    assert!(
        (1..=MAX_REPEATS).contains(&repeats) && (1..=MAX_JOBS).contains(&jobs),
        "an estimate needs validated repeats and jobs"
    );
    let per_arm: Vec<PerTrial> = arms
        .iter()
        .map(|&key| {
            earlier
                .and_then(|e| measured(key, e))
                .unwrap_or_else(|| prior(key))
        })
        .collect();
    let measured = earlier.is_some_and(|e| arms.iter().all(|&key| measured(key, e).is_some()));
    let runs = f64::from(instances * repeats);
    let minutes: f64 = per_arm
        .iter()
        .map(|t| (t.minutes + GRADE_MINUTES) * runs)
        .sum();
    let usd: f64 = per_arm.iter().map(|t| t.usd * runs).sum();
    let estimate = Estimate {
        trials: arms.len() as u32 * instances * repeats,
        hours: minutes / 60.0 / f64::from(jobs),
        usd,
        disk_gb: IMAGE_CACHE_GB + WORKSPACE_GB * f64::from(jobs),
        measured,
    };
    assert!(
        estimate.hours > 0.0 && estimate.usd >= 0.0,
        "an estimate is positive"
    );
    estimate
}

pub fn render(estimate: &Estimate, jobs: u32) -> String {
    let source = if estimate.measured {
        "measured means of an earlier run"
    } else {
        "priors"
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "estimate ({source}): {} trials, {:.1} h at {jobs} at once, ${:.2}, \
         {:.1} GB disk (image cache cap {IMAGE_CACHE_GB:.0} GB + workspaces)",
        estimate.trials, estimate.hours, estimate.usd, estimate.disk_gb
    );
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::swe::aggregate::{SCHEMA, Split};

    fn keys(text: &[&str]) -> Vec<ArmKey> {
        text.iter().map(|t| t.parse().unwrap()).collect()
    }

    #[test]
    fn priors_scale_with_instances_repeats_and_jobs() {
        let arms = keys(&["haiku/new", "haiku/native"]);
        let one = estimate(&arms, 25, 1, 1, None);
        assert_eq!(one.trials, 50);
        assert!((one.usd - 25.0 * 1.2).abs() < 1e-9);
        assert!((one.hours - 25.0 * (23.0 + 15.0) / 60.0).abs() < 1e-9);
        let three = estimate(&arms, 25, 1, 3, None);
        assert!((three.hours * 3.0 - one.hours).abs() < 1e-9);
        assert!(!one.measured && three.disk_gb > one.disk_gb);
        assert!(render(&one, 1).starts_with("estimate (priors): 50 trials"));
    }

    #[test]
    fn an_earlier_run_replaces_the_priors_only_when_it_covers_every_arm() {
        let arm = json!({"n": 10, "passed": 5, "infra": 0, "cost_usd": 5.0, "wall_s": 6000.0});
        let mut value = json!({"schema": SCHEMA, "split": "dev", "tasks": 10, "repeats": 1, "limit": null,
                               "arms": {"haiku/new": arm}, "pairs": []});
        let earlier = Aggregate::parse_saved(&value.to_string(), Split::Dev).unwrap();
        let covered = estimate(&keys(&["haiku/new"]), 10, 1, 1, Some(&earlier));
        assert!(covered.measured);
        assert!((covered.usd - 5.0).abs() < 1e-9);
        assert!((covered.hours - 10.0 * 13.0 / 60.0).abs() < 1e-9);
        let partial = estimate(&keys(&["haiku/new", "luna/new"]), 10, 1, 1, Some(&earlier));
        assert!(!partial.measured);
        value["arms"]["haiku/new"]["cost_usd"] = serde_json::Value::Null;
        let unpriced = Aggregate::parse_saved(&value.to_string(), Split::Dev).unwrap();
        let fallback = estimate(&keys(&["haiku/new"]), 10, 1, 1, Some(&unpriced));
        assert!(!fallback.measured, "an unpriced arm keeps its prior");
    }
}
