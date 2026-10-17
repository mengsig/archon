//! A light profile for reviewers without an explicit model plan.

use archon_core::config::ReviewConfig;
use archon_core::model::{ModelChoice, ModelRef};
use archon_core::model_tier::{ModelCatalog, Tier};
use archon_core::policy::ModelPolicy;

/// Explicit plans keep their contract; unnamed reviewers use the configured profile.
pub fn reviewer_profile_rules(
    named: Option<&[ModelChoice]>,
    own: &ModelRef,
    policy: &ModelPolicy,
    profile: &ReviewConfig,
    catalog: &ModelCatalog,
) -> String {
    if let Some(choices) = named.filter(|choices| !choices.is_empty()) {
        return super::named_reviewer_rules(choices);
    }
    let tier = profile.default_tier;
    let effort = policy.max_effort.map_or(profile.default_effort, |cap| {
        cap.min(profile.default_effort)
    });
    if let Some((model, resolved)) = resolve_default(tier, own, policy, profile, catalog) {
        assert!(
            policy.permits_model(&model),
            "review defaults respect the model policy"
        );
        assert!(resolved <= tier, "review defaults never exceed their tier");
        if resolved == tier {
            return format!(
                "- Delegate every reviewer with role `reviewer`, on {model} at tier {tier} \
                 and effort {effort}.\n"
            );
        }
        return format!(
            "- Delegate every reviewer with role `reviewer`, on {model} at effort {effort}: \
             {tier} unavailable under policy ({}); using {model}.\n",
            policy.describe()
        );
    }
    unplaced_rules(own, policy, effort)
}

/// The permitted model nearest the tier from below: the strongest tier first,
/// then another provider than `own` when the profile prefers one, then name.
fn resolve_default(
    tier: Tier,
    own: &ModelRef,
    policy: &ModelPolicy,
    profile: &ReviewConfig,
    catalog: &ModelCatalog,
) -> Option<(ModelRef, Tier)> {
    let providers = catalog.best_tiers(tier, policy);
    let provider = providers
        .iter()
        .max_by_key(|(provider, best)| {
            let other = profile.prefer_other_provider && **provider != own.provider;
            (**best, other, std::cmp::Reverse(**provider))
        })
        .map(|(provider, _)| *provider)?;
    let (model, choice) = catalog
        .resolve(tier, Some(provider), None, policy)
        .expect("an eligible provider has an allowed model at or below the tier");
    Some((model, choice.resolved))
}

