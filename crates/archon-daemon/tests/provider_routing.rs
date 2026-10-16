//! Provider weights: an item holds one provider for its tier-only choices,
//! picked by weight, through dispatch and retries; a stage's capability floor
//! moves it off that provider; with no weights the orchestrator's provider
//! wins as before.

mod common;

use archon_core::config::Config;
use archon_core::model::{ModelPlan, ModelPlanDraft};
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use common::{Harness, PIPELINE, REPO_NAME};

const ROUTED: &str = "routed";
const ROUTED_TOML: &str = r#"[pipeline]
name = "routed"
description = "a strong implementer and a reviewer that may run one tier lower"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 2
gate = "all_pass"
min_tier = "strong"
timeout_minutes = 30
on_fail = { goto = "implement", max_loops = 1 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

/// Claude has every tier up to frontier; Codex stops at strong.
fn catalog() -> ModelCatalog {
    let entry = |model: &str, tier| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(tier),
        is_default: false,
    };
    ModelCatalog::new(vec![
        entry("claude-code/haiku", Tier::Fast),
        entry("claude-code/sonnet", Tier::Strong),
        entry("claude-code/opus", Tier::Frontier),
        entry("codex/mini", Tier::Fast),
        entry("codex/sol", Tier::Strong),
    ])
}

async fn harness(config: Config) -> Harness {
    let h = Harness::with_config(config).await;
    h.ctx.store.lock().await.set_model_catalog(catalog());
    h.add_pipeline(ROUTED, ROUTED_TOML);
    h
}

fn weighted(pairs: &[(&str, u32)]) -> Config {
    let mut config = Config::default();
    config.models.provider_weights = pairs.iter().map(|(p, w)| ((*p).into(), *w)).collect();
    config
}

/// Every stage asks by tier alone: the implementer for strong, one reviewer
/// for frontier (floored at strong by the pipeline) and one for fast.
fn tier_plan() -> ModelPlanDraft {
    serde_json::from_value(serde_json::json!({
        "implement": [{"tier": "strong"}],
        "review": [{"tier": "frontier"}, {"tier": "fast"}],
        "ship": [{"tier": "fast"}],
    }))
    .unwrap()
}

fn enqueue_request(pipeline: &str, title: &str) -> EnqueueRequest {
    EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlanDraft::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    }
}

async fn enqueue(h: &Harness, pipeline: &str, title: &str, plan: ModelPlanDraft) -> QueueItem {
    let request = EnqueueRequest {
        model_plan: plan,
        ..enqueue_request(pipeline, title)
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// Each plan key's providers, in choice order.
fn providers(plan: &ModelPlan) -> Vec<(String, Vec<String>)> {
    plan.0
        .iter()
        .map(|(key, choices)| {
            let on = choices.iter().map(|c| c.model.provider.clone()).collect();
            (key.clone(), on)
        })
        .collect()
}

/// `held` everywhere the floor allows it; the frontier reviewer needs Claude
/// unless the pipeline lowered its floor.
fn expected(held: &str, frontier: &str) -> Vec<(String, Vec<String>)> {
    vec![
        ("implement".into(), vec![held.into()]),
        ("review".into(), vec![frontier.into(), held.into()]),
        ("ship".into(), vec![held.into()]),
    ]
}

#[tokio::test]
async fn every_stage_dispatch_and_retry_of_an_item_keeps_its_weighted_provider() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    let item = enqueue(&h, PIPELINE, "retried", tier_plan()).await;
    let held = item.routed_provider.clone().unwrap();
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "the implementer runs");
    assert_eq!(stages[0].choice.model.provider, held, "dispatch keeps it");
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: Some(tier_plan()),
        model_reasons: None,
        fresh: false,
        brief_append: None,
    };
    h.cli(retry).await.expect("retry");
    let retried = h.item(&item.id).await;
    assert_eq!(retried.routed_provider.as_deref(), Some(held.as_str()));
    assert_eq!(
        providers(&retried.model_plan),
        expected(&held, "claude-code")
    );
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 2, "the retry's implementer runs");
    assert!(
        stages.iter().all(|s| s.choice.model.provider == held),
        "the retry keeps it"
    );
    let detail = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .expect("get");
    let Reply::ItemDetail(detail) = detail else {
        panic!("get_item replied {detail:?}");
    };
    assert_eq!(detail.item.routed_provider.as_deref(), Some(held.as_str()));

    let mut seen = std::collections::BTreeSet::new();
    for n in 0..40 {
        if seen.len() == 2 && n >= 4 {
            break;
        }
        let item = enqueue(&h, PIPELINE, &format!("item {n}"), tier_plan()).await;
        let held = item
            .routed_provider
            .clone()
            .expect("weights route the item");
        seen.insert(held.clone());
        let frontier = "claude-code";
        assert_eq!(
            providers(&item.model_plan),
            expected(&held, frontier),
            "{held}"
        );
        let lowered = enqueue(&h, ROUTED, &format!("lowered {n}"), tier_plan()).await;
        let held = lowered.routed_provider.clone().unwrap();
        assert_eq!(
            providers(&lowered.model_plan),
            expected(&held, &held),
            "a strong floor keeps the frontier reviewer on {held}"
        );
    }
    assert_eq!(seen.len(), 2, "an even split reaches both: {seen:?}");
}

