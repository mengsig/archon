//! Capability tiers. Aether's catalog maps a model to a tier; Archon ranks
//! only the models Aether leaves untiered on an opt-in provider
//! ([`crate::opt_in`]), and resolves a tier to a model from that catalog,
//! within the model policy (ARCHITECTURE §7.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;
use crate::model::ModelRef;
use crate::policy::ModelPolicy;

/// A provider-local capability band, weakest first.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = crate::protocol::schema::AlsoReads("strongest"))]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CatalogModel {
    pub model: ModelRef,
    pub tier: Option<Tier>,
    /// Aether's default model; preferred among equals.
    #[serde(default)]
    pub is_default: bool,
}

/// The connected models in Aether's order, each listed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalog {
    models: Vec<CatalogModel>,
    aliases: BTreeMap<ModelRef, ModelRef>,
    retired: BTreeMap<ModelRef, Option<ModelRef>>,
    superseded: BTreeMap<ModelRef, ModelRef>,
    unavailable: Option<String>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::unavailable("model catalog has not been read")
    }
}

impl ModelCatalog {
    pub fn new(models: Vec<CatalogModel>) -> Self {
        let mut seen = BTreeSet::new();
        let models: Vec<CatalogModel> = models
            .into_iter()
            .filter(|m| seen.insert(m.model.clone()))
            .map(|mut m| {
                m.tier = m.tier.or_else(|| crate::opt_in::placed_tier(&m.model));
                m
            })
            .collect();
        debug_assert_eq!(models.len(), seen.len(), "each model is listed once");
        Self {
            models,
            aliases: BTreeMap::new(),
            retired: BTreeMap::new(),
            superseded: BTreeMap::new(),
            unavailable: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        let mut catalog = Self::new(Vec::new());
        catalog.unavailable = Some(reason.into());
        catalog
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    pub fn not_ready_reason(&self) -> Option<&str> {
        self.unavailable_reason().or_else(|| {
            self.is_empty()
                .then_some("no configured model; configure a provider in Aether")
        })
    }

    pub fn add_alias(&mut self, alias: ModelRef, canonical: ModelRef) {
        if !self.models.iter().any(|entry| entry.model == alias) {
            self.aliases.insert(alias, canonical);
        }
    }

    /// Aliases that resolve: not retired, and never shadowing a listed model.
    pub fn aliases(&self) -> BTreeMap<ModelRef, ModelRef> {
        let listed: BTreeSet<&ModelRef> = self.models.iter().map(|m| &m.model).collect();
        self.aliases
            .iter()
            .filter(|(alias, canonical)| {
                !self.is_retired(alias) && !listed.contains(alias) && alias != canonical
            })
            .map(|(alias, canonical)| (alias.clone(), canonical.clone()))
            .collect()
    }

    /// Resolve only a known alias, without rejecting historical model IDs.
    pub fn canonical_alias<'a>(&'a self, model: &'a ModelRef) -> &'a ModelRef {
        if self.is_retired(model) {
            return model;
        }
        self.aliases.get(model).unwrap_or(model)
    }

    pub fn add_retired(&mut self, model: ModelRef, successor: Option<ModelRef>) {
        self.retired.insert(model, successor);
    }

    pub fn add_superseded(&mut self, model: ModelRef, successor: ModelRef) {
        self.superseded.insert(model, successor);
    }

    pub fn is_retired(&self, model: &ModelRef) -> bool {
        self.retired.contains_key(model)
    }

    pub fn successor(&self, model: &ModelRef) -> Option<&ModelRef> {
        self.retired.get(model).and_then(Option::as_ref)
    }

    pub fn superseded_by(&self, model: &ModelRef) -> Option<&ModelRef> {
        self.superseded
            .get(model)
            .or_else(|| self.aliases.get(model).and_then(|m| self.superseded.get(m)))
    }

    pub fn check(&self, model: &ModelRef) -> Result<ModelRef, ArchonError> {
        if let Some(reason) = self.not_ready_reason() {
            return Err(ArchonError::Unavailable(reason.to_owned()));
        }
        let canonical = self.canonical_alias(model);
        if let Some(successor) = self
            .retired
            .get(model)
            .or_else(|| self.retired.get(canonical))
        {
            let hint = successor
                .as_ref()
                .map(|m| format!("; successor: {m}"))
                .unwrap_or_else(|| format!("; nearest: {}", self.nearest(model)));
            return Err(ArchonError::Invalid(format!(
                "model {model} is retired{hint}"
            )));
        }
        if self.models.iter().any(|entry| &entry.model == canonical) {
            return Ok(canonical.clone());
        }
        let hint = self.nearest(model);
        Err(ArchonError::Invalid(format!(
            "model {model} is not connected; nearest: {hint}"
        )))
    }

    fn nearest(&self, model: &ModelRef) -> String {
        let same_provider = self
            .models
            .iter()
            .any(|entry| entry.model.provider == model.provider && !self.is_retired(&entry.model));
        let mut candidates: Vec<_> = self
            .models
            .iter()
            .filter(|entry| {
                (!same_provider || entry.model.provider == model.provider)
                    && !self.retired.contains_key(&entry.model)
            })
            .collect();
        candidates.sort_by_cached_key(|entry| {
            (
                edit_distance(&entry.model.model, &model.model),
                &entry.model,
            )
        });
        candidates
            .iter()
            .take(3)
            .map(|entry| entry.model.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// This catalog with only the models `keep` accepts, keeping its
    /// aliases, retirements and supersessions.
    pub fn restricted(&self, keep: impl Fn(&CatalogModel) -> bool) -> ModelCatalog {
        let models: Vec<CatalogModel> = self.models.iter().filter(|m| keep(m)).cloned().collect();
        debug_assert!(models.len() <= self.models.len());
        ModelCatalog {
            models,
            aliases: self.aliases.clone(),
            retired: self.retired.clone(),
            superseded: self.superseded.clone(),
            unavailable: self.unavailable.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn models(&self) -> &[CatalogModel] {
        &self.models
    }

    /// Listed canonical models available for automatic choices.
    pub fn candidates(&self) -> impl Iterator<Item = &CatalogModel> {
        self.models.iter().filter(|entry| {
            !self.is_retired(&entry.model) && self.superseded_by(&entry.model).is_none()
        })
    }

    /// Whether `model` is superseded by a listed, non-retired successor
    /// that passes `serves`: only such a successor replaces it in a pick.
    pub fn replaced_for(&self, model: &ModelRef, serves: impl Fn(&CatalogModel) -> bool) -> bool {
        let Some(successor) = self.superseded_by(model) else {
            return false;
        };
        let canonical = self.canonical_alias(successor);
        self.models
            .iter()
            .find(|m| &m.model == canonical)
            .is_some_and(|m| !self.is_retired(&m.model) && serves(m))
    }

    /// Models that can serve a pick between `floor` and `requested` on
    /// `provider` under `policy`; an opt-in provider's only when `provider`
    /// or the policy names it. A superseded one stays unless a successor
    /// on its own provider serves the same pick, at the floor where it did.
    pub fn tier_candidates<'a, 'p>(
        &'a self,
        (requested, floor): (Tier, Tier),
        provider: Option<&'p str>,
        policy: &'p ModelPolicy,
    ) -> impl Iterator<Item = &'a CatalogModel> + use<'a, 'p> {
        debug_assert!(
            floor <= requested,
            "a floor never exceeds the tier asked for"
        );
        let serves = move |m: &CatalogModel| {
            !self.is_retired(&m.model)
                && m.tier.is_some_and(|t| t <= requested)
                && provider.is_none_or(|p| m.model.provider == p)
                && policy.permits_model(&m.model)
                && (provider.is_some() || self.picks_unasked(&m.model, policy))
        };
        self.models.iter().filter(move |entry| {
            let meets_floor = entry.tier >= Some(floor);
            let replaced = self.replaced_for(&entry.model, |s| {
                s.model.provider == entry.model.provider
                    && serves(s)
                    && (!meets_floor || s.tier >= Some(floor))
            });
            serves(entry) && !replaced
        })
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
        self.resolve_above((requested, Tier::Fast), provider, prefer, policy)
    }

    /// [`Self::resolve`] where a successor below `floor` never replaces a
    /// superseded model that meets it.
    pub fn resolve_above(
        &self,
        (requested, floor): (Tier, Tier),
        provider: Option<&str>,
        prefer: Option<&str>,
        policy: &ModelPolicy,
    ) -> Result<(ModelRef, TierChoice), ArchonError> {
        let candidates: Vec<&CatalogModel> = self
            .tier_candidates((requested, floor), provider, policy)
            .collect();
        let on = |p: &str| candidates.iter().any(|m| m.model.provider == p);
        let chosen_provider = match prefer.filter(|p| on(p)) {
            Some(p) => Some(p),
            // An opt-in provider comes last, whatever its name.
            None => candidates
                .iter()
                .map(|m| m.model.provider.as_str())
                .min_by_key(|p| (crate::opt_in::is_opt_in(p), *p)),
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
        for m in self.tier_candidates((Tier::Frontier, Tier::Fast), None, policy) {
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

fn edit_distance(a: &str, b: &str) -> usize {
    let mut previous: Vec<usize> = (0..=b.chars().count()).collect();
    for (i, left) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.chars().enumerate() {
            current.push(
                (current[j] + 1)
                    .min(previous[j + 1] + 1)
                    .min(previous[j] + usize::from(left != right)),
            );
        }
        previous = current;
    }
    previous[b.chars().count()]
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
    fn catalog_checks_aliases_retirement_and_unknown_models() {
        let mut catalog = ModelCatalog::new(vec![entry("p/new", Some(Tier::Strong))]);
        catalog.add_alias("p/alias".parse().unwrap(), "p/new".parse().unwrap());
        catalog.add_retired("p/old".parse().unwrap(), Some("p/new".parse().unwrap()));
        assert_eq!(
            catalog
                .check(&"p/alias".parse().unwrap())
                .unwrap()
                .to_string(),
            "p/new"
        );
        assert!(
            catalog
                .check(&"p/old".parse().unwrap())
                .unwrap_err()
                .to_string()
                .contains("p/new")
        );
        assert_eq!(
            catalog
                .successor(&"p/old".parse().unwrap())
                .unwrap()
                .to_string(),
            "p/new"
        );
        assert!(
            catalog
                .check(&"p/ne".parse().unwrap())
                .unwrap_err()
                .to_string()
                .contains("p/new")
        );
        assert!(ModelCatalog::default().unavailable_reason().is_some());
        assert!(ModelCatalog::new(vec![]).unavailable_reason().is_none());
    }

    #[test]
    fn exported_aliases_exclude_retired_and_listed_ids() {
        let model = |id: &str| CatalogModel {
            model: id.parse().unwrap(),
            tier: None,
            is_default: false,
        };
        let mut catalog = ModelCatalog::new(vec![model("p/new")]);
        catalog.add_alias("p/old".parse().unwrap(), "p/new".parse().unwrap());
        catalog.add_alias("p/gone".parse().unwrap(), "p/new".parse().unwrap());
        catalog.add_retired("p/gone".parse().unwrap(), None);
        catalog.add_alias("p/new".parse().unwrap(), "p/old".parse().unwrap());
        let aliases = catalog.aliases();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[&"p/old".parse().unwrap()], "p/new".parse().unwrap());
    }

    #[test]
    fn retirement_precedes_conflicting_aliases_and_nearest_uses_three_ids() {
        let mut catalog = ModelCatalog::new(vec![
            entry("p/old", Some(Tier::Strong)),
            entry("p/new", Some(Tier::Fast)),
            entry("p/newer", Some(Tier::Fast)),
            entry("p/newest", Some(Tier::Fast)),
            entry("p/other", Some(Tier::Fast)),
        ]);
        catalog.add_alias("p/old".parse().unwrap(), "p/new".parse().unwrap());
        catalog.add_retired("p/old".parse().unwrap(), Some("p/new".parse().unwrap()));
        assert!(
            catalog
                .check(&"p/old".parse().unwrap())
                .unwrap_err()
                .to_string()
                .contains("retired")
        );
        assert_eq!(
            catalog
                .resolve(Tier::Strong, None, None, &open())
                .unwrap()
                .1
                .resolved,
            Tier::Fast
        );
        let retired_alias: ModelRef = "p/archived".parse().unwrap();
        catalog.add_alias(retired_alias.clone(), "p/new".parse().unwrap());
        catalog.add_retired(retired_alias.clone(), None);
        assert_eq!(catalog.canonical_alias(&retired_alias), &retired_alias);
        assert!(
            catalog
                .check(catalog.canonical_alias(&retired_alias))
                .is_err()
        );
        let error = catalog
            .check(&"q/new".parse().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("p/new") && error.contains("p/newer") && error.contains("p/newest"),
            "{error}"
        );
        assert!(!error.contains("p/other"), "{error}");
        let empty = ModelCatalog::new(vec![]);
        assert!(empty.not_ready_reason().is_some());
        assert!(matches!(
            empty.check(&"p/new".parse().unwrap()),
            Err(ArchonError::Unavailable(_))
        ));
    }

    #[test]
    fn superseded_models_are_valid_but_excluded_from_tiers() {
        let mut catalog = ModelCatalog::new(vec![
            entry("p/old", Some(Tier::Strong)),
            entry("p/new", Some(Tier::Fast)),
        ]);
        catalog.add_superseded("p/old".parse().unwrap(), "p/new".parse().unwrap());
        assert!(catalog.check(&"p/old".parse().unwrap()).is_ok());
        assert_eq!(
            catalog
                .resolve(Tier::Strong, None, None, &open())
                .unwrap()
                .0
                .to_string(),
            "p/new"
        );
        assert_eq!(
            catalog.best_tiers(Tier::Strong, &open()).get("p"),
            Some(&Tier::Fast)
        );
    }

    #[test]
    fn a_superseded_model_with_no_permitted_successor_stays_a_tier_candidate() {
        let mut catalog = ModelCatalog::new(vec![
            entry("p/old", Some(Tier::Strong)),
            entry("p/new", Some(Tier::Strong)),
            entry("p/low", Some(Tier::Fast)),
        ]);
        catalog.add_superseded("p/old".parse().unwrap(), "p/new".parse().unwrap());
        let denied = ModelPolicy {
            deny: ["p/new".parse().unwrap()].into(),
            ..open()
        };
        let (model, tier) = catalog.resolve(Tier::Strong, None, None, &denied).unwrap();
        assert_eq!(
            (model.to_string(), tier.resolved),
            ("p/old".into(), Tier::Strong)
        );
        assert_eq!(
            catalog.best_tiers(Tier::Strong, &denied).get("p"),
            Some(&Tier::Strong)
        );
        let (model, _) = catalog.resolve(Tier::Strong, None, None, &open()).unwrap();
        assert_eq!(
            model.to_string(),
            "p/new",
            "a permitted successor replaces it"
        );
    }

    #[test]
    fn a_successor_above_the_requested_tier_does_not_replace_a_superseded_model() {
        let mut catalog = ModelCatalog::new(vec![
            entry("claude-code/sonnet", Some(Tier::Strong)),
            entry("claude-code/opus", Some(Tier::Frontier)),
        ]);
        catalog.add_superseded(
            "claude-code/sonnet".parse().unwrap(),
            "claude-code/opus".parse().unwrap(),
        );
        let (model, tier) = catalog
            .resolve(Tier::Strong, Some("claude-code"), None, &open())
            .unwrap();
        assert_eq!(
            (model.to_string(), tier.resolved),
            ("claude-code/sonnet".into(), Tier::Strong)
        );
        assert_eq!(
            catalog.best_tiers(Tier::Strong, &open()).get("claude-code"),
            Some(&Tier::Strong)
        );
        let (model, _) = catalog
            .resolve(Tier::Frontier, Some("claude-code"), None, &open())
            .unwrap();
        assert_eq!(model.to_string(), "claude-code/opus", "it serves frontier");
    }

    #[test]
    fn a_successor_below_the_floor_or_on_another_provider_keeps_the_superseded_model() {
        let mut catalog = ModelCatalog::new(vec![
            entry("p/old", Some(Tier::Strong)),
            entry("p/new", Some(Tier::Balanced)),
            entry("q/new", Some(Tier::Strong)),
        ]);
        catalog.add_superseded("p/old".parse().unwrap(), "p/new".parse().unwrap());
        let floored = (Tier::Strong, Tier::Strong);
        assert_eq!(
            catalog.best_tiers_above(floored, &open()).get("p"),
            Some(&Tier::Strong)
        );
        let (model, _) = catalog
            .resolve_above(floored, None, Some("p"), &open())
            .unwrap();
        assert_eq!(model.to_string(), "p/old");
        assert_eq!(
            catalog.best_tiers(Tier::Strong, &open()).get("p"),
            Some(&Tier::Balanced),
            "without a floor the successor serves"
        );
        let mut across = ModelCatalog::new(vec![
            entry("p/old", Some(Tier::Strong)),
            entry("q/new", Some(Tier::Strong)),
        ]);
        across.add_superseded("p/old".parse().unwrap(), "q/new".parse().unwrap());
        let (model, _) = across
            .resolve(Tier::Strong, None, Some("p"), &open())
            .unwrap();
        assert_eq!(
            model.to_string(),
            "p/old",
            "a routed provider keeps its model"
        );
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
