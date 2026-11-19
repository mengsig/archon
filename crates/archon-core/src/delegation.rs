//! The worker policy a delegating stage lead runs under, sent to Aether as
//! its `config.delegation_policy` so that `delegate` refuses a worker outside
//! it. Reviewers are exempt from `workers`, and bound instead to the models
//! the review contract names (`exempt_models`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;
use crate::model::{Effort, ModelChoice, ModelRef};
use crate::model_tier::{CatalogModel, ModelCatalog, Tier};
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
    /// efforts capped, denied models dropped, and each tier kept only when
    /// the catalog holds a model of it under an open policy, else resolved
    /// to a permitted catalog model, so Aether never picks a denied one.
    /// With no worker left, it fails closed: see [`Self::fallback`].
    pub fn within(
        &self,
        policy: &ModelPolicy,
        catalog: &ModelCatalog,
        lead: Option<&ModelRef>,
    ) -> Narrowed {
        assert!(!self.workers.is_empty(), "validated in new");
        let cap = |effort: Effort| policy.max_effort.map_or(effort, |c| effort.min(c));
        let workers = self.permitted_workers(policy, catalog, cap);
        if workers.is_empty() {
            let cause = unresolved_cause(self, policy, catalog);
            return self.fallback(policy, (catalog, lead), &cause);
        }
        let mut narrowed = Self::new(workers, self.default_tier, self.max_parallel)
            .expect("narrowed workers are non-empty and keep the validated cap");
        narrowed.exempt_roles.clone_from(&self.exempt_roles);
        narrowed.exempt_models.clone_from(&self.exempt_models);
        assert!(
            narrowed.max_effort <= self.max_effort,
            "narrowing never raises effort"
        );
        Narrowed {
            policy: narrowed,
            note: None,
        }
    }

    /// The narrowest policy when no planned worker is permitted, for `cause`:
    /// one worker at a time, at the lowest effort, on the cheapest permitted
    /// catalog model other than the `lead`'s own. Aether reads an empty
    /// `workers` as its tier rule, not as none, so with no such model that
    /// rule is narrowed to the fast tier, and Archon's delegation watchdog
    /// cancels any child but an exempt one: the lead works alone.
    pub fn fallback(
        &self,
        policy: &ModelPolicy,
        (catalog, lead): (&ModelCatalog, Option<&ModelRef>),
        cause: &str,
    ) -> Narrowed {
        assert!(!cause.is_empty(), "a fallback says why");
        let serves = |m: &CatalogModel| {
            m.tier.is_some() && policy.permits_model(&m.model) && Some(&m.model) != lead
        };
        let cheapest = catalog
            .models()
            .iter()
            .filter(|m| !catalog.is_retired(&m.model) && serves(m))
            .filter(|m| !catalog.replaced_for(&m.model, serves))
            .min_by_key(|m| m.tier)
            .map(|m| m.model.clone());
        let (workers, default_tier, rest) = match cheapest {
            Some(model) => {
                let rest = format!("the lead may use one worker, on {model} at effort low");
                let only = WorkerChoice::from_plan(&ModelChoice::pinned(model, Effort::Low));
                (vec![only], self.default_tier, rest)
            }
            None => (
                Vec::new(),
                Some(Tier::Fast),
                "the lead works alone".to_owned(),
            ),
        };
        let narrowed = Self {
            workers,
            default_tier,
            max_effort: Some(Effort::Low),
            max_parallel: Some(1),
            exempt_roles: self.exempt_roles.clone(),
            exempt_models: self.exempt_models.clone(),
        };
        assert!(
            narrowed.workers.iter().all(|w| w.model.is_some()),
            "a fallback worker is a permitted model, never a tier"
        );
        assert!(
            narrowed.workers.iter().all(|w| w.model.as_ref() != lead),
            "a lead is never its own fallback worker"
        );
        Narrowed {
            policy: narrowed,
            note: Some(format!(
                "no permitted worker model resolved ({cause}); {rest}"
            )),
        }
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
            let effort = cap(worker.effort);
            let narrowed = match (&worker.model, worker.tier) {
                (Some(model), _) => {
                    let Ok(canonical) = catalog.check(model) else {
                        continue;
                    };
                    if !policy.permits_model(&canonical) {
                        continue;
                    }
                    WorkerChoice::from_plan(&ModelChoice::pinned(canonical, effort))
                }
                (None, Some(tier)) => match place_tier(tier, effort, (policy, restricted), catalog)
                {
                    Some(placed) => placed,
                    None => continue,
                },
                (None, None) => unreachable!("validated in new"),
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
        if self.workers.is_empty() {
            return "none: work alone".to_owned();
        }
        let listed: Vec<String> = self.workers.iter().map(ToString::to_string).collect();
        let mut text = format!("each worker on {}", listed.join(" or "));
        if let Some(n) = self.max_parallel {
            text.push_str(&format!(", at most {n} at once"));
        }
        text
    }
}

