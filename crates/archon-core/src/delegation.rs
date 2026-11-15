//! The worker policy a delegating stage lead runs under, sent to Aether as
//! its `config.delegation_policy` so that `delegate` refuses a worker outside
//! it. Reviewers are exempt from `workers`, and bound instead to the models
//! the review contract names (`exempt_models`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;
use crate::model::{Effort, ModelChoice, ModelRef};
use crate::model_tier::{ModelCatalog, Tier};
use crate::policy::ModelPolicy;

/// Most parallel workers a pipeline or plan may allow one lead.
pub const MAX_PARALLEL_WORKERS: u32 = 8;

/// Most worker choices one `<stage>.workers` plan key may list.
pub const MAX_WORKER_CHOICES: usize = 4;

/// The role of a child that `exempt_models`, not `workers`, bounds.
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
    /// Child roles `workers`, `max_effort` and `max_parallel` do not bound:
    /// `reviewer` when the lead owes reviewers. Empty: every child is a worker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_roles: Vec<String>,
    /// The models an exempt child may run on whatever its tier: the reviewer
    /// models the contract names. Aether holds any other to the lead's tier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_models: Vec<ModelRef>,
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
            exempt_roles: Vec::new(),
            exempt_models: Vec::new(),
        })
    }

    /// This policy with reviewers exempt from `workers` and bound to `models`;
    /// the review contract and Archon's watchdog bound their effort.
    pub fn with_reviewers(mut self, models: Vec<ModelRef>) -> Self {
        assert!(self.exempt_roles.is_empty(), "reviewers are set once");
        assert!(
            models.iter().all(|m| !m.provider.is_empty()),
            "reviewer models are provider-qualified"
        );
        self.exempt_roles = vec![REVIEWER_ROLE.to_owned()];
        self.exempt_models = models;
        self
    }

    /// This policy narrowed to the model policy as it stands at a spawn:
    /// efforts capped, denied models dropped, and under an allow or deny
    /// list each tier resolved to the permitted catalog model Aether would
    /// otherwise not know to pick. With none left, the cheapest permitted
    /// catalog model; `None` when the policy permits no catalog model.
    pub fn within(&self, policy: &ModelPolicy, catalog: &ModelCatalog) -> Option<Self> {
        assert!(!self.workers.is_empty(), "validated in new");
        let cap = |effort: Effort| policy.max_effort.map_or(effort, |c| effort.min(c));
        let mut workers = self.permitted_workers(policy, catalog, cap);
        if workers.is_empty() {
            let lowest = self.workers.iter().map(|w| w.effort).min();
            let effort = cap(lowest.expect("validated in new"));
            let cheapest = catalog
                .models()
                .iter()
                .filter(|m| m.tier.is_some() && policy.permits_model(&m.model))
                .min_by_key(|m| m.tier)?;
            let pinned = ModelChoice::pinned(cheapest.model.clone(), effort);
            workers.push(WorkerChoice::from_plan(&pinned));
        }
        let mut narrowed = Self::new(workers, self.default_tier, self.max_parallel).ok()?;
        narrowed.exempt_roles.clone_from(&self.exempt_roles);
        narrowed.exempt_models.clone_from(&self.exempt_models);
        assert!(
            narrowed.max_effort <= self.max_effort,
            "narrowing never raises effort"
        );
        Some(narrowed)
    }

    fn permitted_workers(
        &self,
        policy: &ModelPolicy,
        catalog: &ModelCatalog,
        cap: impl Fn(Effort) -> Effort,
    ) -> Vec<WorkerChoice> {
        let restricted = policy.allow.is_some() || !policy.deny.is_empty();
        let mut workers: Vec<WorkerChoice> = Vec::new();
        for worker in &self.workers {
            let narrowed = match (&worker.model, worker.tier) {
                (Some(model), _) if !policy.permits_model(model) => continue,
                (None, Some(tier)) if restricted && !catalog.is_empty() => {
                    let Ok((model, _)) = catalog.resolve(tier, None, None, policy) else {
                        continue;
                    };
                    WorkerChoice::from_plan(&ModelChoice::pinned(model, cap(worker.effort)))
                }
                _ => WorkerChoice {
                    effort: cap(worker.effort),
                    ..worker.clone()
                },
            };
            if !workers.contains(&narrowed) {
                workers.push(narrowed);
            }
        }
        debug_assert!(
            workers
                .iter()
                .all(|w| w.model.as_ref().is_none_or(|m| policy.permits_model(m)))
        );
        workers
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
            policy.exempt_roles.is_empty() && policy.exempt_models.is_empty(),
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
            WorkerChoice::from_plan(&pinned(Effort::XHigh)),
            WorkerChoice::of_tier(Tier::Fast, Effort::Low),
        ];
        let reviewer: ModelRef = "claude-code/sonnet".parse().unwrap();
        let policy = WorkerPolicy::new(workers, Some(Tier::Fast), Some(3))
            .unwrap()
            .with_reviewers(vec![reviewer]);
        let wire = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "workers": [
                    {"model": {"provider": "codex", "model": "terra"}, "effort": "x_high"},
                    {"tier": "fast", "effort": "low"}
                ],
                "default_tier": "fast",
                "max_effort": "x_high",
                "max_parallel": 3,
                "exempt_roles": ["reviewer"],
                "exempt_models": [{"provider": "claude-code", "model": "sonnet"}]
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

    fn catalog() -> ModelCatalog {
        use crate::model_tier::CatalogModel;
        let entry = |model: &str, tier| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(tier),
            is_default: false,
        };
        ModelCatalog::new(vec![
            entry("a/fast", Tier::Fast),
            entry("a/balanced", Tier::Balanced),
            entry("b/strong", Tier::Strong),
        ])
    }

    fn balanced_default() -> WorkerPolicy {
        let workers = vec![WorkerChoice::of_tier(Tier::Balanced, Effort::Medium)];
        WorkerPolicy::new(workers, Some(Tier::Balanced), Some(2)).unwrap()
    }

    #[test]
    fn narrowing_caps_effort_and_keeps_a_tier_under_an_open_policy() {
        let policy = ModelPolicy::from_lists(&[], &[], Some(Effort::Low));
        let narrowed = balanced_default().within(&policy, &catalog()).unwrap();
        assert_eq!(
            narrowed.workers,
            [WorkerChoice::of_tier(Tier::Balanced, Effort::Low)]
        );
        assert_eq!(narrowed.max_effort, Some(Effort::Low));
        assert_eq!(narrowed.max_parallel, Some(2));
    }

    #[test]
    fn narrowing_resolves_a_tier_to_a_permitted_model_or_the_cheapest_one() {
        let allow = |models: &[&str]| {
            let refs: Vec<ModelRef> = models.iter().map(|m| m.parse().unwrap()).collect();
            ModelPolicy::from_lists(&refs, &[], None)
        };
        let pinned = |m: &str| {
            WorkerChoice::from_plan(&ModelChoice::pinned(m.parse().unwrap(), Effort::Medium))
        };
        let within = |p: &ModelPolicy| balanced_default().within(p, &catalog());
        let fast = within(&allow(&["a/fast", "b/strong"])).unwrap();
        assert_eq!(
            fast.workers,
            [pinned("a/fast")],
            "highest permitted at or below balanced"
        );
        let strong = within(&allow(&["b/strong"])).unwrap();
        assert_eq!(
            strong.workers,
            [pinned("b/strong")],
            "the only permitted model"
        );
        assert_eq!(
            within(&allow(&["z/none"])),
            None,
            "no permitted catalog model"
        );
        let denied = ModelPolicy::from_lists(&[], &["a/balanced".parse().unwrap()], None);
        assert_eq!(within(&denied).unwrap().workers, [pinned("a/fast")]);
    }

    #[test]
    fn narrowing_drops_a_denied_planned_model_and_keeps_reviewers() {
        let workers = vec![
            WorkerChoice::from_plan(&pinned(Effort::High)),
            WorkerChoice::of_tier(Tier::Fast, Effort::Low),
        ];
        let reviewer: ModelRef = "b/strong".parse().unwrap();
        let planned = WorkerPolicy::new(workers, None, None)
            .unwrap()
            .with_reviewers(vec![reviewer.clone()]);
        let deny = ModelPolicy::from_lists(&[], &["codex/terra".parse().unwrap()], None);
        let narrowed = planned.within(&deny, &catalog()).unwrap();
        let fast = ModelChoice::pinned("a/fast".parse().unwrap(), Effort::Low);
        assert_eq!(narrowed.workers, [WorkerChoice::from_plan(&fast)]);
        assert_eq!(narrowed.max_effort, Some(Effort::Low));
        assert_eq!(narrowed.exempt_models, [reviewer]);
    }
}
