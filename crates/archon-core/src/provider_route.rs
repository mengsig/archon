//! Provider weights for tier-only choices (`[models] provider_weights`):
//! each item holds one provider, picked once by weight from a hash of its
//! id, and a stage moves off it only when its capability floor rules it out.

use std::collections::BTreeMap;

use crate::model_tier::{ModelCatalog, Tier};
use crate::policy::ModelPolicy;

/// The weight of a provider the table leaves out, unless configured.
pub const DEFAULT_PROVIDER_WEIGHT: u32 = 1;

/// Each provider's relative share of tier-only items; one the table leaves
/// out weighs `default`, so a new subscription joins by its models alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderWeights {
    table: BTreeMap<String, u32>,
    default: u32,
}

impl ProviderWeights {
    pub fn new(table: BTreeMap<String, u32>, default: u32) -> Self {
        assert!(
            table.keys().all(|p| !p.is_empty() && !p.contains('/')),
            "weights are keyed by provider, not by model"
        );
        Self { table, default }
    }

    pub fn weight(&self, provider: &str) -> u32 {
        self.table.get(provider).copied().unwrap_or(self.default)
    }
}

/// What routes one item's tier-only choices: the weights, the key its pick
/// hashes (the item id) and the provider it already holds, if any.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRoute<'a> {
    pub weights: &'a ProviderWeights,
    pub key: &'a str,
    pub held: Option<&'a str>,
}

/// A point in `[0, 1)` fixed by `key`: FNV-1a, then the SplitMix64
/// finalizer, so ids that differ in one character land far apart.
pub fn route_point(key: &str) -> f64 {
    assert!(!key.is_empty(), "a route hashes a non-empty key");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;
    let point = (hash >> 11) as f64 / (1_u64 << 53) as f64;
    debug_assert!((0.0..1.0).contains(&point), "{point} is outside [0, 1)");
    point
}

/// The entry whose share of the total weight covers `point`, in the order
/// given; `None` when no weight is positive. Weights need not be whole, so
/// a caller may scale them (by quota headroom, say) before picking.
pub fn weighted_pick<'a>(point: f64, weighted: &[(&'a str, f64)]) -> Option<&'a str> {
    assert!((0.0..1.0).contains(&point), "{point} is outside [0, 1)");
    assert!(
        weighted.iter().all(|(_, w)| w.is_finite() && *w >= 0.0),
        "weights are finite and non-negative"
    );
    let positive: Vec<(&'a str, f64)> =
        weighted.iter().copied().filter(|(_, w)| *w > 0.0).collect();
    let total: f64 = positive.iter().map(|(_, w)| w).sum();
    let target = point * total;
    let mut reached = 0.0;
    for (entry, weight) in &positive {
        reached += weight;
        if target < reached {
            return Some(entry);
        }
    }
    // Rounding may leave `target` a hair past the last sum.
    positive.last().map(|(entry, _)| *entry)
}

impl ModelCatalog {
    /// Each provider's best allowed tier at or below `requested`.
    pub fn best_tiers(&self, requested: Tier, policy: &ModelPolicy) -> BTreeMap<&str, Tier> {
        let mut best = BTreeMap::new();
        for m in self.models() {
            let Some(tier) = m.tier.filter(|t| *t <= requested) else {
                continue;
            };
            if policy.permits_model(&m.model) {
                let held = best.entry(m.model.provider.as_str()).or_insert(tier);
                *held = (*held).max(tier);
            }
        }
        debug_assert!(best.values().all(|t| *t <= requested));
        best
    }

    /// The provider a tier-only choice of `route`'s item resolves on: the one
    /// it holds while that meets `floor`, else a weighted pick among those
    /// whose best allowed model does. `None` when no weighted provider does.
    pub fn routed_provider(
        &self,
        (requested, floor): (Tier, Tier),
        route: ProviderRoute<'_>,
        policy: &ModelPolicy,
    ) -> Option<&str> {
        assert!(
            floor <= requested,
            "a floor never exceeds the tier asked for"
        );
        let eligible: Vec<(&str, f64)> = self
            .best_tiers(requested, policy)
            .into_iter()
            .filter(|(_, best)| *best >= floor)
            .map(|(provider, _)| (provider, f64::from(route.weights.weight(provider))))
            .filter(|(_, weight)| *weight > 0.0)
            .collect();
        let held = route
            .held
            .and_then(|held| eligible.iter().find(|(p, _)| *p == held));
        if let Some((provider, _)) = held {
            return Some(provider);
        }
        let picked = weighted_pick(route_point(route.key), &eligible);
        debug_assert!(picked.is_none_or(|p| eligible.iter().any(|(e, _)| *e == p)));
        picked
    }

