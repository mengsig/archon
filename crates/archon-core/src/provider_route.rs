//! Provider weights for tier-only choices (`[models] provider_weights`):
//! each item holds one provider, picked once by weight (scaled by live quota
//! headroom) from a hash of its id, and a stage moves off it only when its
//! capability floor rules it out or its quota runs out.

use std::collections::BTreeMap;

use crate::Timestamp;
use crate::model_tier::{ModelCatalog, Tier};
use crate::policy::ModelPolicy;
use crate::quota::QuotaView;

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
/// hashes (the item id), the provider it already holds, if any, and the
/// providers' quota headroom, when Aether reports it.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRoute<'a> {
    pub weights: &'a ProviderWeights,
    pub key: &'a str,
    pub held: Option<&'a str>,
    pub quota: Option<&'a QuotaView>,
}

/// Where a quota-aware route sends a tier-only choice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Routed<'a> {
    To(&'a str),
    /// Every weighted provider that meets the floor is out of quota; the
    /// earliest of their resets, when one is known.
    Exhausted {
        until: Option<Timestamp>,
    },
    /// No weighted provider meets the floor within the policy.
    Nowhere,
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

/// The highest scored entry; the first of equals, in the order given.
fn best<'a>(scored: &[(&'a str, f64)]) -> &'a str {
    assert!(!scored.is_empty(), "a best of none");
    let mut top = scored[0];
    for entry in &scored[1..] {
        if entry.1 > top.1 {
            top = *entry;
        }
    }
    debug_assert!(top.1 > 0.0, "the best has quota left");
    top.0
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

    /// The providers with a positive weight whose best allowed model at or
    /// below `requested` reaches `floor`, each with its weight times its
    /// quota headroom (1 while unknown).
    fn scored(
        &self,
        (requested, floor): (Tier, Tier),
        route: ProviderRoute<'_>,
        policy: &ModelPolicy,
    ) -> Vec<(&str, f64)> {
        assert!(
            floor <= requested,
            "a floor never exceeds the tier asked for"
        );
        let scored: Vec<(&str, f64)> = self
            .best_tiers(requested, policy)
            .into_iter()
            .filter(|(_, best)| *best >= floor)
            .map(|(provider, _)| (provider, f64::from(route.weights.weight(provider))))
            .filter(|(_, weight)| *weight > 0.0)
            .map(|(p, weight)| (p, weight * route.quota.map_or(1.0, |q| q.factor(p))))
            .collect();
        debug_assert!(scored.iter().all(|(_, s)| s.is_finite() && *s >= 0.0));
        scored
    }

    /// Where a tier-only choice of `route`'s item goes: the provider it holds
    /// while that meets `floor` and has quota; when that one ran out, the
    /// best scored of the rest; else a pick weighted by score.
    pub fn route(
        &self,
        tiers: (Tier, Tier),
        route: ProviderRoute<'_>,
        policy: &ModelPolicy,
    ) -> Routed<'_> {
        let scored = self.scored(tiers, route, policy);
        if scored.is_empty() {
            return Routed::Nowhere;
        }
        if scored.iter().all(|(_, score)| *score <= 0.0) {
            let quota = route.quota.expect("only quota zeroes a positive weight");
            let until = quota.earliest_reset(scored.iter().map(|(p, _)| *p));
            return Routed::Exhausted { until };
        }
        let held = route
            .held
            .and_then(|held| scored.iter().find(|(p, _)| *p == held));
        let picked = match held {
            Some((provider, score)) if *score > 0.0 => *provider,
            Some(_) => best(&scored),
            None => weighted_pick(route_point(route.key), &scored).expect("a positive score"),
        };
        debug_assert!(scored.iter().any(|(p, s)| *p == picked && *s > 0.0));
        Routed::To(picked)
    }

    /// The provider a tier-only choice of `route`'s item resolves on, as
    /// [`Self::route`] has it. When every provider meeting `floor` is out of
    /// quota it routes as if quota were unknown: the dispatcher holds such an
    /// item, so a choice made anyway never falls below the floor. `None` when
    /// no weighted provider meets the floor.
    pub fn routed_provider(
        &self,
        tiers: (Tier, Tier),
        route: ProviderRoute<'_>,
        policy: &ModelPolicy,
    ) -> Option<&str> {
        match self.route(tiers, route, policy) {
            Routed::To(provider) => Some(provider),
            Routed::Nowhere => None,
            Routed::Exhausted { .. } => {
                let blind = ProviderRoute {
                    quota: None,
                    ..route
                };
                self.routed_provider(tiers, blind, policy)
            }
        }
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
    use crate::quota::{ProviderHeadroom, QuotaWindow, Reserves};

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
            quota: None,
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

    /// A reading of one 5-hour window with `used` percent spent, resetting
    /// in two hours.
    fn five_hour(used: u64, now: crate::Timestamp) -> ProviderHeadroom {
        ProviderHeadroom {
            windows: vec![QuotaWindow {
                name: "five_hour".into(),
                window_minutes: Some(300),
                remaining_percent: 100 - used,
                resets_at: Some(now + chrono::TimeDelta::hours(2)),
                resets_in_seconds: Some(7200),
            }],
            observed_at: Some(now),
            age_seconds: Some(0),
            stale: false,
        }
    }

    fn reserves(orchestrator: u8) -> Reserves {
        Reserves {
            orchestrator_provider: "claude-code".into(),
            orchestrator,
            interactive_providers: Default::default(),
            interactive: 0,
        }
    }

    fn view(used: &[(&str, u64)], orchestrator_reserve: u8) -> QuotaView {
        let now: crate::Timestamp = "2026-10-16T12:00:00Z".parse().unwrap();
        let readings: Vec<(&str, ProviderHeadroom)> =
            used.iter().map(|(p, u)| (*p, five_hour(*u, now))).collect();
        let readings = readings.iter().map(|(p, r)| (*p, r));
        QuotaView::new(readings, &reserves(orchestrator_reserve), now, now)
    }

    fn quota_route<'a>(
        weights: &'a ProviderWeights,
        key: &'a str,
        quota: &'a QuotaView,
    ) -> ProviderRoute<'a> {
        ProviderRoute {
            quota: Some(quota),
            ..route(weights, key)
        }
    }

    /// The percent of `ids` whose item provider is `provider`.
    fn share<'a>(
        catalog: &ModelCatalog,
        provider: &str,
        ids: &'a [String],
        route: impl Fn(&'a str) -> ProviderRoute<'a>,
    ) -> f64 {
        let open = ModelPolicy::default();
        let hits = ids
            .iter()
            .filter(|id| catalog.item_provider(route(id), &open) == Some(provider))
            .count();
        hits as f64 / ids.len() as f64 * 100.0
    }

    #[test]
    fn a_nearly_spent_claude_window_sends_new_items_to_codex_despite_seven_to_three() {
        let ids = synthetic_ids(1000);
        let both = catalog(&[
            ("claude-code/opus", Tier::Frontier),
            ("codex/sol", Tier::Frontier),
        ]);
        let table = weights(&[("claude-code", 7), ("codex", 3)], 1);
        let reserved = view(&[("claude-code", 95), ("codex", 20)], 15);
        let codex = share(&both, "codex", &ids, |id| {
            quota_route(&table, id, &reserved)
        });
        assert_eq!(
            codex, 100.0,
            "5% left is under the orchestrator's 15% reserve"
        );
        let unreserved = view(&[("claude-code", 95), ("codex", 20)], 0);
        let codex = share(&both, "codex", &ids, |id| {
            quota_route(&table, id, &unreserved)
        });
        assert!(
            codex > 80.0,
            "headroom outweighs 7:3 even with no reserve: {codex}%"
        );
    }

    #[test]
    fn the_orchestrators_reserve_is_never_routed_to_and_holds_when_alone() {
        let both = catalog(&[
            ("claude-code/opus", Tier::Frontier),
            ("codex/sol", Tier::Strong),
        ]);
        let table = weights(&[("claude-code", 1000), ("codex", 1)], 1);
        let quota = view(&[("claude-code", 86), ("codex", 0)], 15);
        let open = ModelPolicy::default();
        for id in synthetic_ids(200) {
            let held = ProviderRoute {
                held: Some("claude-code"),
                ..quota_route(&table, &id, &quota)
            };
            let strong = both.route((Tier::Frontier, Tier::Strong), held, &open);
            assert_eq!(strong, Routed::To("codex"), "{id}: 14% left is the reserve");
        }
        let alone = both.route(
            (Tier::Frontier, Tier::Frontier),
            quota_route(&table, "qi_x", &quota),
            &open,
        );
        let reset = quota.get("claude-code").unwrap().resets_at;
        assert!(reset.is_some());
        assert_eq!(
            alone,
            Routed::Exhausted { until: reset },
            "never below the floor"
        );
        assert_eq!(
            both.routed_provider(
                (Tier::Frontier, Tier::Frontier),
                quota_route(&table, "qi_x", &quota),
                &open
            ),
            Some("claude-code"),
            "a resolution made anyway stays at the floor"
        );
    }

    #[test]
    fn an_item_keeps_its_provider_until_that_runs_out_then_takes_the_best() {
        let three = catalog(&[
            ("claude-code/opus", Tier::Strong),
            ("codex/sol", Tier::Strong),
            ("gemini/pro", Tier::Strong),
        ]);
        let table = weights(&[("claude-code", 1), ("codex", 1)], 1);
        let open = ModelPolicy::default();
        let healthy = view(&[("claude-code", 90), ("codex", 10), ("gemini", 50)], 0);
        let held = |quota| ProviderRoute {
            held: Some("claude-code"),
            ..quota_route(&table, "qi_a", quota)
        };
        let kept = three.route((Tier::Strong, Tier::Strong), held(&healthy), &open);
        assert_eq!(kept, Routed::To("claude-code"), "low but not out keeps it");
        let spent = view(&[("claude-code", 100), ("codex", 10), ("gemini", 50)], 0);
        let moved = three.route((Tier::Strong, Tier::Strong), held(&spent), &open);
        assert_eq!(
            moved,
            Routed::To("codex"),
            "the most headroom wins the switch"
        );
    }

    #[test]
    fn a_third_provider_is_scored_from_its_report_alone_and_unknown_changes_nothing() {
        let ids = synthetic_ids(1000);
        let three = catalog(&[
            ("claude-code/opus", Tier::Strong),
            ("codex/sol", Tier::Strong),
            ("gemini/pro", Tier::Strong),
        ]);
        let table = weights(&[("claude-code", 1), ("codex", 1)], 1);
        let plain = share(&three, "gemini", &ids, |id| route(&table, id));
        let unknown = QuotaView::default();
        assert_eq!(
            plain,
            share(&three, "gemini", &ids, |id| quota_route(
                &table, id, &unknown
            ))
        );
        let fresh = view(&[("claude-code", 90), ("codex", 90), ("gemini", 0)], 0);
        let scored = share(&three, "gemini", &ids, |id| quota_route(&table, id, &fresh));
        assert!(
            scored > 80.0,
            "gemini's full window wins most items: {scored}%"
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