/// Neither the catalog nor the policy places a model: name one the policy
/// permits, never one it would refuse, and never send the agent to look.
fn unplaced_rules(
    own: &ModelRef,
    policy: &ModelPolicy,
    effort: archon_core::model::Effort,
) -> String {
    assert!(
        policy.max_effort.is_none_or(|cap| effort <= cap),
        "effort respects policy"
    );
    let listed = policy
        .allow
        .iter()
        .flatten()
        .find(|model| policy.permits_model(model));
    let model = match (policy.permits_model(own), listed) {
        (true, _) => own,
        (false, Some(model)) => model,
        (false, None) => {
            return format!(
                "- Delegate no reviewer: the model policy ({}) allows no model.\n",
                policy.describe()
            );
        }
    };
    format!(
        "- Delegate every reviewer with role `reviewer`, on {model} at effort {effort}: \
         Archon's cached catalog places no allowed model by tier, and the model policy \
         ({}) permits it.\n",
        policy.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::Effort;
    use archon_core::model_tier::CatalogModel;

    fn catalog() -> ModelCatalog {
        ModelCatalog::new(
            [
                ("claude-code/sonnet", Tier::Balanced),
                ("codex/balanced", Tier::Balanced),
                ("codex/strong", Tier::Strong),
                ("third/fast", Tier::Fast),
            ]
            .into_iter()
            .map(|(name, tier)| CatalogModel {
                model: name.parse().unwrap(),
                tier: Some(tier),
                is_default: false,
            })
            .collect(),
        )
    }

    #[test]
    fn unnamed_reviewers_get_configured_tier_effort_and_other_provider() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let config = ReviewConfig {
            default_tier: Tier::Strong,
            default_effort: Effort::Medium,
            ..ReviewConfig::default()
        };
        for named in [None, Some(&[][..])] {
            let rules =
                reviewer_profile_rules(named, &own, &ModelPolicy::default(), &config, &catalog());
            assert!(
                rules.contains("codex/strong at tier strong and effort medium"),
                "{rules}"
            );
            assert!(rules.contains("role `reviewer`"));
        }
    }

    #[test]
    fn provider_preference_can_be_disabled_and_policy_always_wins() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let mut config = ReviewConfig::default();
        let rules =
            reviewer_profile_rules(None, &own, &ModelPolicy::default(), &config, &catalog());
        assert!(rules.contains("codex/balanced at tier balanced and effort low"));
        config.prefer_other_provider = false;
        let rules =
            reviewer_profile_rules(None, &own, &ModelPolicy::default(), &config, &catalog());
        assert!(rules.contains("claude-code/sonnet at tier balanced and effort low"));
        config.prefer_other_provider = true;
        config.default_effort = Effort::High;
        let policy =
            ModelPolicy::from_lists(&[], &["codex/balanced".parse().unwrap()], Some(Effort::Low));
        let rules = reviewer_profile_rules(None, &own, &policy, &config, &catalog());
        assert!(rules.contains("claude-code/sonnet at tier balanced and effort low"));
        assert!(!rules.contains("third/fast"));
    }

    #[test]
    fn explicit_review_plans_are_unchanged() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let named = [ModelChoice {
            model: own.clone(),
            effort: Effort::High,
            tier: None,
        }];
        let policy = ModelPolicy::default();
        let rules = reviewer_profile_rules(
            Some(&named),
            &own,
            &policy,
            &ReviewConfig::default(),
            &catalog(),
        );
        assert_eq!(
            rules,
            super::super::reviewer_model_rules(Some(&named), &own, &policy)
        );
        assert!(rules.contains("claude-code/sonnet at effort high"));
    }

    fn assert_no_lookup_instruction(rules: &str) {
        assert!(!rules.contains("list_models"), "{rules}");
        assert!(!rules.contains("Prefer a provider other than"), "{rules}");
    }

    fn policy_allowing(models: &[&str]) -> ModelPolicy {
        let allow: Vec<ModelRef> = models.iter().map(|m| m.parse().unwrap()).collect();
        ModelPolicy::from_lists(&allow, &[], None)
    }

    fn single_fast_catalog() -> ModelCatalog {
        ModelCatalog::new(
            [
                ("codex/luna", Tier::Fast),
                ("claude-code/sonnet", Tier::Balanced),
                ("codex/sol", Tier::Balanced),
            ]
            .into_iter()
            .map(|(name, tier)| CatalogModel {
                model: name.parse().unwrap(),
                tier: Some(tier),
                is_default: false,
            })
            .collect(),
        )
    }

    #[test]
    fn single_fast_model_policy_resolves_to_that_model_without_lookup() {
        let own: ModelRef = "codex/luna".parse().unwrap();
        let policy = policy_allowing(&["codex/luna"]);
        let rules = reviewer_profile_rules(
            None,
            &own,
            &policy,
            &ReviewConfig::default(),
            &single_fast_catalog(),
        );
        assert!(rules.contains("on codex/luna at effort low"), "{rules}");
        assert!(rules.contains("balanced unavailable under policy"));
        assert_no_lookup_instruction(&rules);
        assert!(!rules.contains("sonnet") && !rules.contains("sol"));
    }

    #[test]
    fn unplaced_models_fall_back_to_a_permitted_one_never_a_lookup() {
        let own: ModelRef = "codex/luna".parse().unwrap();
        let config = ReviewConfig::default();
        for catalog in [ModelCatalog::default(), catalog()] {
            let rules = reviewer_profile_rules(
                None,
                &own,
                &policy_allowing(&["codex/luna"]),
                &config,
                &catalog,
            );
            assert!(rules.contains("on codex/luna at effort low"), "{rules}");
            assert_no_lookup_instruction(&rules);
        }
        let rules = reviewer_profile_rules(
            None,
            &own,
            &policy_allowing(&["third/other"]),
            &config,
            &ModelCatalog::default(),
        );
        assert!(rules.contains("on third/other at effort low"), "{rules}");
        let rules = reviewer_profile_rules(
            None,
            &own,
            &policy_allowing(&[]).narrowed_by(&policy_allowing(&["third/other"])),
            &config,
            &ModelCatalog::default(),
        );
        assert!(rules.contains("on third/other"), "{rules}");
        let none = ModelPolicy {
            allow: Some(Default::default()),
            ..ModelPolicy::default()
        };
        let rules = reviewer_profile_rules(None, &own, &none, &config, &catalog());
        assert!(rules.contains("Delegate no reviewer"), "{rules}");
    }

    #[test]
    fn balanced_models_of_both_providers_still_prefer_the_other_provider() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let policy = policy_allowing(&["claude-code/sonnet", "codex/sol", "codex/luna"]);
        let rules = reviewer_profile_rules(
            None,
            &own,
            &policy,
            &ReviewConfig::default(),
            &single_fast_catalog(),
        );
        assert!(
            rules.contains("codex/sol at tier balanced and effort low"),
            "{rules}"
        );
    }

    #[test]
    fn repository_policy_narrows_the_global_one() {
        let own: ModelRef = "codex/luna".parse().unwrap();
        let global = policy_allowing(&["codex/luna", "codex/sol", "claude-code/sonnet"]);
        let config = ReviewConfig::default();
        let wide = reviewer_profile_rules(None, &own, &global, &config, &single_fast_catalog());
        assert!(
            wide.contains("claude-code/sonnet at tier balanced"),
            "{wide}"
        );
        let narrowed = global.narrowed_by(&policy_allowing(&["codex/luna"]));
        let rules = reviewer_profile_rules(None, &own, &narrowed, &config, &single_fast_catalog());
        assert!(rules.contains("on codex/luna at effort low"), "{rules}");
        assert_no_lookup_instruction(&rules);
        assert!(!rules.contains("sonnet") && !rules.contains("sol"));
    }

    #[test]
    fn a_tier_above_the_catalog_falls_back_to_the_best_permitted_tier() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let config = ReviewConfig {
            default_tier: Tier::Frontier,
            ..ReviewConfig::default()
        };
        let rules =
            reviewer_profile_rules(None, &own, &ModelPolicy::default(), &config, &catalog());
        assert!(rules.contains("on codex/strong at effort low"), "{rules}");
        assert!(rules.contains("frontier unavailable under policy"));
        assert_no_lookup_instruction(&rules);
    }
}
