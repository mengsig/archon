//! The one adapter to Aether's SWE runner output: its aggregate file.
//!
//! The runner writes the same schema for both splits: totals per arm and
//! paired discordant counts per arm pair. Only a dev file may add per-instance
//! verdicts. Parsing is an allowlist (`deny_unknown_fields`), and a holdout
//! file's errors never quote its content, so nothing sealed is echoed or kept.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, anyhow, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::arms::ArmKey;

/// The schema version this adapter reads and writes.
pub const SCHEMA: &str = "swe-aggregate/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    /// The committed dev manifest: per-instance results may be read.
    Dev,
    /// The sealed set: only aggregates ever leave Aether's runner.
    Holdout,
}

impl fmt::Display for Split {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Split::Dev => "dev",
            Split::Holdout => "holdout",
        })
    }
}

/// One arm's totals. `n` counts graded trials; `infra` counts harness
/// failures, which are in no other number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmTotals {
    pub n: u32,
    pub passed: u32,
    pub infra: u32,
    pub cost_usd: f64,
    pub wall_s: f64,
}

/// Instances both arms graded, and those only one of them solved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairCounts {
    pub a: ArmKey,
    pub b: ArmKey,
    pub n: u32,
    pub a_only: u32,
    pub b_only: u32,
}

/// A validated aggregate. It holds no instance ids, so saving it keeps
/// nothing of the sealed set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aggregate {
    pub schema: String,
    pub split: Split,
    pub arms: BTreeMap<ArmKey, ArmTotals>,
    pub pairs: Vec<PairCounts>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema: String,
    split: Split,
    arms: BTreeMap<ArmKey, ArmTotals>,
    pairs: Vec<PairCounts>,
    #[serde(default)]
    instances: Option<Vec<InstanceVerdict>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceVerdict {
    instance_id: String,
    arm: ArmKey,
    resolved: bool,
}

impl Serialize for ArmKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ArmKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|e: anyhow::Error| serde::de::Error::custom(e))
    }
}

impl Aggregate {
    /// Parse and check a runner aggregate for `split`. With `expected`, the
    /// file must hold exactly those arms. Holdout errors name only a position.
    pub fn parse(text: &str, split: Split, expected: Option<&[ArmKey]>) -> anyhow::Result<Self> {
        let wire: Wire = serde_json::from_str(text).map_err(|e| match split {
            Split::Dev => anyhow!("parsing a dev aggregate: {e}"),
            Split::Holdout => anyhow!(
                "a holdout aggregate is not the {SCHEMA} schema ({:?} error at line {}, column {}); \
                 its content is withheld",
                e.classify(),
                e.line(),
                e.column()
            ),
        })?;
        ensure!(
            wire.schema == SCHEMA,
            "an aggregate's schema is not {SCHEMA}"
        );
        ensure!(
            wire.split == split,
            "expected a {split} aggregate, got {}",
            wire.split
        );
        let aggregate = Aggregate {
            schema: wire.schema,
            split: wire.split,
            arms: wire.arms,
            pairs: wire.pairs,
        };
        aggregate.validate(expected)?;
        match (split, wire.instances) {
            (Split::Holdout, Some(_)) => {
                bail!("a holdout aggregate holds per-instance verdicts; refused and not kept")
            }
            (Split::Dev, Some(instances)) => aggregate.check_instances(&instances)?,
            (_, None) => {}
        }
        Ok(aggregate)
    }

