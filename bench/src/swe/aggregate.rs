//! archon-bench's record of one SWE run: per-arm totals and paired
//! discordant counts, validated, with no instance id or verdict in it.
//!
//! `swe run` builds it from the runner's own output (`runner::output`) and
//! saves it; `swe gate` and `--estimate-from` read it back. Parsing is an
//! allowlist (`deny_unknown_fields`), and a holdout file's errors never quote
//! its content, so nothing sealed is echoed or kept.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, anyhow, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::arms::{ArmKey, Harness, Model};
use super::runner::MAX_REPEATS;
use super::stats;

/// The schema of a saved aggregate.
pub const SCHEMA: &str = "archon-bench-swe-aggregate/1";

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
/// failures, which are in no other number. Cost or wall time is `None` when
/// the runner could not price or time every graded trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmTotals {
    pub n: u32,
    pub passed: u32,
    pub infra: u32,
    pub cost_usd: Option<f64>,
    pub wall_s: Option<f64>,
}

/// Trial pairs both arms graded (matched on task and repeat), and those only
/// one of them solved. `a` is the runner's baseline, `b` its candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairCounts {
    pub a: ArmKey,
    pub b: ArmKey,
    pub n: u32,
    pub a_only: u32,
    pub b_only: u32,
}

/// A validated aggregate. It holds no instance id, so saving it keeps
/// nothing of the sealed set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregate {
    pub schema: String,
    pub split: Split,
    /// Tasks the runner counted, and repeats of each: a complete pair grades
    /// `tasks * repeats` trial pairs.
    pub tasks: u32,
    pub repeats: u32,
    /// Dev only: a `--limit` smoke ran the split's first `limit` tasks, so
    /// the gate never counts it as the whole split.
    pub limit: Option<u32>,
    pub arms: BTreeMap<ArmKey, ArmTotals>,
    pub pairs: Vec<PairCounts>,
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

/// Parse JSON of `split`; a holdout error names only its class and position.
pub(crate) fn parse_json<T: DeserializeOwned>(
    text: &str,
    split: Split,
    schema: &str,
) -> anyhow::Result<T> {
    serde_json::from_str(text).map_err(|e| match split {
        Split::Dev => anyhow!("parsing a dev {schema} file: {e}"),
        Split::Holdout => anyhow!(
            "a holdout file is not the {schema} schema ({:?} error at line {}, column {}); \
             its content is withheld",
            e.classify(),
            e.line(),
            e.column()
        ),
    })
}

impl Aggregate {
    /// Parse and check an aggregate `swe run` saved for `split`.
    pub fn parse_saved(text: &str, split: Split) -> anyhow::Result<Self> {
        let aggregate: Aggregate = parse_json(text, split, SCHEMA)?;
        ensure!(
            aggregate.schema == SCHEMA,
            "a saved aggregate's schema is not {SCHEMA}"
        );
        ensure!(
            aggregate.split == split,
            "expected a {split} aggregate, got {}",
            aggregate.split
        );
        aggregate.validate()?;
        Ok(aggregate)
    }

    /// The invariants every aggregate holds, whichever file it came from.
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (1..=Model::ALL.len() * Harness::ALL.len()).contains(&self.arms.len()),
            "an aggregate has 1 to 6 arms, not {}",
            self.arms.len()
        );
        ensure!(
            (1..=stats::MAX_PAIRS).contains(&self.tasks),
            "an aggregate counts 1 to {} tasks, not {}",
            stats::MAX_PAIRS,
            self.tasks
        );
        ensure!(
            (1..=MAX_REPEATS).contains(&self.repeats),
            "an aggregate has 1 to {MAX_REPEATS} repeats, not {}",
            self.repeats
        );
        ensure!(
            self.limit.is_none() || self.split == Split::Dev,
            "only a dev aggregate is limited: the holdout runs whole"
        );
        ensure!(
            self.limit.is_none_or(|limit| self.tasks <= limit),
            "an aggregate limited to {:?} tasks counts {}",
            self.limit,
            self.tasks
        );
        let cells = u64::from(self.tasks) * u64::from(self.repeats);
        for (key, totals) in &self.arms {
            ensure!(
                totals.passed <= totals.n,
                "{key}: more passes than graded trials"
            );
            ensure!(
                u64::from(totals.n) + u64::from(totals.infra) <= cells,
                "{key}: more trials than tasks times repeats"
            );
            for (what, value) in [("cost_usd", totals.cost_usd), ("wall_s", totals.wall_s)] {
                ensure!(
                    value.is_none_or(|v| v.is_finite() && v >= 0.0),
                    "{key}: {what} is not a nonnegative number"
                );
            }
        }
        self.validate_pairs()
    }

    fn validate_pairs(&self) -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        for pair in &self.pairs {
            let (a, b) = (pair.a, pair.b);
            ensure!(a != b, "{a}: an arm is paired with itself");
            ensure!(a.model == b.model, "{a} vs {b}: a pair crosses models");
            let fewest = self.totals(a)?.n.min(self.totals(b)?.n);
            let discordant = u64::from(pair.a_only) + u64::from(pair.b_only);
            ensure!(
                discordant <= u64::from(pair.n),
                "{a} vs {b}: more discordant than paired"
            );
            ensure!(
                discordant <= u64::from(stats::MAX_PAIRS),
                "{a} vs {b}: more discordant pairs than the gate's exact test takes"
            );
            ensure!(
                pair.n <= fewest,
                "{a} vs {b}: more pairs than graded trials"
            );
            ensure!(
                seen.insert((a.min(b), a.max(b))),
                "{a} vs {b}: paired twice"
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
