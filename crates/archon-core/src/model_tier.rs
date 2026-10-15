//! Capability tiers. Aether's catalog is the one place that maps a model to
//! a tier; Archon never ranks models itself, it resolves a tier to a model
//! from that catalog, within the model policy (ARCHITECTURE §7.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;
use crate::model::ModelRef;
use crate::policy::ModelPolicy;

/// A provider-local capability band, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Fast,
    Balanced,
    /// `strongest` is the legacy spelling, kept for stored plans and pipelines.
    #[serde(alias = "strongest")]
    Strong,
    Frontier,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Fast, Tier::Balanced, Tier::Strong, Tier::Frontier];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
            Self::Frontier => "frontier",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Tier {
    type Err = ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "strong" | "strongest" => Ok(Self::Strong),
            "frontier" => Ok(Self::Frontier),
            other => Err(ArchonError::Invalid(format!(
                "tier {other:?} is not one of fast, balanced, strong, frontier"
            ))),
        }
    }
}

/// How a plan entry asked by tier was resolved; recorded on the plan and on
/// every attempt that runs it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierChoice {
    pub requested: Tier,
    /// The provider the entry asked for; `None` let Archon prefer one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The resolved model's tier: below `requested` when it fell back.
    pub resolved: Tier,
}

impl TierChoice {
    pub fn fell_back(&self) -> bool {
        self.resolved < self.requested
    }
}

impl fmt::Display for TierChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tier {}", self.requested)?;
        if let Some(provider) = &self.provider {
            write!(f, " on {provider}")?;
        }
        if self.fell_back() {
            write!(f, ", fell back to {}", self.resolved)?;
        }
        Ok(())
    }
}

/// One connected model of Aether's catalog; `tier` is Aether's, or `None`
/// when Aether could not place it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub model: ModelRef,
    pub tier: Option<Tier>,
    /// Aether's default model; preferred among equals.
    #[serde(default)]
    pub is_default: bool,
}

/// The connected models in Aether's order, each listed once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    models: Vec<CatalogModel>,
}

