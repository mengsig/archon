//! The worker policy a delegating stage lead runs under, sent to Aether as
//! its `config.delegation_policy` so that `delegate` refuses a worker outside
//! it. Reviewers are bounded apart, by the models the review contract names.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;
use crate::model::{Effort, ModelChoice, ModelRef};
use crate::model_tier::Tier;

/// Most parallel workers a pipeline or plan may allow one lead.
pub const MAX_PARALLEL_WORKERS: u32 = 8;

/// Most worker choices one `<stage>.workers` plan key may list.
pub const MAX_WORKER_CHOICES: usize = 4;

/// The role of a child that `reviewers`, not `workers`, bounds.
pub const REVIEWER_ROLE: &str = "reviewer";

/// One allowed worker: an exact `model`, or any model of `tier`, at `effort`
/// or below. Exactly one of `model` and `tier` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    pub effort: Effort,
}

impl WorkerChoice {
    /// A plan entry is an exact model: Archon resolved its tier within the
    /// model policy, so Aether is not left to pick one outside it.
    pub fn from_plan(choice: &ModelChoice) -> Self {
        Self {
            model: Some(choice.model.clone()),
            tier: None,
            effort: choice.effort,
        }
    }

    pub fn of_tier(tier: Tier, effort: Effort) -> Self {
        Self {
            model: None,
            tier: Some(tier),
            effort,
        }
    }
}

impl fmt::Display for WorkerChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.model, self.tier) {
            (Some(model), _) => write!(f, "{model} at effort {}", self.effort),
            (None, Some(tier)) => write!(f, "the {tier} tier at effort {}", self.effort),
            (None, None) => unreachable!("a worker choice names a model or a tier"),
        }
    }
}

/// What a lead's workers may run on: Aether's delegation policy wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerPolicy {
    /// The allowed workers; a child must match one, at its effort or below.
    pub workers: Vec<WorkerChoice>,
    /// The tier a worker takes when the lead names no model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<Tier>,
    /// No worker above this effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_effort: Option<Effort>,
    /// At most this many workers run at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
    /// What a child with role `reviewer` must match instead of `workers`: the
    /// reviewer models the contract names, each up to its effort. Empty: a
    /// reviewer is bounded as any worker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<WorkerChoice>,
}

impl WorkerPolicy {
    /// A policy of `workers`, capped at their highest effort.
    pub fn new(
        workers: Vec<WorkerChoice>,
        default_tier: Option<Tier>,
        max_parallel: Option<u32>,
    ) -> Result<Self, ArchonError> {
        if workers.is_empty() {
            return Err(ArchonError::Invalid(
                "a worker policy allows at least one worker".into(),
            ));
        }
        if let Some(bad) = workers
            .iter()
            .find(|w| w.model.is_some() == w.tier.is_some())
        {
            return Err(ArchonError::Invalid(format!(
                "a worker choice names a model or a tier, not both or neither: {bad:?}"
            )));
        }
        if max_parallel.is_some_and(|n| !(1..=MAX_PARALLEL_WORKERS).contains(&n)) {
            return Err(ArchonError::Invalid(format!(
                "workers max_parallel must be 1 to {MAX_PARALLEL_WORKERS}, not {max_parallel:?}"
            )));
        }
        let max_effort = workers.iter().map(|w| w.effort).max();
        Ok(Self {
            workers,
            default_tier,
            max_effort,
            max_parallel,
            reviewers: Vec::new(),
        })
    }

    /// This policy with reviewers bounded to `models`, each at `effort` or below.
    pub fn with_reviewers(mut self, models: Vec<ModelRef>, effort: Effort) -> Self {
        assert!(self.reviewers.is_empty(), "reviewers are set once");
        self.reviewers = models
            .into_iter()
            .map(|model| WorkerChoice {
                model: Some(model),
                tier: None,
                effort,
            })
            .collect();
        self
    }

    /// The policy in a sentence, for the lead's contract.
    pub fn describe(&self) -> String {
        assert!(!self.workers.is_empty(), "validated in new");
        let listed: Vec<String> = self.workers.iter().map(ToString::to_string).collect();
        let mut text = format!("each worker on {}", listed.join(" or "));
        if let Some(n) = self.max_parallel {
            text.push_str(&format!(", at most {n} at once"));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(effort: Effort) -> ModelChoice {
        ModelChoice::pinned("codex/terra".parse().unwrap(), effort)
    }

    #[test]
    fn a_policy_caps_effort_at_its_highest_worker() {
        let workers = vec![
            WorkerChoice::from_plan(&pinned(Effort::Low)),
            WorkerChoice::of_tier(Tier::Balanced, Effort::Medium),
        ];
        let policy = WorkerPolicy::new(workers, Some(Tier::Balanced), Some(2)).unwrap();
        assert_eq!(policy.max_effort, Some(Effort::Medium));
        assert!(
            policy.reviewers.is_empty(),
            "bounded as workers until named"
        );
        assert_eq!(
            policy.describe(),
            "each worker on codex/terra at effort low or the balanced tier at effort medium, \
             at most 2 at once"
        );
    }

    #[test]
    fn a_policy_serializes_as_aether_reads_it() {
        let workers = vec![
            WorkerChoice::from_plan(&pinned(Effort::Medium)),
            WorkerChoice::of_tier(Tier::Fast, Effort::Low),
        ];
        let reviewer: ModelRef = "claude-code/sonnet".parse().unwrap();
        let policy = WorkerPolicy::new(workers, Some(Tier::Fast), None)
            .unwrap()
            .with_reviewers(vec![reviewer], Effort::XHigh);
        let wire = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "workers": [
                    {"model": {"provider": "codex", "model": "terra"}, "effort": "medium"},
                    {"tier": "fast", "effort": "low"}
                ],
                "default_tier": "fast",
                "max_effort": "medium",
                "reviewers": [
                    {"model": {"provider": "claude-code", "model": "sonnet"}, "effort": "x_high"}
                ]
            })
        );
        let back: WorkerPolicy = serde_json::from_value(wire).unwrap();
        assert_eq!(back, policy);
    }

    #[test]
    fn a_policy_refuses_no_workers_an_ambiguous_choice_and_a_parallel_cap_out_of_range() {
        assert!(WorkerPolicy::new(Vec::new(), None, None).is_err());
        let both = WorkerChoice {
            model: Some("codex/terra".parse().unwrap()),
            tier: Some(Tier::Fast),
            effort: Effort::Low,
        };
        assert!(WorkerPolicy::new(vec![both], None, None).is_err());
        let fast = || vec![WorkerChoice::of_tier(Tier::Fast, Effort::Low)];
        assert!(WorkerPolicy::new(fast(), None, Some(0)).is_err());
        assert!(WorkerPolicy::new(fast(), None, Some(MAX_PARALLEL_WORKERS + 1)).is_err());
        assert!(WorkerPolicy::new(fast(), None, Some(MAX_PARALLEL_WORKERS)).is_ok());
    }
}