/// A worker policy as a spawn sends it, with what Archon records when no
/// planned worker survived the model policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narrowed {
    pub policy: WorkerPolicy,
    /// `no permitted worker model resolved (<cause>); ...`, else `None`.
    pub note: Option<String>,
}

/// A tier entry within the model policy: the whole tier when the policy is
/// open and the catalog holds a model of it, else one permitted catalog model.
fn place_tier(
    tier: Tier,
    effort: Effort,
    (policy, restricted): (&ModelPolicy, bool),
    catalog: &ModelCatalog,
) -> Option<WorkerChoice> {
    let whole = !restricted && catalog.candidates().any(|m| m.tier == Some(tier));
    if whole {
        return Some(WorkerChoice::of_tier(tier, effort));
    }
    // An unplaceable tier is an expected outcome; the caller names why.
    let (model, _) = catalog.resolve(tier, None, None, policy).ok()?;
    assert!(policy.permits_model(&model), "resolve honours the policy");
    Some(WorkerChoice::from_plan(&ModelChoice::pinned(model, effort)))
}

/// Why none of `planned`'s workers is permitted under `policy` and `catalog`.
fn unresolved_cause(
    planned: &WorkerPolicy,
    policy: &ModelPolicy,
    catalog: &ModelCatalog,
) -> String {
    assert!(!planned.workers.is_empty(), "validated in new");
    let listed: Vec<String> = planned.workers.iter().map(ToString::to_string).collect();
    let listed = listed.join(" or ");
    if catalog.models().is_empty() {
        return format!("the model catalog is empty; planned: {listed}");
    }
    if policy.allow.is_some() || !policy.deny.is_empty() {
        return format!("the model policy permits none of: {listed}");
    }
    format!("the model catalog places none of: {listed}")
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
    fn a_fallback_worker_keeps_a_superseded_model_whose_successor_is_denied() {
        let entry = |model: &str| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(Tier::Strong),
            is_default: false,
        };
        let mut catalog = ModelCatalog::new(vec![entry("p/lead"), entry("p/old"), entry("p/new")]);
        catalog.add_superseded("p/old".parse().unwrap(), "p/new".parse().unwrap());
        let denied = ModelPolicy::from_lists(&[], &["p/new".parse().unwrap()], None);
        let lead: ModelRef = "p/lead".parse().unwrap();
        let workers = vec![WorkerChoice::of_tier(Tier::Balanced, Effort::Medium)];
        let base = WorkerPolicy::new(workers, Some(Tier::Balanced), Some(2)).unwrap();
        let narrowed = base.fallback(&denied, (&catalog, Some(&lead)), "no balanced model");
        let wire = serde_json::to_value(&narrowed.policy).unwrap().to_string();
        assert!(wire.contains("\"old\""), "{wire}");
        let open = ModelPolicy::default();
        let narrowed = base.fallback(&open, (&catalog, Some(&lead)), "no balanced model");
        let wire = serde_json::to_value(&narrowed.policy).unwrap().to_string();
        assert!(
            wire.contains("\"new\"") && !wire.contains("\"old\""),
            "{wire}"
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
        let narrowed = balanced_default().within(&policy, &catalog(), None);
        assert_eq!(narrowed.note, None);
        let narrowed = narrowed.policy;
        assert_eq!(
            narrowed.workers,
            [WorkerChoice::of_tier(Tier::Balanced, Effort::Low)]
        );
        assert_eq!(narrowed.max_effort, Some(Effort::Low));
        assert_eq!(narrowed.max_parallel, Some(2));
        assert_eq!(
            narrowed.describe(),
            "each worker on the balanced tier at effort low, at most 2 at once"
        );
    }

    fn pinned_at(model: &str, effort: Effort) -> WorkerChoice {
        WorkerChoice::from_plan(&ModelChoice::pinned(model.parse().unwrap(), effort))
    }

    fn allow(models: &[&str]) -> ModelPolicy {
        let refs: Vec<ModelRef> = models.iter().map(|m| m.parse().unwrap()).collect();
        ModelPolicy::from_lists(&refs, &[], None)
    }

    #[test]
    fn narrowing_resolves_a_tier_to_a_permitted_catalog_model() {
        let within = |p: &ModelPolicy| balanced_default().within(p, &catalog(), None);
        let fast = within(&allow(&["a/fast", "b/strong"]));
        assert_eq!(fast.note, None);
        assert_eq!(
            fast.policy.workers,
            [pinned_at("a/fast", Effort::Medium)],
            "the highest permitted at or below balanced"
        );
        assert_eq!(fast.policy.max_parallel, Some(2));
        let denied = ModelPolicy::from_lists(&[], &["a/balanced".parse().unwrap()], None);
        assert_eq!(
            within(&denied).policy.workers,
            [pinned_at("a/fast", Effort::Medium)]
        );
        let open = ModelPolicy::from_lists(&[], &[], None);
        let blind = balanced_default().within(&open, &ModelCatalog::default(), None);
        assert!(
            blind.policy.workers.is_empty(),
            "no tier the catalog cannot place"
        );
    }

    #[test]
    fn with_no_permitted_worker_the_cheapest_permitted_model_takes_one_at_low() {
        let narrowed = balanced_default().within(&allow(&["b/strong"]), &catalog(), None);
        assert_eq!(
            narrowed.policy.workers,
            [pinned_at("b/strong", Effort::Low)]
        );
        assert_eq!(narrowed.policy.max_parallel, Some(1));
        assert_eq!(narrowed.policy.max_effort, Some(Effort::Low));
        let note = narrowed.note.expect("the fallback is recorded");
        assert!(
            note.starts_with(
                "no permitted worker model resolved (the model policy permits none of: "
            ),
            "{note}"
        );
        assert!(
            note.ends_with("one worker, on b/strong at effort low"),
            "{note}"
        );
    }

    #[test]
    fn a_fallback_never_picks_the_lead_s_own_model() {
        let lead: ModelRef = "b/strong".parse().unwrap();
        let only_lead = balanced_default().within(&allow(&["b/strong"]), &catalog(), Some(&lead));
        assert!(only_lead.policy.workers.is_empty(), "{only_lead:?}");
        assert!(only_lead.note.unwrap().ends_with("; the lead works alone"));
        let fast_lead: ModelRef = "a/fast".parse().unwrap();
        let denied = WorkerPolicy::new(
            vec![WorkerChoice::from_plan(&pinned(Effort::Medium))],
            None,
            None,
        );
        let other = denied.unwrap().within(
            &allow(&["a/fast", "b/strong"]),
            &catalog(),
            Some(&fast_lead),
        );
        assert_eq!(
            other.policy.workers,
            [pinned_at("b/strong", Effort::Low)],
            "the cheapest permitted model but the lead's"
        );
    }

    #[test]
    fn with_no_permitted_model_at_all_the_lead_works_alone_and_keeps_its_reviewers() {
        let reviewer: ModelRef = "r/review".parse().unwrap();
        let planned = balanced_default().with_reviewers(vec![reviewer.clone()]);
        for (policy, catalog) in [
            (allow(&["z/lead"]), catalog()),
            (allow(&["z/lead"]), ModelCatalog::default()),
            (
                ModelPolicy::from_lists(&[], &[], None),
                ModelCatalog::default(),
            ),
        ] {
            let narrowed = planned.within(&policy, &catalog, None);
            assert!(
                narrowed.policy.workers.is_empty(),
                "never the lead's own model"
            );
            assert_eq!(narrowed.policy.max_parallel, Some(1));
            assert_eq!(narrowed.policy.max_effort, Some(Effort::Low));
            assert_eq!(
                narrowed.policy.default_tier,
                Some(Tier::Fast),
                "the narrowest tier rule"
            );
            assert_eq!(narrowed.policy.exempt_roles, [REVIEWER_ROLE]);
            assert_eq!(
                narrowed.policy.exempt_models,
                std::slice::from_ref(&reviewer)
            );
            let note = narrowed.note.expect("the lead is told");
            assert!(note.ends_with("; the lead works alone"), "{note}");
            assert_eq!(narrowed.policy.describe(), "none: work alone");
        }
        let empty = planned.within(&allow(&["z/lead"]), &ModelCatalog::default(), None);
        let note = empty.note.unwrap();
        assert!(
            note.contains("(the model catalog is empty; planned: "),
            "{note}"
        );
        let wire = serde_json::to_value(&empty.policy).unwrap();
        assert_eq!(
            wire["workers"],
            serde_json::json!([]),
            "Aether's tier rule, capped"
        );
        assert_eq!(wire["max_parallel"], 1);
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
        let narrowed = planned.within(&deny, &catalog(), None);
        assert_eq!(narrowed.note, None);
        let narrowed = narrowed.policy;
        assert_eq!(narrowed.workers, [pinned_at("a/fast", Effort::Low)]);
        assert_eq!(narrowed.max_effort, Some(Effort::Low));
        assert_eq!(narrowed.exempt_models, [reviewer]);
    }
}