    /// The provider an item holds: the one it held while the policy still
    /// allows a tiered model there, else its weighted pick among all of them.
    pub fn item_provider(&self, route: ProviderRoute<'_>, policy: &ModelPolicy) -> Option<&str> {
        self.routed_provider((Tier::Frontier, Tier::Fast), route, policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_tier::CatalogModel;

    fn weights(pairs: &[(&str, u32)], default: u32) -> ProviderWeights {
        let table = pairs.iter().map(|(p, w)| ((*p).to_owned(), *w)).collect();
        ProviderWeights::new(table, default)
    }

    fn catalog(entries: &[(&str, Tier)]) -> ModelCatalog {
        let entry = |(model, tier): &(&str, Tier)| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(*tier),
            is_default: false,
        };
        ModelCatalog::new(entries.iter().map(entry).collect())
    }

    /// Deterministic, ULID-shaped item ids, so the test cannot flake.
    fn synthetic_ids(count: usize) -> Vec<String> {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        (0..count)
            .map(|n| {
                let mut state = (n as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                let body: String = (0..26)
                    .map(|_| {
                        state = state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        ALPHABET[(state >> 59) as usize] as char
                    })
                    .collect();
                format!("qi_{body}")
            })
            .collect()
    }

    fn route<'a>(weights: &'a ProviderWeights, key: &'a str) -> ProviderRoute<'a> {
        ProviderRoute {
            weights,
            key,
            held: None,
        }
    }

    #[test]
    fn weights_of_seven_to_three_split_a_thousand_items_seventy_thirty() {
        let both = catalog(&[
            ("claude-code/opus", Tier::Frontier),
            ("codex/sol", Tier::Frontier),
        ]);
        let table = weights(&[("claude-code", 7), ("codex", 3)], 1);
        let open = ModelPolicy::default();
        let ids = synthetic_ids(1000);
        let claude = ids
            .iter()
            .filter(|id| both.item_provider(route(&table, id), &open) == Some("claude-code"))
            .count();
        let share = claude as f64 / ids.len() as f64 * 100.0;
        assert!((67.0..=73.0).contains(&share), "claude-code got {share}%");
        let first = both.item_provider(route(&table, &ids[0]), &open);
        assert_eq!(
            first,
            both.item_provider(route(&table, &ids[0]), &open),
            "deterministic"
        );
    }

    #[test]
    fn a_frontier_floor_never_routes_to_a_provider_whose_best_is_strong() {
        let mixed = catalog(&[
            ("claude-code/opus", Tier::Frontier),
            ("codex/sol", Tier::Strong),
        ]);
        let table = weights(&[("claude-code", 1), ("codex", 1000)], 1);
        let open = ModelPolicy::default();
        for id in synthetic_ids(200) {
            let held = ProviderRoute {
                held: Some("codex"),
                ..route(&table, &id)
            };
            let frontier = mixed.routed_provider((Tier::Frontier, Tier::Frontier), held, &open);
            assert_eq!(frontier, Some("claude-code"), "{id}");
        }
        let lowered = mixed.routed_provider(
            (Tier::Frontier, Tier::Strong),
            ProviderRoute {
                held: Some("codex"),
                ..route(&table, "qi_any")
            },
            &open,
        );
        assert_eq!(
            lowered,
            Some("codex"),
            "a lower floor keeps the held provider"
        );
        let denied = ModelPolicy::from_lists(&[], &["claude-code/opus".parse().unwrap()], None);
        let none = mixed.routed_provider(
            (Tier::Frontier, Tier::Frontier),
            route(&table, "qi_x"),
            &denied,
        );
        assert_eq!(none, None, "nothing meets the floor within the policy");
    }

    #[test]
    fn a_provider_the_table_omits_takes_the_default_weight() {
        let three = catalog(&[
            ("claude-code/opus", Tier::Strong),
            ("codex/sol", Tier::Strong),
            ("gemini/pro", Tier::Strong),
        ]);
        let table = weights(&[("claude-code", 1), ("codex", 1)], 1);
        let open = ModelPolicy::default();
        let ids = synthetic_ids(900);
        let gemini = ids
            .iter()
            .filter(|id| three.item_provider(route(&table, id), &open) == Some("gemini"))
            .count();
        assert!((250..=350).contains(&gemini), "gemini got {gemini} of 900");
        let silent = weights(&[("claude-code", 1)], 0);
        assert!(
            ids.iter()
                .all(|id| three.item_provider(route(&silent, id), &open) == Some("claude-code")),
            "a zero default leaves the others out"
        );
    }

    #[test]
    fn a_weighted_pick_walks_the_shares_in_order() {
        let shares = [("a", 1.0), ("b", 0.0), ("c", 3.0)];
        assert_eq!(weighted_pick(0.0, &shares), Some("a"));
        assert_eq!(weighted_pick(0.24, &shares), Some("a"));
        assert_eq!(weighted_pick(0.25, &shares), Some("c"));
        assert_eq!(weighted_pick(0.999_999, &shares), Some("c"));
        assert_eq!(weighted_pick(0.5, &[("a", 0.0)]), None);
        assert_eq!(weighted_pick(0.5, &[]), None);
        let points: Vec<f64> = synthetic_ids(50).iter().map(|id| route_point(id)).collect();
        assert!(points.iter().all(|p| (0.0..1.0).contains(p)));
    }
}