    fn validate(&self, expected: Option<&[ArmKey]>) -> anyhow::Result<()> {
        ensure!(!self.arms.is_empty(), "an aggregate names no arm");
        if let Some(expected) = expected {
            let want: BTreeSet<ArmKey> = expected.iter().copied().collect();
            let got: BTreeSet<ArmKey> = self.arms.keys().copied().collect();
            ensure!(
                got == want,
                "the aggregate's arms {got:?} are not the requested {want:?}"
            );
        }
        for (key, totals) in &self.arms {
            ensure!(
                totals.passed <= totals.n,
                "{key}: more passes than graded trials"
            );
            for (what, value) in [("cost_usd", totals.cost_usd), ("wall_s", totals.wall_s)] {
                ensure!(
                    value.is_finite() && value >= 0.0,
                    "{key}: {what} is not a nonnegative number"
                );
            }
        }
        let mut seen = BTreeSet::new();
        for pair in &self.pairs {
            ensure!(pair.a != pair.b, "{}: an arm is paired with itself", pair.a);
            let (a, b) = (self.totals(pair.a)?, self.totals(pair.b)?);
            ensure!(
                pair.a_only + pair.b_only <= pair.n,
                "{} vs {}: more discordant than paired",
                pair.a,
                pair.b
            );
            ensure!(
                pair.n <= a.n.min(b.n),
                "{} vs {}: more pairs than graded trials",
                pair.a,
                pair.b
            );
            ensure!(
                seen.insert((pair.a.min(pair.b), pair.a.max(pair.b))),
                "{} vs {}: paired twice",
                pair.a,
                pair.b
            );
        }
        Ok(())
    }

    /// Dev only: the verdicts must add up to the totals and the pairs.
    fn check_instances(&self, instances: &[InstanceVerdict]) -> anyhow::Result<()> {
        let mut by_arm: BTreeMap<ArmKey, BTreeMap<&str, bool>> = BTreeMap::new();
        for verdict in instances {
            self.totals(verdict.arm)?;
            let previous = by_arm
                .entry(verdict.arm)
                .or_default()
                .insert(&verdict.instance_id, verdict.resolved);
            ensure!(
                previous.is_none(),
                "{}: instance {} is graded twice",
                verdict.arm,
                verdict.instance_id
            );
        }
        for (key, totals) in &self.arms {
            let verdicts = by_arm.get(key).cloned().unwrap_or_default();
            let passed = verdicts.values().filter(|&&resolved| resolved).count() as u32;
            ensure!(
                verdicts.len() as u32 == totals.n,
                "{key}: {} verdicts for n = {}",
                verdicts.len(),
                totals.n
            );
            ensure!(
                passed == totals.passed,
                "{key}: {passed} resolved verdicts for passed = {}",
                totals.passed
            );
        }
        let none = BTreeMap::new();
        for pair in &self.pairs {
            let a = by_arm.get(&pair.a).unwrap_or(&none);
            let b = by_arm.get(&pair.b).unwrap_or(&none);
            let both: Vec<(bool, bool)> = a
                .iter()
                .filter_map(|(id, &x)| b.get(id).map(|&y| (x, y)))
                .collect();
            let a_only = both.iter().filter(|&&(x, y)| x && !y).count() as u32;
            let b_only = both.iter().filter(|&&(x, y)| !x && y).count() as u32;
            ensure!(
                (both.len() as u32, a_only, b_only) == (pair.n, pair.a_only, pair.b_only),
                "{} vs {}: the pair counts disagree with the verdicts",
                pair.a,
                pair.b
            );
        }
        Ok(())
    }

    pub fn totals(&self, key: ArmKey) -> anyhow::Result<&ArmTotals> {
        self.arms
            .get(&key)
            .with_context(|| format!("the aggregate has no {key} arm"))
    }

    /// The pair of `x` and `y` as (paired, x only, y only), whichever way round it is stored.
    pub fn paired(&self, x: ArmKey, y: ArmKey) -> Option<(u32, u32, u32)> {
        self.pairs.iter().find_map(|pair| {
            if (pair.a, pair.b) == (x, y) {
                Some((pair.n, pair.a_only, pair.b_only))
            } else if (pair.a, pair.b) == (y, x) {
                Some((pair.n, pair.b_only, pair.a_only))
            } else {
                None
            }
        })
    }

    pub fn to_json(&self) -> String {
        let text = serde_json::to_string_pretty(self).expect("an aggregate serializes");
        assert!(
            !text.contains("instance"),
            "a saved aggregate holds no instance field"
        );
        text
    }
}

#[cfg(test)]
mod tests;
