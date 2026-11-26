//! Opt-in providers: subscriptions too small to spend on Archon's own picks.
//! Archon picks their models by tier, route or learned evidence only when the
//! model policy's allowlist names them; an explicit `model_plan` entry always may.
//! Antigravity (agy) is the one such provider. Aether lists its models
//! untiered, so Archon places them here.

use crate::model::ModelRef;
use crate::model_tier::{ModelCatalog, Tier};
use crate::policy::ModelPolicy;

/// Aether's engine id for the Antigravity CLI.
pub const ANTIGRAVITY: &str = "antigravity";

/// The most an opt-in provider weighs when `provider_weights` leaves it out.
pub const OPT_IN_WEIGHT: u32 = 1;

pub fn is_opt_in(provider: &str) -> bool {
    provider == ANTIGRAVITY
}

/// Archon's tier for an Antigravity model, from agy's own model list: Flash
/// is fast; Neon and Hydrogen offer an xhigh variant and Neon is agy's default.
pub fn placed_tier(model: &ModelRef) -> Option<Tier> {
    if model.provider != ANTIGRAVITY {
        return None;
    }
    let tier = match model.model.as_str() {
        "gemini-3.8-flash" => Tier::Fast,
        "gemini-4-carbon" | "gemini-4-argon" | "gpt-oss-120b" => Tier::Balanced,
        "gemini-4-neon" | "gemini-4-hydrogen" | "claude-sonnet-5-5" => Tier::Strong,
        "claude-opus-5-5" => Tier::Frontier,
        _ => return None,
    };
    Some(tier)
}

/// The providers that serve a secondary route's model first hand: agy also
/// serves Claude and GPT models, which their own vendors' engines serve first.
pub fn original_providers(model: &ModelRef) -> Option<&'static [&'static str]> {
    if model.provider != ANTIGRAVITY {
        return None;
    }
    let name = model.model.as_str();
    if name.starts_with("claude-") {
        return Some(&["claude-code", "anthropic"]);
    }
    if name.starts_with("gpt-") {
        return Some(&["codex", "openai"]);
    }
    None
}

impl ModelCatalog {
    /// Whether Archon may choose `model` when nobody named it: always off an
    /// opt-in provider; on one only when `policy` allows it by name, or for a
    /// secondary route while its original providers list no tiered model.
    pub fn picks_unasked(&self, model: &ModelRef, policy: &ModelPolicy) -> bool {
        if !is_opt_in(&model.provider) {
            return true;
        }
        if policy.names_model(model) {
            return true;
        }
        let Some(originals) = original_providers(model) else {
            return false;
        };
        let original_listed = self
            .candidates()
            .any(|m| m.tier.is_some() && originals.contains(&m.model.provider.as_str()));
        debug_assert!(originals.iter().all(|p| !is_opt_in(p)));
        !original_listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effort, ModelChoice, ModelPlan};
    use crate::model_tier::CatalogModel;
    use crate::provider_route::{ProviderRoute, ProviderWeights};

    fn id(text: &str) -> ModelRef {
        text.parse().unwrap()
    }

    fn catalog(models: &[(&str, Option<Tier>)]) -> ModelCatalog {
        let entry = |(model, tier): &(&str, Option<Tier>)| CatalogModel {
            model: id(model),
            tier: *tier,
            is_default: false,
        };
        ModelCatalog::new(models.iter().map(entry).collect())
    }

    #[test]
    fn the_catalog_places_untiered_antigravity_models_and_keeps_aethers_tiers() {
        let catalog = catalog(&[
            ("antigravity/gemini-3.8-flash", None),
            ("antigravity/gemini-4-carbon", None),
            ("antigravity/gemini-4-neon", None),
            ("antigravity/gemini-4-argon", Some(Tier::Strong)),
            ("antigravity/gemini-pro-orchestrator", None),
            ("other/untiered", None),
        ]);
        let tier = |model: &str| catalog.tier_of(&id(model));
        assert_eq!(tier("antigravity/gemini-3.8-flash"), Some(Tier::Fast));
        assert_eq!(tier("antigravity/gemini-4-carbon"), Some(Tier::Balanced));
        assert_eq!(tier("antigravity/gemini-4-neon"), Some(Tier::Strong));
        assert_eq!(tier("antigravity/gemini-4-argon"), Some(Tier::Strong));
        assert_eq!(tier("antigravity/gemini-pro-orchestrator"), None);
        assert_eq!(tier("other/untiered"), None, "only Antigravity is placed");
    }