impl ModelCatalog {
    pub fn new(models: Vec<CatalogModel>) -> Self {
        let mut seen = BTreeSet::new();
        let models = models
            .into_iter()
            .filter(|m| seen.insert(m.model.clone()))
            .collect();
        Self { models }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn models(&self) -> &[CatalogModel] {
        &self.models
    }

    pub fn tier_of(&self, model: &ModelRef) -> Option<Tier> {
        self.models
            .iter()
            .find(|m| &m.model == model)
            .and_then(|m| m.tier)
    }

    /// The model for `requested`: on `provider` when given, else on
    /// `prefer` first, then any other provider. Within a provider the
    /// highest tier at or below `requested` wins, Aether's default first,
    /// then catalog order. Refused, naming the tiers there are, when no
    /// allowed model sits at or below `requested`.
    pub fn resolve(
        &self,
        requested: Tier,
        provider: Option<&str>,
        prefer: Option<&str>,
        policy: &ModelPolicy,
    ) -> Result<(ModelRef, TierChoice), ArchonError> {
        let usable = |m: &&CatalogModel| {
            m.tier.is_some_and(|t| t <= requested)
                && provider.is_none_or(|p| m.model.provider == p)
                && policy.permits_model(&m.model)
        };
        let candidates: Vec<&CatalogModel> = self.models.iter().filter(usable).collect();
        let on = |p: &str| candidates.iter().any(|m| m.model.provider == p);
        let chosen_provider = match prefer.filter(|p| on(p)) {
            Some(p) => Some(p),
            None => candidates.iter().map(|m| m.model.provider.as_str()).min(),
        };
        let Some(chosen_provider) = chosen_provider else {
            return Err(self.unresolvable(requested, provider, policy));
        };
        let best = candidates
            .iter()
            .filter(|m| m.model.provider == chosen_provider)
            .max_by_key(|m| (m.tier, m.is_default, std::cmp::Reverse(self.position(m))))
            .expect("the chosen provider has a candidate");
        let resolved = best.tier.expect("candidates have a tier");
        debug_assert!(resolved <= requested);
        Ok((
            best.model.clone(),
            TierChoice {
                requested,
                provider: provider.map(str::to_owned),
                resolved,
            },
        ))
    }

    fn position(&self, model: &CatalogModel) -> usize {
        self.models
            .iter()
            .position(|m| m.model == model.model)
            .expect("a candidate is in the catalog")
    }

    fn unresolvable(
        &self,
        requested: Tier,
        provider: Option<&str>,
        policy: &ModelPolicy,
    ) -> ArchonError {
        let mut tiers: BTreeMap<&str, BTreeSet<Tier>> = BTreeMap::new();
        for m in self
            .models
            .iter()
            .filter(|m| policy.permits_model(&m.model))
        {
            if let Some(tier) = m.tier {
                tiers.entry(&m.model.provider).or_default().insert(tier);
            }
        }
        let available = match tiers.is_empty() {
            true => "none".to_owned(),
            false => tiers
                .iter()
                .map(|(p, ts)| {
                    let ts: Vec<_> = ts.iter().map(|t| t.as_str()).collect();
                    format!("{p}: {}", ts.join(", "))
                })
                .collect::<Vec<_>>()
                .join("; "),
        };
        let on = provider.map_or(String::new(), |p| format!(" on {p}"));
        ArchonError::Invalid(format!(
            "no connected, allowed model at or below tier {requested}{on}; available tiers: \
             {available}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(model: &str, tier: Option<Tier>) -> CatalogModel {
        CatalogModel {
            model: model.parse().unwrap(),
            tier,
            is_default: false,
        }
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog::new(vec![
            entry("a/fast-1", Some(Tier::Fast)),
            entry("a/strong-1", Some(Tier::Strong)),
            entry("a/frontier-1", Some(Tier::Frontier)),
            entry("b/fast-2", Some(Tier::Fast)),
            entry("b/balanced-2", Some(Tier::Balanced)),
            entry("b/strong-2", Some(Tier::Strong)),
            entry("b/unplaced", None),
        ])
    }

    fn open() -> ModelPolicy {
        ModelPolicy::default()
    }

    #[test]
    fn strongest_is_an_alias_of_strong() {
        let tier: Tier = serde_json::from_str("\"strongest\"").unwrap();
        assert_eq!(tier, Tier::Strong);
        assert_eq!(serde_json::to_string(&tier).unwrap(), "\"strong\"");
        assert_eq!("strongest".parse::<Tier>().unwrap(), Tier::Strong);
        assert!(Tier::Fast < Tier::Balanced && Tier::Strong < Tier::Frontier);
    }

    #[test]
    fn the_preferred_provider_wins_and_an_explicit_one_binds() {
        let (model, choice) = catalog()
            .resolve(Tier::Strong, None, Some("b"), &open())
            .unwrap();
        assert_eq!(model.to_string(), "b/strong-2");
        assert!(!choice.fell_back());
        let (model, _) = catalog()
            .resolve(Tier::Strong, Some("a"), Some("b"), &open())
            .unwrap();
        assert_eq!(model.to_string(), "a/strong-1");
        let (model, _) = catalog()
            .resolve(Tier::Fast, None, Some("missing"), &open())
            .unwrap();
        assert_eq!(model.to_string(), "a/fast-1", "else the first provider");
    }

    #[test]
    fn a_missing_tier_falls_back_downward_within_the_provider() {
        let (model, choice) = catalog()
            .resolve(Tier::Frontier, None, Some("b"), &open())
            .unwrap();
        assert_eq!(model.to_string(), "b/strong-2");
        assert_eq!(choice.resolved, Tier::Strong);
        assert!(choice.fell_back());
        let (model, choice) = catalog()
            .resolve(Tier::Balanced, Some("a"), None, &open())
            .unwrap();
        assert_eq!(model.to_string(), "a/fast-1");
        assert_eq!(choice.to_string(), "tier balanced on a, fell back to fast");
    }

    #[test]
    fn policy_denial_moves_resolution_and_nothing_left_is_refused() {
        let denied = ModelPolicy::from_lists(&[], &["b/strong-2".parse().unwrap()], None);
        let (model, choice) = catalog()
            .resolve(Tier::Strong, None, Some("b"), &denied)
            .unwrap();
        assert_eq!(model.to_string(), "b/balanced-2", "the next tier down");
        assert!(choice.fell_back());
        let only = ModelPolicy::from_lists(&["a/strong-1".parse().unwrap()], &[], None);
        let error = catalog()
            .resolve(Tier::Fast, None, Some("a"), &only)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("at or below tier fast") && error.contains("available tiers: a: strong"),
            "{error}"
        );
        let error = catalog()
            .resolve(Tier::Fast, Some("c"), None, &open())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("on c")
                && error.contains("a: fast, strong, frontier; b: fast, balanced, strong"),
            "{error}"
        );
        let error = ModelCatalog::default()
            .resolve(Tier::Fast, None, None, &open())
            .unwrap_err()
            .to_string();
        assert!(error.ends_with("available tiers: none"), "{error}");
    }

    #[test]
    fn aether_s_default_is_preferred_among_equals() {
        let mut models = vec![
            entry("a/fast-old", Some(Tier::Fast)),
            entry("a/fast-new", Some(Tier::Fast)),
        ];
        let first = ModelCatalog::new(models.clone())
            .resolve(Tier::Fast, None, None, &open())
            .unwrap();
        assert_eq!(first.0.to_string(), "a/fast-old", "catalog order");
        models[1].is_default = true;
        let default = ModelCatalog::new(models)
            .resolve(Tier::Fast, None, None, &open())
            .unwrap();
        assert_eq!(default.0.to_string(), "a/fast-new");
    }
}
