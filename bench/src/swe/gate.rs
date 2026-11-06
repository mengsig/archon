//! The release gate on the SWE family. Per model and split, the candidate
//! (`new`) must be no worse than native and no worse than the best recent
//! release (`old`): an exact one-sided paired test at `ALPHA`. Any failing
//! or missing comparison fails the release.

use serde::Serialize;

use super::aggregate::{Aggregate, Split};
use super::arms::{ArmKey, Harness, Model};
use super::stats;

/// The one-sided significance level of every comparison, fixed before any run.
pub const ALPHA: f64 = 0.05;

/// The splits a release gate needs.
pub const SPLITS: [Split; 2] = [Split::Dev, Split::Holdout];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reference {
    Native,
    BestRelease,
}

impl Reference {
    pub fn harness(self) -> Harness {
        match self {
            Reference::Native => Harness::Native,
            Reference::BestRelease => Harness::Old,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Not significantly worse.
    Pass,
    /// Significantly worse at `ALPHA`.
    Worse,
    /// No aggregate for the split, no such pair, or a trial lost to an
    /// infrastructure failure: an incomplete comparison cannot pass.
    Incomplete,
}

/// The paired counts of one comparison, oriented from the candidate's side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Paired {
    pub n: u32,
    pub new_only: u32,
    pub reference_only: u32,
    /// P(the reference wins at least this many discordant pairs | no difference).
    pub p_worse: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub model: Model,
    pub split: Split,
    pub reference: Reference,
    pub paired: Option<Paired>,
    pub verdict: Verdict,
    /// Why a comparison is incomplete; empty otherwise.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateReport {
    pub alpha: f64,
    pub comparisons: Vec<Comparison>,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        !self.comparisons.is_empty() && self.comparisons.iter().all(|c| c.verdict == Verdict::Pass)
    }

    pub fn of(&self, reference: Reference) -> impl Iterator<Item = &Comparison> {
        self.comparisons
            .iter()
            .filter(move |c| c.reference == reference)
    }
}

/// Every comparison for `models` over both splits, from whichever aggregates exist.
pub fn evaluate(models: &[Model], aggregates: &[Aggregate]) -> GateReport {
    assert!(!models.is_empty(), "a gate covers at least one model");
    assert!(
        aggregates
            .iter()
            .map(|a| a.split)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == aggregates.len(),
        "at most one aggregate per split"
    );
    let mut comparisons = Vec::new();
    for reference in [Reference::Native, Reference::BestRelease] {
        for &model in models {
            for split in SPLITS {
                let aggregate = aggregates.iter().find(|a| a.split == split);
                comparisons.push(compare(model, split, reference, aggregate));
            }
        }
    }
    GateReport {
        alpha: ALPHA,
        comparisons,
    }
}

fn compare(
    model: Model,
    split: Split,
    reference: Reference,
    aggregate: Option<&Aggregate>,
) -> Comparison {
    let new = ArmKey {
        model,
        harness: Harness::New,
    };
    let other = ArmKey {
        model,
        harness: reference.harness(),
    };
    let (paired, verdict, reason) = match aggregate {
        None => (None, Verdict::Incomplete, format!("no {split} aggregate")),
        Some(aggregate) => judge(aggregate, new, other),
    };
    Comparison {
        model,
        split,
        reference,
        paired,
        verdict,
        reason,
    }
}

/// The paired test of `new` against `other`, and why it is incomplete if it is.
fn judge(aggregate: &Aggregate, new: ArmKey, other: ArmKey) -> (Option<Paired>, Verdict, String) {
    assert_eq!(new.model, other.model, "a comparison stays on one model");
    let Some((n, new_only, reference_only)) = aggregate.paired(new, other) else {
        return (
            None,
            Verdict::Incomplete,
            format!("no {new} vs {other} pair"),
        );
    };
    let p_worse = stats::sign_test_upper_tail(reference_only, new_only + reference_only);
    let paired = Some(Paired {
        n,
        new_only,
        reference_only,
        p_worse,
    });
    let infra: u32 = [new, other]
        .iter()
        .filter_map(|key| aggregate.arms.get(key))
        .map(|totals| totals.infra)
        .sum();
    if infra > 0 {
        let reason = format!("{infra} infrastructure failures; rerun them");
        return (paired, Verdict::Incomplete, reason);
    }
    if n == 0 {
        return (
            paired,
            Verdict::Incomplete,
            "no instance graded on both arms".into(),
        );
    }
    let verdict = if p_worse < ALPHA {
        Verdict::Worse
    } else {
        Verdict::Pass
    };
    (paired, verdict, String::new())
}

