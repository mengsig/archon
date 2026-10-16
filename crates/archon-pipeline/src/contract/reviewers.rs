//! A light profile for reviewers without an explicit model plan.

use archon_core::config::ReviewConfig;
use archon_core::model::{ModelChoice, ModelRef};
use archon_core::model_tier::ModelCatalog;
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
    let providers = catalog.best_tiers(tier, policy);
    let provider = providers
        .iter()
        .filter(|(_, best)| **best == tier)
        .map(|(provider, _)| *provider)
        .min_by_key(|provider| {
            let is_own = *provider == own.provider;
            (is_own == profile.prefer_other_provider, *provider)
        });
    if let Some(provider) = provider {
        let (model, resolved) = catalog
            .resolve(tier, Some(provider), None, policy)
            .expect("an eligible provider has an allowed model at the requested tier");
        assert_eq!(
            resolved.resolved, tier,
            "review defaults never fall below their tier"
        );
        assert!(
            policy.permits_model(&model),
            "review defaults respect the model policy"
        );
        return format!(
            "- Delegate every reviewer with role `reviewer`, on {model} at tier {tier} \
             and effort {effort}.\n"
        );
    }
    unresolved_rules(own, policy, profile, effort)
}

fn unresolved_rules(
    own: &ModelRef,
    policy: &ModelPolicy,
    profile: &ReviewConfig,
    effort: archon_core::model::Effort,
) -> String {
    assert!(
        effort <= profile.default_effort,
        "policy may only lower default effort"
    );
    assert!(
        policy.max_effort.is_none_or(|cap| effort <= cap),
        "effort respects policy"
    );
    let tier = profile.default_tier;
    let preference = if profile.prefer_other_provider {
        format!(
            " Prefer a provider other than {} when one meets that tier.",
            own.provider
        )
    } else {
        String::new()
    };
    format!(
        "- Delegate every reviewer with role `reviewer`, at tier {tier} and effort {effort}, \
         within the model policy: {}.{preference} No allowed model at that tier is in \
         Archon's cached catalog; use Aether's list_models to resolve it, and report a \
         blocker if none meets it.\n",
        policy.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::Effort;
    use archon_core::model_tier::{CatalogModel, Tier};

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

    #[test]
    fn missing_catalog_or_tier_names_the_profile_and_blocker() {
        let own: ModelRef = "claude-code/sonnet".parse().unwrap();
        let config = ReviewConfig {
            default_tier: Tier::Frontier,
            ..ReviewConfig::default()
        };
        for catalog in [ModelCatalog::default(), catalog()] {
            let rules =
                reviewer_profile_rules(None, &own, &ModelPolicy::default(), &config, &catalog);
            assert!(rules.contains("tier frontier and effort low"));
            assert!(rules.contains("report a blocker"));
            assert!(rules.contains("Prefer a provider other than claude-code"));
        }
    }
}
