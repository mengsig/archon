//! Usage fixtures follow Aether's EventKind::AgentUsageUpdated wire shape.
use archon_aether::events::{EventKind, parse_event};
use archon_core::cost::CostBasis;

#[test]
fn real_usage_events_preserve_the_cost_basis() {
    for (fixture, basis) in [
        (
            include_str!("fixtures/usage_list_price.json"),
            CostBasis::ListPrice,
        ),
        (
            include_str!("fixtures/usage_provider_reported.json"),
            CostBasis::ProviderReported,
        ),
    ] {
        let event = parse_event(serde_json::from_str(fixture).unwrap()).unwrap();
        let EventKind::AgentUsageUpdated { usage } = event.kind else {
            panic!("usage event expected")
        };
        assert_eq!(usage.cost_basis, basis);
        assert_eq!(usage.cost_usd, 0.5);
        assert_eq!(usage.cost_complete, Some(true));
        assert_eq!(usage.input_tokens, 1200);
        assert_eq!(usage.estimated_cost_usd, None);
    }
}

#[test]
fn missing_basis_is_unknown_and_invalid_basis_is_visible() {
    let mut wire: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/usage_list_price.json")).unwrap();
    wire["kind"]["total"]
        .as_object_mut()
        .unwrap()
        .remove("cost_basis");
    let EventKind::AgentUsageUpdated { usage } = parse_event(wire.clone()).unwrap().kind else {
        panic!("usage event expected")
    };
    assert_eq!(usage.cost_basis, CostBasis::Unknown);
    wire["kind"]["total"]["cost_basis"] = serde_json::json!("billed");
    assert!(parse_event(wire).is_err());
}

/// Aether 1.0.9 books a Codex turn as an estimate beside a zero cost; until
/// then the same dollars arrived as list-price `cost_usd` with no estimate.
#[test]
fn a_codex_turn_is_an_estimate_beside_a_zero_cost() {
    let wire = include_str!("fixtures/usage_codex_estimate.json");
    let EventKind::AgentUsageUpdated { usage } = parse_event(serde_json::from_str(wire).unwrap())
        .unwrap()
        .kind
    else {
        panic!("usage event expected")
    };
    assert_eq!(usage.cost_usd, 0.0);
    assert_eq!(usage.estimated_cost_usd, Some(0.5));
    assert_eq!(usage.cost_complete, Some(false));
    assert_eq!(usage.cost_basis, CostBasis::ListPrice);
    let old = include_str!("fixtures/usage_list_price.json");
    let EventKind::AgentUsageUpdated { usage: old } =
        parse_event(serde_json::from_str(old).unwrap())
            .unwrap()
            .kind
    else {
        panic!("usage event expected")
    };
    let api_equivalent =
        |u: &archon_aether::events::UsageTotals| u.cost_usd + u.estimated_cost_usd.unwrap_or(0.0);
    assert_eq!(api_equivalent(&usage), api_equivalent(&old));
}