/// The noise bound stated before a run: for each count of discordant pairs,
/// how many of them the reference must win before the gate fails.
pub fn noise_bound(instances: u32) -> Vec<(u32, Option<u32>)> {
    assert!(instances > 0, "a noise bound needs instances");
    let bound: Vec<(u32, Option<u32>)> = (1..=instances)
        .map(|d| (d, stats::critical_losses(d, ALPHA)))
        .collect();
    assert!(
        bound
            .windows(2)
            .all(|w| w[0].1.unwrap_or(0) <= w[1].1.unwrap_or(u32::MAX)),
        "the critical count never falls as discordant pairs grow"
    );
    bound
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn aggregate(split: &str, pairs: serde_json::Value, infra: u32) -> Aggregate {
        let arm = json!({"n": 25, "passed": 10, "infra": infra, "cost_usd": 3.0, "wall_s": 9000.0});
        let value = json!({
            "schema": crate::swe::aggregate::SCHEMA, "split": split,
            "arms": {"haiku/old": arm, "haiku/new": arm, "haiku/native": arm},
            "pairs": pairs,
        });
        let split = if split == "dev" {
            Split::Dev
        } else {
            Split::Holdout
        };
        Aggregate::parse(&value.to_string(), split, None).unwrap()
    }

    fn pairs(native_wins: u32, old_wins: u32) -> serde_json::Value {
        json!([
            {"a": "haiku/native", "b": "haiku/new", "n": 25, "a_only": native_wins, "b_only": 1},
            {"a": "haiku/new", "b": "haiku/old", "n": 25, "a_only": 1, "b_only": old_wins},
        ])
    }

    #[test]
    fn a_release_passes_only_when_every_split_and_reference_is_not_worse() {
        let report = evaluate(
            &[Model::Haiku],
            &[
                aggregate("dev", pairs(2, 2), 0),
                aggregate("holdout", pairs(1, 3), 0),
            ],
        );
        assert_eq!(report.comparisons.len(), 4);
        assert!(report.passed(), "{report:#?}");
        let native = report.of(Reference::Native).next().unwrap().paired.unwrap();
        assert_eq!(
            (native.new_only, native.reference_only),
            (1, 2),
            "oriented from new"
        );
    }

    #[test]
    fn losing_significantly_to_native_or_the_best_release_fails() {
        let dev = aggregate("dev", pairs(2, 2), 0);
        let report = evaluate(
            &[Model::Haiku],
            &[dev.clone(), aggregate("holdout", pairs(8, 0), 0)],
        );
        assert!(!report.passed());
        let worse: Vec<_> = report
            .comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Worse)
            .collect();
        assert_eq!(worse.len(), 1);
        assert_eq!(
            (worse[0].reference, worse[0].split),
            (Reference::Native, Split::Holdout)
        );
        let report = evaluate(
            &[Model::Haiku],
            &[dev, aggregate("holdout", pairs(0, 8), 0)],
        );
        let worse: Vec<_> = report
            .comparisons
            .iter()
            .filter(|c| c.verdict == Verdict::Worse)
            .collect();
        assert_eq!(worse[0].reference, Reference::BestRelease);
    }

    #[test]
    fn a_missing_split_pair_or_infra_trial_makes_the_gate_incomplete() {
        let report = evaluate(&[Model::Haiku], &[aggregate("dev", pairs(0, 0), 0)]);
        assert!(!report.passed());
        assert!(
            report
                .comparisons
                .iter()
                .any(|c| c.reason == "no holdout aggregate")
        );
        let report = evaluate(
            &[Model::Haiku, Model::Luna],
            &[aggregate("dev", pairs(0, 0), 0)],
        );
        assert!(
            report
                .comparisons
                .iter()
                .any(|c| c.reason == "no luna/new vs luna/native pair")
        );
        let report = evaluate(
            &[Model::Haiku],
            &[
                aggregate("dev", pairs(0, 0), 1),
                aggregate("holdout", pairs(0, 0), 0),
            ],
        );
        let dev: Vec<_> = report
            .comparisons
            .iter()
            .filter(|c| c.split == Split::Dev)
            .collect();
        assert!(
            dev.iter()
                .all(|c| c.verdict == Verdict::Incomplete && c.paired.is_some())
        );
    }

    #[test]
    fn the_noise_bound_lists_the_first_failing_count_per_discordant_total() {
        let bound = noise_bound(25);
        assert_eq!(bound.len(), 25);
        assert_eq!(bound[3], (4, None));
        assert_eq!(bound[4], (5, Some(5)));
        assert_eq!(bound[7], (8, Some(7)));
    }

    #[test]
    fn the_noise_bound_table_in_the_docs_holds() {
        let bound = noise_bound(25);
        let documented = [
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 7),
            (9, 8),
            (10, 9),
            (12, 10),
            (15, 12),
            (20, 15),
            (25, 18),
        ];
        for (discordant, losses) in documented {
            assert_eq!(bound[discordant as usize - 1], (discordant, Some(losses)));
        }
    }
}