#[tokio::test]
async fn without_weights_tier_only_choices_stay_on_the_orchestrator_s_provider() {
    let h = harness(Config::default()).await;
    assert_eq!(h.ctx.config.orchestrator.model.provider, "claude-code");
    let item = enqueue(&h, PIPELINE, "unweighted", tier_plan()).await;
    assert_eq!(item.routed_provider, None, "nothing is routed");
    assert_eq!(
        providers(&item.model_plan),
        expected("claude-code", "claude-code")
    );
    let implement = &item.model_plan.choices_for("implement").unwrap()[0];
    assert_eq!(implement.model.to_string(), "claude-code/sonnet");
}

#[tokio::test]
async fn a_repository_s_weights_route_its_items_and_a_pin_still_binds() {
    let h = harness(Config::default()).await;
    h.repo_config("[models]\nprovider_weights = { codex = 1 }\ndefault_provider_weight = 0\n");
    let item = enqueue(&h, ROUTED, "repo weighted", tier_plan()).await;
    assert_eq!(item.routed_provider.as_deref(), Some("codex"));
    assert_eq!(providers(&item.model_plan), expected("codex", "codex"));
    let pinned: ModelPlanDraft = serde_json::from_value(serde_json::json!({
        "implement": [{"model": "claude-code/sonnet"}],
        "review": [{"tier": "strong", "provider": "claude-code"}, {"tier": "fast"}],
        "ship": [{"tier": "fast"}],
    }))
    .unwrap();
    let item = enqueue(&h, PIPELINE, "pinned", pinned).await;
    let mut want = expected("codex", "claude-code");
    want[0].1 = vec!["claude-code".into()];
    assert_eq!(
        providers(&item.model_plan),
        want,
        "an exact model and an explicit provider win over the weights"
    );
}

#[tokio::test]
async fn a_floor_no_weighted_provider_reaches_is_refused_not_lowered() {
    let h = harness(weighted(&[("codex", 1)])).await;
    h.repo_config("[models]\ndefault_provider_weight = 0\n");
    let request = EnqueueRequest {
        model_plan: tier_plan(),
        ..enqueue_request(PIPELINE, "frontier")
    };
    let refused = h.cli(Request::Enqueue(request)).await.unwrap_err();
    let refused = refused.to_string();
    assert!(refused.contains("at or above tier frontier"), "{refused}");
    assert!(refused.contains("codex: strong (weight 1)"), "{refused}");
    assert!(
        refused.contains("claude-code: frontier (weight 0)"),
        "{refused}"
    );
    let lowered = enqueue(&h, ROUTED, "lowered", tier_plan()).await;
    assert_eq!(providers(&lowered.model_plan), expected("codex", "codex"));
}