    #[test]
    fn only_a_named_or_orphaned_secondary_route_is_picked_unasked() {
        let flash = id("antigravity/gemini-3.8-flash");
        let opus = id("antigravity/claude-opus-5-5");
        let open = ModelPolicy::default();
        let both = catalog(&[
            ("claude-code/opus", Some(Tier::Frontier)),
            ("antigravity/gemini-3.8-flash", None),
            ("antigravity/claude-opus-5-5", None),
        ]);
        assert!(both.picks_unasked(&id("claude-code/opus"), &open));
        assert!(!both.picks_unasked(&flash, &open), "opt-in");
        assert!(!both.picks_unasked(&opus, &open), "the original serves it");
        let named = ModelPolicy::from_lists(std::slice::from_ref(&flash), &[], None);
        assert!(both.picks_unasked(&flash, &named), "the allowlist opts in");
        let alone = catalog(&[
            ("antigravity/gemini-3.8-flash", None),
            ("antigravity/claude-opus-5-5", None),
        ]);
        assert!(alone.picks_unasked(&opus, &open), "Antigravity-only user");
        assert!(!alone.picks_unasked(&flash, &open), "Gemini stays opt-in");
        let frontier = |c: &ModelCatalog| c.resolve(Tier::Frontier, None, None, &open).unwrap().0;
        assert_eq!(
            frontier(&both),
            id("claude-code/opus"),
            "the original serves"
        );
        assert_eq!(frontier(&alone), opus, "the secondary route serves");
    }

    fn mixed() -> ModelCatalog {
        catalog(&[
            ("antigravity/gemini-3.8-flash", None),
            ("antigravity/gemini-4-neon", None),
            ("claude-code/haiku", Some(Tier::Fast)),
            ("claude-code/sonnet", Some(Tier::Strong)),
        ])
    }

    #[test]
    fn default_tier_picks_never_land_on_antigravity() {
        let catalog = mixed();
        let open = ModelPolicy::default();
        for tier in Tier::ALL {
            let (model, _) = catalog.resolve(tier, None, None, &open).unwrap();
            assert_eq!(model.provider, "claude-code", "{tier}");
        }
        assert!(
            !catalog
                .best_tiers(Tier::Frontier, &open)
                .contains_key(ANTIGRAVITY),
            "routing never sees it"
        );
        let weights = ProviderWeights::new([(ANTIGRAVITY.to_owned(), 0)].into(), 1);
        let only = catalog.restricted(|m| m.model.provider == ANTIGRAVITY);
        let route = ProviderRoute {
            weights: &weights,
            key: "qi_x",
            held: None,
            quota: None,
        };
        assert_eq!(only.item_provider(route, &open), None);
        assert!(only.resolve(Tier::Fast, None, None, &open).is_err());
    }

    #[test]
    fn an_explicit_plan_or_provider_runs_antigravity() {
        let catalog = mixed();
        let open = ModelPolicy::default();
        let flash = id("antigravity/gemini-3.8-flash");
        assert_eq!(
            catalog.check(&flash).unwrap(),
            flash,
            "a pinned model is listed"
        );
        let plan: ModelPlan = serde_json::from_value(serde_json::json!({
            "review": [{"model": "antigravity/gemini-3.8-flash", "effort": "low"}]
        }))
        .unwrap();
        open.check_plan(&plan).unwrap();
        let (model, choice) = catalog
            .resolve(Tier::Strong, Some(ANTIGRAVITY), None, &open)
            .unwrap();
        assert_eq!(model, id("antigravity/gemini-4-neon"));
        assert_eq!(choice.resolved, Tier::Strong);
    }

    #[test]
    fn model_policy_allow_opts_in_and_deny_refuses_antigravity() {
        let catalog = mixed();
        let flash = id("antigravity/gemini-3.8-flash");
        let allowed =
            ModelPolicy::from_lists(&[flash.clone(), id("claude-code/sonnet")], &[], None);
        let (model, _) = catalog.resolve(Tier::Fast, None, None, &allowed).unwrap();
        assert_eq!(model, flash, "the only allowed fast model");
        let (model, _) = catalog.resolve(Tier::Strong, None, None, &allowed).unwrap();
        assert_eq!(
            model.provider, "claude-code",
            "an opt-in provider comes last"
        );
        let weights = ProviderWeights::new([("claude-code".to_owned(), 7)].into(), 5);
        assert_eq!(weights.weight(ANTIGRAVITY), OPT_IN_WEIGHT, "low weight");
        assert_eq!(weights.weight("codex"), 5);
        let denied = ModelPolicy::from_lists(&[], std::slice::from_ref(&flash), None);
        let pinned = ModelChoice::pinned(flash.clone(), Effort::Low);
        assert!(denied.check(&pinned).is_err());
        assert!(
            catalog
                .resolve(Tier::Fast, Some(ANTIGRAVITY), None, &denied)
                .is_err(),
            "deny wins over a named provider"
        );
        let outside = ModelPolicy::from_lists(&[id("claude-code/sonnet")], &[], None);
        assert!(
            outside.check(&pinned).is_err(),
            "an allowlist without it refuses it"
        );
    }

    #[test]
    fn unknown_antigravity_headroom_routes_at_full_weight() {
        let catalog = mixed();
        let flash = id("antigravity/gemini-3.8-flash");
        let only_flash = ModelPolicy::from_lists(&[flash], &[], None);
        let weights = ProviderWeights::new(Default::default(), 1);
        let quota = crate::quota::QuotaView::default();
        let route = ProviderRoute {
            weights: &weights,
            key: "qi_y",
            held: None,
            quota: Some(&quota),
        };
        assert_eq!(quota.factor(ANTIGRAVITY), 1.0, "unknown headroom");
        assert_eq!(catalog.item_provider(route, &only_flash), Some(ANTIGRAVITY));
    }
}
