//! Provider weights: an item holds one provider for its tier-only choices,
//! picked by weight, through dispatch and retries; a stage's capability floor
//! moves it off that provider; with no weights the orchestrator's provider
//! wins as before. Quota headroom scales the weights: an item waits while
//! every provider is out and a running one moves off a provider that ran out.

use crate::common;

use archon_core::Timestamp;
use archon_core::config::Config;
use archon_core::model::{ModelPlan, ModelPlanDraft, ModelRef};
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
use archon_core::notify::NotificationKind;
use archon_core::plan_origin::{Authority, PlanSource, PlannedPlan};
use archon_core::protocol::{
    DaemonStatus, EnqueueRequest, Reply, RepoRef, Request, WaitingReason, WaitingReasonCode,
};
use archon_core::provider_route::ProviderRoute;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::quota::{ProviderHeadroom, QuotaWindow};
use archon_daemon::quota::Reading;
use chrono::TimeDelta;
use common::{Harness, PIPELINE, REPO_NAME, completed};

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
        CatalogModel {
            model: common::MODEL.parse().unwrap(),
            tier: None,
            is_default: false,
        },
        CatalogModel {
            model: common::REVIEW_MODEL.parse().unwrap(),
            tier: None,
            is_default: false,
        },
    ])
}

async fn harness(config: Config) -> Harness {
    let h = Harness::with_config(config).await;
    h.set_catalog(catalog()).await;
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
        intent: None,
        onto: None,
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
        intent: None,
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
        stage_timeouts: Default::default(),
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
        intent: None,
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

/// A fresh five-hour window with `remaining` percent left, resetting `in_`
/// from now.
fn window(remaining: u64, in_: TimeDelta) -> ProviderHeadroom {
    ProviderHeadroom {
        windows: vec![QuotaWindow {
            name: "5h".into(),
            window_minutes: Some(300),
            remaining_percent: remaining,
            resets_at: Some(archon_core::now() + in_),
            resets_in_seconds: None,
        }],
        observed_at: Some(archon_core::now()),
        age_seconds: Some(0),
        stale: false,
    }
}

/// Aetherd's answer, read `ago`.
fn read(h: &Harness, ago: TimeDelta, providers: Vec<(&str, ProviderHeadroom)>) {
    let providers = providers
        .into_iter()
        .map(|(p, r)| (p.to_owned(), r))
        .collect();
    h.ctx.quota.set_reading(Reading {
        read_at: archon_core::now() - ago,
        providers,
    });
}

async fn status(h: &Harness) -> DaemonStatus {
    match h.cli(Request::Status(Default::default())).await {
        Ok(Reply::Status(status)) => *status,
        other => panic!("status replied {other:?}"),
    }
}

fn waiting_reason(status: &DaemonStatus, item: &QueueItem) -> WaitingReason {
    let row = status
        .waiting_items
        .iter()
        .find(|row| row.item.id == item.id);
    let reason = row.and_then(|row| row.waiting_reason.clone());
    reason.unwrap_or_else(|| panic!("{} waits: {status:?}", item.id))
}

fn minute(at: Timestamp) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

#[tokio::test]
async fn with_every_provider_out_an_item_waits_for_the_earliest_reset_then_starts() {
    let h = harness(weighted(&[("claude-code", 7), ("codex", 3)])).await;
    let codex = window(0, TimeDelta::minutes(40));
    let codex_reset = codex.windows[0]
        .resets_at
        .expect("the fixture names its reset");
    // Claude keeps 10%, under the orchestrator's 15% reserve; Codex is spent.
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(2))),
            ("codex", codex),
        ],
    );
    let item = enqueue(&h, ROUTED, "held", tier_plan()).await;
    h.dispatch().await;
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert!(h.stages(&item.id).await.is_empty(), "nothing spawned");
    let now = status(&h).await;
    let reason = waiting_reason(&now, &item);
    assert_eq!(reason.code, WaitingReasonCode::ProviderQuota);
    assert!(reason.message.contains(&minute(codex_reset)), "{reason:?}");
    let quota = now.quota.expect("status shows the reading");
    let claude = quota
        .providers
        .iter()
        .find(|p| p.provider == "claude-code")
        .unwrap();
    assert_eq!((claude.headroom, claude.reserve_percent), (Some(0.0), 15));
    assert!(claude.next_reset.is_some());

    let other = enqueue(&h, PIPELINE, "explicit", common::plan().into()).await;
    h.dispatch().await;
    assert_eq!(h.stages(&other.id).await.len(), 1, "named models start");

    // The same reading once Codex's window has reset.
    read(
        &h,
        TimeDelta::minutes(1),
        vec![
            ("claude-code", window(10, TimeDelta::hours(2))),
            ("codex", window(0, TimeDelta::seconds(-10))),
        ],
    );
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "the implementer starts on reset");
    assert_eq!(stages[0].choice.model.provider, "codex");
    assert_eq!(
        h.item(&item.id).await.routed_provider.as_deref(),
        Some("codex")
    );
}

#[tokio::test]
async fn a_running_item_s_next_stages_move_off_a_spent_provider_and_get_item_says_why() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "moves", tier_plan()).await;
    assert_eq!(item.routed_provider.as_deref(), Some("claude-code"));
    h.dispatch().await;
    let implement = h.stages(&item.id).await;
    assert_eq!(implement[0].choice.model.provider, "claude-code");

    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(90, TimeDelta::hours(4))),
        ],
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviews: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review")
        .collect();
    assert_eq!(reviews.len(), 2, "both reviewers start");
    assert!(
        reviews.iter().all(|s| s.choice.model.provider == "codex"),
        "{reviews:?}"
    );
    let detail = match h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
    {
        Ok(Reply::ItemDetail(detail)) => detail,
        other => panic!("get_item replied {other:?}"),
    };
    assert_eq!(detail.item.routed_provider.as_deref(), Some("codex"));
    let switch = detail.item.provider_switch.expect("the switch is recorded");
    assert!(
        switch.contains("stage review moved from claude-code to codex"),
        "{switch}"
    );
    assert!(switch.contains("15% reserve"), "{switch}");
    let review = detail.item.model_plan.choices_for("review").unwrap();
    assert!(
        review.iter().all(|c| c.model.provider == "codex"),
        "{review:?}"
    );
}

#[tokio::test]
async fn a_quota_reroute_posts_each_model_it_swaps_and_never_moves_a_pin() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let plan = serde_json::from_value(serde_json::json!({
        "implement": [{"tier": "strong"}],
        "review": [{"model": "claude-code/opus"}, {"tier": "fast"}],
        "ship": [{"tier": "fast"}],
    }))
    .unwrap();
    let item = enqueue(&h, ROUTED, "announced", plan).await;
    h.dispatch().await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(90, TimeDelta::hours(4))),
        ],
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviews: Vec<String> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review")
        .map(|s| s.choice.model.to_string())
        .collect();
    assert_eq!(
        reviews,
        ["claude-code/opus", "codex/mini"],
        "the pin stays; the tier moves"
    );
    let notices = h
        .notices(archon_core::notify::NotificationKind::Progress)
        .await;
    let swapped: Vec<&String> = notices
        .iter()
        .filter(|n| n.contains("changed models"))
        .collect();
    assert_eq!(swapped.len(), 1, "one notice for the reroute: {notices:?}");
    let notice = swapped[0];
    assert!(notice.contains(item.id.as_str()), "{notice}");
    assert!(
        notice.contains("stage review now runs on codex/mini instead of claude-code/haiku"),
        "{notice}"
    );
    assert!(
        notice.contains("stage ship now runs on codex/mini instead of claude-code/haiku"),
        "the stored later stage too: {notice}"
    );
    assert!(notice.contains("has no quota"), "it says why: {notice}");
    assert!(
        !notice.contains("claude-code/opus"),
        "the pin did not move: {notice}"
    );
    let rerouted = h.item(&item.id).await;
    let origins = crate::plan_origins::shown(&h, &rerouted).await;
    let (pin, moved) = (&origins["review"][0].model, &origins["ship"][0].model);
    assert_eq!(
        (pin.source, pin.by),
        (PlanSource::Explicit, Authority::Operator)
    );
    assert_eq!(
        (moved.source, moved.by),
        (PlanSource::Tier, Authority::Daemon)
    );
    let note = moved.note.as_deref().unwrap_or_default();
    assert!(note.starts_with("moved to follow provider codex"), "{note}");
    assert_eq!(crate::plan_origins::unattributed(&h).await, 0);
    let switch = rerouted.provider_switch.expect("noted");
    for swap in [
        "stage review now runs on codex/mini instead of claude-code/haiku",
        "stage ship now runs on codex/mini instead of claude-code/haiku",
    ] {
        assert!(
            switch.contains(swap),
            "the item's record keeps it: {switch}"
        );
    }
}

/// A stored choice that no longer resolves keeps its model alone: the stages
/// a reroute moved are stored as they run, so the plan shows what runs.
#[tokio::test]
async fn a_choice_that_no_longer_resolves_keeps_the_rest_of_a_reroute_stored() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "half resolvable", tier_plan()).await;
    h.dispatch().await;
    {
        let mut store = h.ctx.store.lock().await;
        // Edited as stored, so the daemon's origins of the other stages stay.
        let mut raw = serde_json::to_value(store.item(&item.id).unwrap()).unwrap();
        let tier = &mut raw["model_plan"]["ship"][0]["tier"];
        assert!(tier.is_object(), "a tier");
        tier["provider"] = "gone".into();
        let stored: QueueItem = serde_json::from_value(raw).unwrap();
        store.update_item(&stored).unwrap();
    }
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(90, TimeDelta::hours(4))),
        ],
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviews: Vec<String> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review")
        .map(|s| s.choice.model.to_string())
        .collect();
    assert_eq!(reviews[1], "codex/mini", "the fast reviewer moved");
    let stored = h.item(&item.id).await.model_plan;
    let review: Vec<String> = (stored.0["review"].iter())
        .map(|c| c.model.to_string())
        .collect();
    assert_eq!(review, reviews, "the stored plan is the models that run");
    assert_eq!(
        stored.0["ship"][0].model.to_string(),
        "claude-code/haiku",
        "the choice that does not resolve keeps its model"
    );
}

#[tokio::test]
async fn a_queued_item_that_dispatch_moves_off_a_spent_provider_posts_each_swap() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "moved before it ran", tier_plan()).await;
    assert_eq!(item.routed_provider.as_deref(), Some("claude-code"));
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(90, TimeDelta::hours(4))),
        ],
    );
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages[0].choice.model.to_string(), "codex/sol", "it moved");
    let notices = h
        .notices(archon_core::notify::NotificationKind::Progress)
        .await;
    let swapped: Vec<&String> = notices
        .iter()
        .filter(|n| n.contains("changed models"))
        .collect();
    assert_eq!(swapped.len(), 1, "one notice for the move: {notices:?}");
    let notice = swapped[0];
    assert!(
        notice.contains("stage implement now runs on codex/sol instead of claude-code/sonnet"),
        "{notice}"
    );
    assert!(
        notice.contains("stage ship now runs on codex/mini instead of claude-code/haiku"),
        "{notice}"
    );
    let switch = h.item(&item.id).await.provider_switch.expect("noted");
    assert!(
        switch.contains("moved from claude-code to codex"),
        "{switch}"
    );
}

#[tokio::test]
async fn an_aetherd_without_headroom_or_with_a_stale_one_routes_by_weight_alone() {
    let config = weighted(&[("claude-code", 7), ("codex", 3)]);
    let weights = config.models.provider_weights(&Default::default()).unwrap();
    let h = harness(config).await;
    let (catalog, policy) = (catalog(), Default::default());
    let mut stale = window(0, TimeDelta::hours(1));
    stale.stale = true;
    for reading in [vec![], vec![("codex", stale)]] {
        read(&h, TimeDelta::zero(), reading);
        for n in 0..12 {
            let item = enqueue(&h, ROUTED, &format!("unknown {n}"), tier_plan()).await;
            let route = ProviderRoute {
                weights: &weights,
                key: item.id.as_str(),
                held: None,
                quota: None,
            };
            let by_weight = catalog.item_provider(route, &policy);
            assert_eq!(
                item.routed_provider.as_deref(),
                by_weight,
                "as without quota"
            );
        }
        let now = status(&h).await;
        assert!(
            now.waiting_items.iter().all(|row| row
                .waiting_reason
                .as_ref()
                .is_none_or(|r| r.code != WaitingReasonCode::ProviderQuota)),
            "nothing waits for quota"
        );
    }
}

#[tokio::test]
async fn a_running_item_s_next_stages_wait_while_every_provider_is_out_then_start() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "waits", tier_plan()).await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await.len(), 1, "the implementer runs");
    let codex = window(0, TimeDelta::hours(1));
    let codex_reset = codex.windows[0]
        .resets_at
        .expect("the fixture names its reset");
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", codex),
        ],
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(
        h.stages(&item.id).await.len(),
        1,
        "no reviewer spends a reserve"
    );
    let note = h
        .item(&item.id)
        .await
        .provider_switch
        .expect("the wait is recorded");
    assert!(
        note.contains("stage review waits for provider quota"),
        "{note}"
    );
    assert!(note.contains(&minute(codex_reset)), "{note}");

    read(
        &h,
        TimeDelta::minutes(1),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(0, TimeDelta::seconds(-10))),
        ],
    );
    h.dispatch().await;
    let reviews: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review")
        .collect();
    assert_eq!(reviews.len(), 2, "both reviewers start on reset");
    assert!(reviews.iter().all(|s| s.choice.model.provider == "codex"));
}

#[tokio::test]
async fn a_switch_moves_the_item_s_later_stages_too() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "follows", tier_plan()).await;
    h.dispatch().await;
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(90, TimeDelta::hours(4))),
        ],
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let stored = h.item(&item.id).await;
    assert_eq!(stored.routed_provider.as_deref(), Some("codex"));
    assert_eq!(
        providers(&stored.model_plan),
        expected("codex", "codex"),
        "every tier-only choice follows the item"
    );
}

#[tokio::test]
async fn a_countdown_only_reset_shows_in_status() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    let mut countdown = window(0, TimeDelta::minutes(40));
    countdown.windows[0].resets_at = None;
    countdown.windows[0].resets_in_seconds = Some(2400);
    read(&h, TimeDelta::zero(), vec![("codex", countdown)]);
    let quota = status(&h).await.quota.expect("status shows the reading");
    let reset = quota.providers[0]
        .next_reset
        .expect("the countdown names it");
    let expected = quota.as_of + TimeDelta::seconds(2400);
    assert_eq!(reset, expected);
}

#[tokio::test]
async fn a_repository_s_own_weights_hold_its_items_when_every_provider_is_out() {
    let h = harness(Config::default()).await;
    h.repo_config("[models]\nprovider_weights = { codex = 1, \"claude-code\" = 1 }\n");
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(0, TimeDelta::hours(2))),
            ("codex", window(0, TimeDelta::hours(1))),
        ],
    );
    let item = enqueue(&h, ROUTED, "repo held", tier_plan()).await;
    h.dispatch().await;
    assert!(h.stages(&item.id).await.is_empty(), "nothing spawned");
    let reason = waiting_reason(&status(&h).await, &item);
    assert_eq!(reason.code, WaitingReasonCode::ProviderQuota);
}

/// Stores `item`'s plan as a store without origins of its own reads it: every
/// field a pin of `source`. Answers the plan it pinned.
async fn pinned(h: &Harness, item: &QueueItem, source: PlanSource) -> ModelPlan {
    let mut store = h.ctx.store.lock().await;
    let mut stored = store.item(&item.id).unwrap();
    let plan = stored.model_plan.plan().clone();
    assert!(plan.0.values().flatten().all(|c| c.tier.is_some()));
    stored.model_plan = match source {
        PlanSource::LegacyUnknown => PlannedPlan::legacy(plan.clone()),
        PlanSource::Unattributed => PlannedPlan::unattributed(&stored.id, plan.clone()),
        other => panic!("no store read makes a {other:?} pin"),
    };
    store.update_item(&stored).unwrap();
    plan
}

const PINS: [PlanSource; 2] = [PlanSource::LegacyUnknown, PlanSource::Unattributed];

/// Dispatch resolves a queued pinned tier, so admission holds it for quota
/// like a daemon tier rather than letting it start on an exhausted provider.
#[tokio::test]
async fn a_queued_item_with_pinned_tiers_waits_for_quota_like_any_tier() {
    for source in PINS {
        let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
        read(
            &h,
            TimeDelta::zero(),
            vec![
                ("claude-code", window(0, TimeDelta::hours(2))),
                ("codex", window(0, TimeDelta::hours(1))),
            ],
        );
        let item = enqueue(&h, ROUTED, "queued with pins", tier_plan()).await;
        pinned(&h, &item, source).await;
        h.dispatch().await;
        assert!(h.stages(&item.id).await.is_empty(), "{source:?}: spawned");
        let reason = waiting_reason(&status(&h).await, &item);
        assert_eq!(reason.code, WaitingReasonCode::ProviderQuota, "{source:?}");
    }
}

/// A queued pinned tier whose saved resolution left the catalog is no invalid
/// pin: the tier was asked for, and dispatch resolves it on a listed model.
#[tokio::test]
async fn a_queued_pinned_tier_whose_model_left_the_catalog_resolves_again() {
    for source in PINS {
        let h = harness(Config::default()).await;
        let item = enqueue(&h, ROUTED, "queued with a stale tier", tier_plan()).await;
        let plan = pinned(&h, &item, source).await;
        let gone = plan.choices_for("implement").unwrap()[0].model.clone();
        let next: archon_core::model::ModelRef =
            format!("{}/next-strong", gone.provider).parse().unwrap();
        let mut listed: Vec<CatalogModel> = catalog().models().to_vec();
        listed.retain(|entry| entry.model != gone);
        listed.push(CatalogModel {
            model: next.clone(),
            tier: Some(Tier::Strong),
            is_default: false,
        });
        h.set_catalog(ModelCatalog::new(listed)).await;
        h.dispatch().await;

        let state = h.state(&item.id).await;
        assert!(
            matches!(state, QueueState::Running { .. }),
            "{source:?}: {state:?}"
        );
        let running = h.running().await;
        assert_eq!(running.len(), 1, "{source:?}: the implementer runs");
        assert_eq!(running[0].choice.model, next, "{source:?}");
        let tier = running[0].choice.tier.as_ref().expect("still a tier");
        assert_eq!(
            tier.requested,
            Tier::Strong,
            "{source:?}: the tier asked for"
        );
    }
}

/// An item queued on Codex whose pinned strong tier lost its last Codex
/// model, while Claude Code still lists one and both have quota.
async fn pinned_tier_left_codex(h: &Harness, source: PlanSource) -> QueueItem {
    claude_then_codex(h, 0, 80);
    let item = enqueue(h, ROUTED, "stale tier, other provider", tier_plan()).await;
    let plan = pinned(h, &item, source).await;
    let gone = plan.choices_for("implement").unwrap()[0].model.clone();
    assert_eq!(gone.to_string(), "codex/sol", "{source:?}: queued on Codex");
    let mut listed: Vec<CatalogModel> = catalog().models().to_vec();
    listed.retain(|entry| entry.model != gone);
    assert_eq!(listed.len() + 1, catalog().models().len(), "one model left");
    h.set_catalog(ModelCatalog::new(listed)).await;
    claude_then_codex(h, 80, 80);
    item
}

fn codex_first() -> Config {
    let mut config = weighted(&[("claude-code", 1), ("codex", 1)]);
    config.orchestrator.model = "codex/mini".parse().unwrap();
    config
}

/// A queued pinned tier whose provider lists no model of that tier any more
/// resolves on the other weighted provider, as dispatch routes it.
#[tokio::test]
async fn a_queued_pinned_tier_resolves_again_on_another_weighted_provider() {
    for source in PINS {
        let h = harness(codex_first()).await;
        let item = pinned_tier_left_codex(&h, source).await;
        h.dispatch().await;

        let state = h.state(&item.id).await;
        assert!(
            matches!(state, QueueState::Running { .. }),
            "{source:?}: {state:?}"
        );
        let implementers = spawned_on(&h, &item, "implement").await;
        assert_eq!(implementers, ["claude-code/sonnet"], "{source:?}");
    }
}

/// The one "changed models" notice of `item`, which its `provider_switch`
/// note repeats after the minute it was written.
async fn changed_models(h: &Harness, item: &QueueItem) -> String {
    let notices = h
        .notices(archon_core::notify::NotificationKind::Progress)
        .await;
    let swapped: Vec<&String> = notices
        .iter()
        .filter(|n| n.contains("changed models"))
        .collect();
    assert_eq!(swapped.len(), 1, "one notice: {notices:?}");
    let (_, told) = swapped[0].split_once("changed models: ").unwrap();
    let switch = h.item(&item.id).await.provider_switch;
    let switch = switch.expect("the move is noted on the item");
    assert!(switch.ends_with(told), "{switch} repeats {told}");
    told.to_owned()
}

/// The move is told as any other at dispatch, though the catalog repair of
/// the claimed item stored the new model first, and for the catalog's reason.
#[tokio::test]
async fn a_tier_the_catalog_repair_moved_is_still_noted_at_dispatch() {
    for source in PINS {
        let h = harness(codex_first()).await;
        let item = pinned_tier_left_codex(&h, source).await;
        h.dispatch().await;

        let told = changed_models(&h, &item).await;
        let moved = "stage implement moved from codex/sol to claude-code/sonnet: \
            the model catalog no longer lists codex/sol; \
            stage implement now runs on claude-code/sonnet instead of codex/sol";
        assert!(told.starts_with(moved), "{source:?}: {told}");
        assert!(!told.contains("tier resolves"), "{source:?}: {told}");
        assert!(!told.contains("item's provider"), "{source:?}: {told}");
    }
}

/// How [`list_beside`] lists `old` beside its new model `next`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Beside {
    /// `next` is the default of `old`'s tier.
    Default,
    /// `old` is still listed, superseded by `next`.
    Superseded,
    /// `old` is retired with `next` its successor, over a connection that
    /// reads Aether's retired list.
    Retired,
}

/// `catalog()` and a new model `next` of `old`'s tier, as Aether lists them
/// and archond holds them, `old` listed beside it as `beside` says.
async fn list_beside(h: &Harness, (old, next): (&str, &str), beside: Beside) {
    use serde_json::json;
    let old: archon_core::model::ModelRef = old.parse().unwrap();
    let next: archon_core::model::ModelRef = next.parse().unwrap();
    assert_eq!(old.provider, next.provider, "a successor of its provider");
    let mut held = h.ctx.store.lock().await.model_catalog();
    let mut listed = held.models().to_vec();
    let tier = listed.iter().find(|e| e.model == old).expect("listed").tier;
    listed.push(CatalogModel {
        model: next.clone(),
        tier,
        is_default: beside == Beside::Default,
    });
    if beside == Beside::Retired {
        listed.retain(|e| e.model != old);
    }
    let superseded = beside == Beside::Superseded;
    let rows: Vec<_> = (listed.iter())
        .map(|e| {
            let successor = held.superseded_by(&e.model).map(|m| &m.model);
            let successor = successor.or((superseded && e.model == old).then_some(&next.model));
            json!({
                "model": e.model, "configured": true, "tier": e.tier,
                "is_default": e.is_default, "superseded_by": successor,
            })
        })
        .collect();
    let reply = archon_aether::fake::Scripted::reply("models", json!(rows));
    h.catalog_server.set_default("list_models", reply);
    let was: Vec<_> = (held.models().iter())
        .filter(|e| e.model != old || superseded)
        .filter_map(|e| Some((e.model.clone(), held.superseded_by(&e.model)?.clone())))
        .collect();
    held = ModelCatalog::new(listed);
    for (model, successor) in was {
        held.add_superseded(model, successor);
    }
    match beside {
        Beside::Default => {}
        Beside::Superseded => held.add_superseded(old, next),
        Beside::Retired => {
            retire_on_server(h, (&old, &next)).await;
            held.add_retired(old, Some(next));
        }
    }
    h.ctx.store.lock().await.set_model_catalog(held);
}

/// Have the catalog server report `old` retired for `next`, and connect to it
/// again so the daemon reads its retired list as well.
async fn retire_on_server(h: &Harness, (old, next): (&ModelRef, &ModelRef)) {
    let retired = serde_json::json!([{"model": old, "successor": next.model}]);
    h.catalog_server.set_features(vec!["retired_models".into()]);
    let reply = archon_aether::fake::Scripted::reply("retired_models", retired);
    h.catalog_server.set_default("list_retired_models", reply);
    let client = archon_aether::AetherClient::connect(&h.catalog_server.endpoint())
        .await
        .expect("catalog connection");
    assert!(
        client
            .welcome()
            .features
            .iter()
            .any(|f| f == "retired_models")
    );
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(client)))
        .unwrap();
}

/// No weights and no exploring: a stage a plan leaves out is filled with the
/// orchestrator's model, which `catalog()` lists without a tier.
fn untiered_fallback() -> Config {
    let mut config = Config::default();
    config.orchestrator.model = common::MODEL.parse().unwrap();
    config.learning.explore_rate = 0.0;
    config
}

/// A queued item whose `fallback` stage the plan left to the daemon, which
/// filled it with the orchestrator's untiered model; answers that model.
async fn with_untiered_fallback(h: &Harness, fallback: &str) -> (QueueItem, String) {
    let mut plan = serde_json::to_value(tier_plan()).unwrap();
    plan.as_object_mut().unwrap().remove(fallback).unwrap();
    let plan = serde_json::from_value(plan).unwrap();
    let item = enqueue(h, ROUTED, "a stage left to the daemon", plan).await;
    let filled = &item.model_plan.0[fallback][0];
    assert!(filled.tier.is_none(), "an untiered fallback: {filled:?}");
    let shown = crate::plan_origins::shown(h, &item).await;
    let origin = &shown[fallback][0].model;
    let filled_by = (origin.source, origin.by);
    assert_eq!(filled_by, (PlanSource::Fallback, Authority::Daemon));
    assert_eq!(filled.model.to_string(), common::MODEL);
    (item.clone(), filled.model.to_string())
}

/// An untiered fallback the catalog retired moves for that reason: the
/// note never says a tier resolved, as the stage has none.
#[tokio::test]
async fn a_retired_fallback_the_repair_moved_is_noted_as_the_catalog_s_move() {
    let h = harness(untiered_fallback()).await;
    let (item, old) = with_untiered_fallback(&h, "implement").await;
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Retired).await;
    h.dispatch().await;

    assert_eq!(spawned_on(&h, &item, "implement").await, [next.as_str()]);
    let told = changed_models(&h, &item).await;
    let moved = format!(
        "stage implement moved from {old} to {next}: {old} is retired; \
         stage implement now runs on {next} instead of {old}"
    );
    assert_eq!(told, moved, "the only move, for the catalog's reason");
}

/// A daemon pick and a pin the catalog supersedes but still lists keep their
/// models (I3): no repair rewrites them, and the stage runs as planned.
#[tokio::test]
async fn a_listed_superseded_pick_and_pin_are_kept() {
    let h = harness(untiered_fallback()).await;
    let pins = serde_json::json!({
        "review": [{"model": "claude-code/opus"}, {"model": "claude-code/sonnet"}],
        "ship": [{"model": "claude-code/haiku"}],
    });
    let item = enqueue(
        &h,
        ROUTED,
        "superseded",
        serde_json::from_value(pins).unwrap(),
    )
    .await;
    let old = item.model_plan.0["implement"][0].model.to_string();
    assert_ne!(old, "claude-code/opus", "the pick is not the pin");
    list_beside(&h, (&old, &format!("{old}-next")), Beside::Superseded).await;
    list_beside(
        &h,
        ("claude-code/opus", "claude-code/opus-next"),
        Beside::Superseded,
    )
    .await;
    h.dispatch().await;

    assert_eq!(spawned_on(&h, &item, "implement").await, [old.as_str()]);
    let stored = h.item(&item.id).await.model_plan;
    assert_eq!(
        stored.0["implement"][0].model.to_string(),
        old,
        "the pick is kept"
    );
    assert_eq!(
        stored.0["review"][0].model.to_string(),
        "claude-code/opus",
        "the pin is kept"
    );
    let notices = h.notices(NotificationKind::Progress).await;
    assert!(
        !notices.iter().any(|n| n.contains("changed models")),
        "{notices:?}"
    );
}

/// A reserved create of the daemon's own pick, superseded before it is sent,
/// is sent again on that model: a listed superseded model is kept (I3).
#[tokio::test]
async fn a_daemon_pick_superseded_before_its_reserved_create_is_sent_is_kept() {
    let h = harness(untiered_fallback()).await;
    let busy = archon_core::error::ArchonError::Unavailable("aetherd busy".into());
    h.backend.fail_next(busy);
    let (item, old) = with_untiered_fallback(&h, "implement").await;
    h.dispatch().await;
    let sent = |h: &Harness| -> Vec<String> {
        let requests = h.backend.requests();
        requests
            .iter()
            .map(|r| r.choice.model.to_string())
            .collect()
    };
    assert_eq!(sent(&h), [old.as_str()], "the create that failed");
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Superseded).await;
    h.dispatch().await;
    h.dispatch().await;

    assert_eq!(
        sent(&h),
        [old.as_str(), old.as_str()],
        "sent again on {old}"
    );
    let running = h.running().await;
    let models: Vec<_> = running.iter().map(|s| s.choice.model.to_string()).collect();
    assert_eq!(models, [old.as_str()], "{:?}", h.state(&item.id).await);
}

/// A plan without any tier is repaired and noted too, though dispatch has no
/// tier of it to resolve.
#[tokio::test]
async fn a_repair_of_a_plan_without_tiers_is_noted_at_dispatch() {
    let h = harness(untiered_fallback()).await;
    let pins = serde_json::json!({
        "review": [{"model": "claude-code/opus"}, {"model": "claude-code/sonnet"}],
        "ship": [{"model": "claude-code/haiku"}],
    });
    let pins = serde_json::from_value(pins).unwrap();
    let item = enqueue(&h, ROUTED, "no tier at all", pins).await;
    let choices: Vec<_> = item.model_plan.0.values().flatten().collect();
    assert!(choices.iter().all(|c| c.tier.is_none()), "{choices:?}");
    let old = item.model_plan.0["implement"][0].model.to_string();
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Retired).await;
    h.dispatch().await;

    assert_eq!(spawned_on(&h, &item, "implement").await, [next.as_str()]);
    let told = changed_models(&h, &item).await;
    let moved = format!(
        "stage implement moved from {old} to {next}: {old} is retired; \
         stage implement now runs on {next} instead of {old}"
    );
    assert_eq!(told, moved, "the only move, for the catalog's reason");
}

/// A tier that resolves to another model at dispatch, with no catalog repair,
/// is told as a tier's move, as it was before the catalog was checked.
#[tokio::test]
async fn a_tier_dispatch_moved_with_no_repair_keeps_the_tier_s_reason() {
    let h = harness(Config::default()).await;
    let item = enqueue(&h, ROUTED, "a tier that moves", tier_plan()).await;
    let old = item.model_plan.0["implement"][0].model.to_string();
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Default).await;
    h.dispatch().await;

    assert_eq!(spawned_on(&h, &item, "implement").await, [next.as_str()]);
    let told = changed_models(&h, &item).await;
    let moved = format!(
        "stage implement moved from {old} to {next}: its tier resolves to {next} now; \
         stage implement now runs on {next} instead of {old}"
    );
    assert_eq!(told, moved, "the only move, for the tier's reason");
}

/// One dispatch that moves a tier and repairs an untiered fallback tells the
/// catalog's move first, for its own reason, and gives the tier's move none
/// of the catalog's.
#[tokio::test]
async fn a_catalog_repair_beside_a_tier_move_is_told_first_for_its_own_reason() {
    let h = harness(untiered_fallback()).await;
    let (item, fallback) = with_untiered_fallback(&h, "ship").await;
    let old = item.model_plan.0["implement"][0].model.to_string();
    let (next, successor) = (format!("{old}-next"), format!("{fallback}-next"));
    list_beside(&h, (&old, &next), Beside::Default).await;
    list_beside(&h, (&fallback, &successor), Beside::Retired).await;
    h.dispatch().await;

    assert_eq!(spawned_on(&h, &item, "implement").await, [next.as_str()]);
    let told = changed_models(&h, &item).await;
    let moved = format!(
        "stage ship moved from {fallback} to {successor}: \
         {fallback} is retired; \
         stage ship now runs on {successor} instead of {fallback}; \
         stage implement now runs on {next} instead of {old}"
    );
    assert_eq!(
        told, moved,
        "the catalog's move first, the tier's unexplained"
    );
    let stored = h.item(&item.id).await.model_plan;
    assert_eq!(stored.0["ship"][0].model.to_string(), successor);
}

/// A run's pending pinned tier whose provider lost its last model at the
/// floor resolves on the other weighted provider, as dispatch routes it.
#[tokio::test]
async fn a_pending_pinned_tier_resolves_again_on_another_weighted_provider() {
    for source in PINS {
        let h = harness(codex_first()).await;
        claude_then_codex(&h, 0, 80);
        let item = enqueue(&h, ROUTED, "stale tier in a run", tier_plan()).await;
        let plan = pinned(&h, &item, source).await;
        let gone = plan.choices_for("review").unwrap()[0].model.clone();
        assert_eq!(gone.to_string(), "codex/sol", "{source:?}: on Codex");
        h.dispatch().await;
        let mut listed: Vec<CatalogModel> = catalog().models().to_vec();
        listed.retain(|entry| entry.model != gone);
        h.set_catalog(ModelCatalog::new(listed)).await;
        claude_then_codex(&h, 80, 80);
        h.settle_running(|_| completed("implemented")).await;
        h.dispatch().await;

        let mut reviewers = spawned_on(&h, &item, "review").await;
        reviewers.sort();
        assert_eq!(reviewers, ["claude-code/opus", "codex/mini"], "{source:?}");
        let told = changed_models(&h, &item).await;
        let moved = "stage review moved from codex/sol to claude-code/opus: \
            the model catalog no longer lists codex/sol";
        assert!(told.starts_with(moved), "{source:?}: {told}");
    }
}

/// Once a run's catalog repair was noted, a fan-out the reroute moves in part
/// and follows onto a new default runs every slot on its stored model.
#[tokio::test]
async fn a_fan_out_the_reroute_follows_after_a_noted_repair_runs_its_stored_models() {
    let h = harness(codex_first()).await;
    claude_then_codex(&h, 0, 80);
    let item = enqueue(&h, ROUTED, "noted, then followed", tier_plan()).await;
    h.dispatch().await;
    let mut listed: Vec<CatalogModel> = catalog().models().to_vec();
    listed.retain(|entry| entry.model.to_string() != "codex/sol");
    h.set_catalog(ModelCatalog::new(listed.clone())).await;
    claude_then_codex(&h, 80, 80);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert!(h.item(&item.id).await.provider_switch.is_some(), "noted");
    let entry = |model: &str, tier, is_default| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(tier),
        is_default,
    };
    listed.push(entry("codex/sol-next", Tier::Strong, false));
    listed.push(entry("codex/mini-next", Tier::Fast, true));
    h.set_catalog(ModelCatalog::new(listed)).await;
    h.settle_running(|_| common::verdict(false)).await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented again")).await;
    h.dispatch().await;

    let stages = h.stages(&item.id).await;
    let reviews: Vec<_> = stages.iter().filter(|s| s.stage_id == "review").collect();
    assert_eq!(reviews.len(), 4, "two rounds of two reviewers");
    let stored = h.item(&item.id).await.model_plan;
    let planned = stored.choices_for("review").unwrap();
    for review in &reviews[2..] {
        let slot = review.fan_index as usize % planned.len();
        let model = &planned[slot].model;
        assert_eq!(
            &review.choice.model, model,
            "slot {slot} runs its stored model"
        );
    }
    let fast = planned[1].model.to_string();
    assert_eq!(fast, "codex/mini-next", "followed onto the new default");
}

/// A retry whose pipeline gained a stage since its last run still notes the
/// move its kept plan's catalog repair made, though the plan grew a key.
#[tokio::test]
async fn a_retry_after_its_pipeline_gained_a_stage_notes_the_catalog_move() {
    let h = harness(codex_first()).await;
    let item = pinned_tier_left_codex(&h, PlanSource::Unattributed).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let polish = "[[stage]]\nid = \"polish\"\nkind = \"work\"\nprompt = \"implement.md\"\n\
        timeout_minutes = 30\n\n[[stage]]\nid = \"ship\"";
    let grown = ROUTED_TOML.replacen("[[stage]]\nid = \"ship\"", polish, 1);
    assert_ne!(grown, ROUTED_TOML, "a stage is added");
    h.add_pipeline(ROUTED, &grown);
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");

    let retried = h.item(&item.id).await;
    assert!(retried.model_plan.choices_for("polish").is_some(), "filled");
    let told = changed_models(&h, &item).await;
    let moved = "stage implement moved from codex/sol to claude-code/sonnet: \
        the model catalog no longer lists codex/sol";
    assert!(told.starts_with(moved), "{told}");
}

/// A retry keeping such a plan is accepted on the other weighted provider.
#[tokio::test]
async fn a_retry_keeps_a_pinned_tier_that_resolves_on_another_weighted_provider() {
    for source in PINS {
        let h = harness(codex_first()).await;
        let item = pinned_tier_left_codex(&h, source).await;
        let cancel = Request::CancelItem {
            item: item.id.clone(),
            reason: None,
        };
        h.cli(cancel).await.expect("cancel");
        let retry = Request::RetryItem {
            item: item.id.clone(),
            model_plan: None,
            model_reasons: None,
            fresh: false,
            brief_append: None,
            stage_timeouts: Default::default(),
        };
        h.cli(retry)
            .await
            .unwrap_or_else(|e| panic!("{source:?}: {e}"));

        let retried = h.item(&item.id).await;
        assert_eq!(retried.state.kind(), "queued", "{source:?}");
        let implement = &retried.model_plan.choices_for("implement").unwrap()[0];
        assert_eq!(
            implement.model.to_string(),
            "claude-code/sonnet",
            "{source:?}"
        );
        h.dispatch().await;
        let told = changed_models(&h, &item).await;
        let moved = "stage implement moved from codex/sol to claude-code/sonnet: \
            the model catalog no longer lists codex/sol";
        assert!(told.starts_with(moved), "{source:?}: {told}");
        let implementers = spawned_on(&h, &item, "implement").await;
        assert_eq!(implementers, ["claude-code/sonnet"], "{source:?}");
    }
}

#[tokio::test]
async fn a_stage_waiting_for_quota_takes_no_agent_slot_from_other_work() {
    let mut config = weighted(&[("claude-code", 1), ("codex", 1)]);
    config.limits.max_parallel_agents = 1;
    let h = harness(config).await;
    let frontier = ROUTED_TOML.replace("min_tier = \"strong\"", "min_tier = \"frontier\"");
    h.add_pipeline(ROUTED, &frontier);
    let fresh = |claude| {
        vec![
            ("claude-code", window(claude, TimeDelta::hours(4))),
            ("codex", window(80, TimeDelta::hours(4))),
        ]
    };
    read(&h, TimeDelta::zero(), fresh(80));
    let waiting = enqueue(&h, ROUTED, "waits", tier_plan()).await;
    h.dispatch().await;
    assert_eq!(h.stages(&waiting.id).await.len(), 1, "the implementer runs");
    read(&h, TimeDelta::zero(), fresh(0));
    h.settle_running(|_| completed("implemented")).await;
    let pins = serde_json::from_value(serde_json::json!({
        "implement": [{"model": "codex/sol"}],
        "review": [{"model": "codex/sol"}, {"model": "codex/sol"}],
        "ship": [{"model": "codex/sol"}],
    }))
    .unwrap();
    let pinned = enqueue(&h, ROUTED, "pinned", pins).await;
    h.dispatch().await;
    assert_eq!(h.stages(&waiting.id).await.len(), 1, "the review waits");
    assert_eq!(
        h.stages(&pinned.id).await.len(),
        1,
        "the free slot goes to work that can run"
    );
}

/// Steer `stage` of `item` onto `model`.
async fn steer_model(h: &Harness, item: &QueueItem, stage: &str, model: &str) {
    let steer = archon_core::steer::Steer {
        stage: Some(stage.into()),
        model: Some(model.parse().unwrap()),
        ..Default::default()
    };
    let request = Request::SteerItem {
        item: item.id.clone(),
        steer,
    };
    h.cli(request).await.expect("the steer is taken");
}

/// The model of each stage `stage` of `item` ran, in start order.
async fn ran_on(h: &Harness, item: &QueueItem, stage: &str) -> Vec<String> {
    (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == stage)
        .inspect(|s| assert!(s.choice.tier.is_none(), "a steered pin: {:?}", s.choice))
        .map(|s| s.choice.model.to_string())
        .collect()
}

#[tokio::test]
async fn a_steered_model_runs_on_every_fan_out_slot_loop_and_retry_off_quota() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    let spent_codex = || {
        vec![
            ("claude-code", window(80, TimeDelta::hours(4))),
            ("codex", window(0, TimeDelta::hours(1))),
        ]
    };
    read(&h, TimeDelta::zero(), spent_codex());
    let paused = archon_core::protocol::PauseTarget::Global { hard: false };
    h.cli(Request::Pause(paused.clone())).await.expect("pause");
    let item = enqueue(&h, ROUTED, "steered", tier_plan()).await;
    assert_eq!(item.routed_provider.as_deref(), Some("claude-code"));
    steer_model(&h, &item, "implement", "codex/sol").await;
    steer_model(&h, &item, "review", "codex/sol").await;
    h.cli(Request::Resume(paused)).await.expect("resume");
    h.dispatch().await;
    assert_eq!(ran_on(&h, &item, "implement").await, ["codex/sol"]);

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(
        ran_on(&h, &item, "review").await,
        ["codex/sol", "codex/sol"],
        "every fan-out slot runs the steered model"
    );
    h.settle_running(|_| common::verdict(false)).await;
    h.dispatch().await;
    assert_eq!(
        ran_on(&h, &item, "implement").await,
        ["codex/sol", "codex/sol"],
        "the loop back runs it again"
    );

    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .expect("cancel");
    read(&h, TimeDelta::zero(), spent_codex());
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");
    h.dispatch().await;
    let implemented = ran_on(&h, &item, "implement").await;
    assert_eq!(implemented.len(), 3, "the retry starts it: {implemented:?}");
    assert!(
        implemented.iter().all(|m| m == "codex/sol"),
        "{implemented:?}"
    );
    let stored = h.item(&item.id).await;
    assert!(
        (stored.model_plan.0["review"].iter()).all(|c| c.model.to_string() == "codex/sol"),
        "no reroute moved the pin: {:?}",
        stored.model_plan
    );
    let notices = h
        .notices(archon_core::notify::NotificationKind::Progress)
        .await;
    assert!(
        !notices.iter().any(|n| n.contains("instead of codex/sol")),
        "no pin was swapped: {notices:?}"
    );
}

/// The models each stage of `item` was spawned on, in spawn order.
async fn spawned_on(h: &Harness, item: &QueueItem, stage: &str) -> Vec<String> {
    (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == stage)
        .map(|s| s.choice.model.to_string())
        .collect()
}

fn claude_then_codex(h: &Harness, claude: u64, codex: u64) {
    read(
        h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(claude, TimeDelta::hours(3))),
            ("codex", window(codex, TimeDelta::hours(4))),
        ],
    );
}

/// A pinned tier is what was pinned: dispatch resolves it to a model it
/// records as its own, and later quota moves keep that choice in the tier.
#[tokio::test]
async fn dispatch_resolves_a_pinned_tier_which_then_moves_only_within_it() {
    for source in PINS {
        let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
        claude_then_codex(&h, 80, 0);
        let item = enqueue(&h, ROUTED, "pinned tiers", tier_plan()).await;
        let queued = pinned(&h, &item, source).await;
        let before = crate::plan_origins::shown(&h, &item).await;
        assert_eq!(before["implement"][0].model.source, source);

        claude_then_codex(&h, 10, 90);
        h.dispatch().await;
        assert_eq!(spawned_on(&h, &item, "implement").await, ["codex/sol"]);
        let dispatched = crate::plan_origins::shown(&h, &item).await;
        for (key, origin) in dispatched
            .iter()
            .flat_map(|(k, e)| e.iter().map(move |o| (k, o)))
        {
            let model = (origin.model.source, origin.model.by);
            assert_eq!(model, (PlanSource::Tier, Authority::Daemon), "{key}");
            assert!(origin.model.at.is_some(), "{source:?} {key}");
            assert_eq!(origin.effort.source, source, "{key}: the effort stays");
            let tier = origin.tier.as_ref().expect("a tiered choice");
            assert_eq!(tier.source, source, "{key}: the tier is the pin");
        }

        claude_then_codex(&h, 90, 0);
        h.settle_running(|_| completed("implemented")).await;
        h.dispatch().await;
        let reviews = spawned_on(&h, &item, "review").await;
        assert_eq!(reviews, ["claude-code/opus", "claude-code/haiku"]);
        let stored = h.item(&item.id).await;
        let tiers = |plan: &ModelPlan| -> Vec<_> {
            plan.0.values().flatten().map(|c| c.tier.clone()).collect()
        };
        assert_eq!(tiers(&stored.model_plan), tiers(&queued), "{source:?}");
    }
}

/// A run whose stored models are pins moves none of them: not the stored plan,
/// not the driver a restart rebuilds, not the stage it spawns next.
#[tokio::test]
async fn a_recovered_run_with_pinned_models_follows_no_quota_move() {
    for source in PINS {
        let mut h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
        claude_then_codex(&h, 80, 0);
        let item = enqueue(&h, ROUTED, "run with pins", tier_plan()).await;
        h.dispatch().await;
        let before = pinned(&h, &item, source).await;

        h.restart();
        h.set_catalog(catalog()).await;
        archon_daemon::executor::recover(&h.ctx)
            .await
            .expect("recover");
        claude_then_codex(&h, 10, 90);
        h.settle_running(|_| completed("implemented")).await;
        h.dispatch().await;

        let reviews = spawned_on(&h, &item, "review").await;
        assert_eq!(reviews, ["claude-code/opus", "claude-code/haiku"]);
        let stored = h.item(&item.id).await;
        assert_eq!(*stored.model_plan, before, "{source:?}: a pin moved");
        let origins = crate::plan_origins::shown(&h, &stored).await;
        let fields = origins
            .values()
            .flatten()
            .flat_map(|o| [&o.model, &o.effort]);
        assert!(fields.clone().all(|f| f.source == source), "{origins:?}");
        assert_eq!(fields.count(), 8, "the model and effort of four choices");
    }
}

/// A run 1.6.3 left with a tiered implementer keeps that agent through the
/// restart of an upgrade: its pinned model reads without a tier, the same agent.
#[tokio::test]
async fn a_recovered_run_from_before_origins_resumes_its_kept_tiered_implementer() {
    let mut h = harness(Config::default()).await;
    let item = enqueue(&h, ROUTED, "kept through an upgrade", tier_plan()).await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let implementers = |stages: Vec<archon_core::run::StageRun>| -> Vec<_> {
        let of_implement = stages.into_iter().filter(|s| s.stage_id == "implement");
        of_implement.map(|s| (s.choice, s.aether_agent)).collect()
    };
    let first = implementers(h.stages(&item.id).await);
    assert!(first[0].0.tier.is_some(), "a tiered implementer: {first:?}");
    pinned(&h, &item, PlanSource::LegacyUnknown).await;
    let spawned = h.backend.requests().len();

    h.restart();
    h.set_catalog(catalog()).await;
    archon_daemon::executor::recover(&h.ctx)
        .await
        .expect("recover");
    let failed = archon_core::run::StageOutcome::Completed {
        summary: "the greeting is never tested\nVERDICT: fail".into(),
        structured: Some(serde_json::json!({"verdict": "fail", "findings": []})),
    };
    h.settle_running(|_| failed.clone()).await;
    h.dispatch().await;

    let again = implementers(h.stages(&item.id).await);
    assert_eq!(again.len(), 2, "the fix is a second attempt: {again:?}");
    assert_eq!(again[1].0.model, first[0].0.model, "the pinned model");
    assert_eq!(again[1].1, first[0].1, "the kept agent takes the findings");
    assert_eq!(h.backend.requests().len(), spawned, "nothing respawned");
}

/// A retry the catalog repair moves off a retired daemon pick is noted on
/// the item and in its feed, as a move the repair makes at dispatch is.
#[tokio::test]
async fn a_retry_the_catalog_repair_moved_is_noted() {
    let h = harness(untiered_fallback()).await;
    let (item, old) = with_untiered_fallback(&h, "implement").await;
    h.dispatch().await;
    assert_eq!(spawned_on(&h, &item, "implement").await, [old.as_str()]);
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Retired).await;
    h.settle_running(|_| archon_core::run::StageOutcome::Failed {
        error: "aetherd lost the agent".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;

    assert_eq!(
        spawned_on(&h, &item, "implement").await,
        [old.as_str(), next.as_str()]
    );
    let told = changed_models(&h, &item).await;
    let moved = format!("stage implement moved from {old} to {next}: {old} is retired");
    assert!(told.starts_with(&moved), "{told}");
}

/// A retry the run still plans on a superseded model the catalog lists,
/// while the stored plan names another for its slot, keeps its model (I3):
/// its create is sent once, never refused over and over.
#[tokio::test]
async fn a_retry_on_a_listed_superseded_model_keeps_it() {
    let h = harness(untiered_fallback()).await;
    let (item, old) = with_untiered_fallback(&h, "implement").await;
    h.dispatch().await;
    assert_eq!(spawned_on(&h, &item, "implement").await, [old.as_str()]);
    let next = format!("{old}-next");
    list_beside(&h, (&old, &next), Beside::Superseded).await;
    // The stored slot moved on; the run's own copy of the plan did not.
    {
        let mut store = h.ctx.store.lock().await;
        let mut stored = store.item(&item.id).unwrap();
        let plan = stored.model_plan.legacy_mut();
        plan.0.get_mut("implement").unwrap()[0].model = next.parse().unwrap();
        store.update_item(&stored).unwrap();
    }
    // The endless replacement never yields, so only another thread can end it.
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if finished
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_err()
        {
            eprintln!("the stale choice is replaced without end");
            std::process::exit(101);
        }
    });
    h.settle_running(|_| archon_core::run::StageOutcome::Failed {
        error: "aetherd lost the agent".into(),
        infra: true,
    })
    .await;
    for _ in 0..3 {
        h.dispatch().await;
    }
    done.send(()).unwrap();
    watchdog.join().unwrap();

    let attempts = spawned_on(&h, &item, "implement").await;
    assert_eq!(
        attempts,
        [old.as_str(), old.as_str()],
        "one retry, on {old}: {:?}",
        h.state(&item.id).await
    );
    let running: Vec<_> = (h.running().await.into_iter())
        .map(|s| s.choice.model.to_string())
        .collect();
    assert_eq!(running, [old.as_str()]);
}

/// A kept implementer takes its review's findings though a quota move took the
/// stored plan to another provider, before its reviewers were reserved or after:
/// its resume keeps the agent's model.
#[tokio::test]
async fn a_kept_implementer_takes_its_findings_after_a_quota_move() {
    for before_reviews in [true, false] {
        let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
        claude_then_codex(&h, 90, 0);
        let item = enqueue(&h, ROUTED, "kept through a quota move", tier_plan()).await;
        h.dispatch().await;
        let implementers = |stages: Vec<archon_core::run::StageRun>| -> Vec<_> {
            let of_implement = stages.into_iter().filter(|s| s.stage_id == "implement");
            of_implement.map(|s| (s.choice, s.aether_agent)).collect()
        };
        let first = implementers(h.stages(&item.id).await);
        assert_eq!(first[0].0.model.provider, "claude-code", "{first:?}");
        if before_reviews {
            claude_then_codex(&h, 10, 90);
        }
        h.settle_running(|_| completed("implemented")).await;
        h.dispatch().await;
        claude_then_codex(&h, 10, 90);
        let failed = archon_core::run::StageOutcome::Completed {
            summary: "the greeting is never tested\nVERDICT: fail".into(),
            structured: Some(serde_json::json!({"verdict": "fail", "findings": []})),
        };
        h.settle_running(|_| failed.clone()).await;
        h.dispatch().await;

        let again = implementers(h.stages(&item.id).await);
        let case = format!("quota moved before reviews: {before_reviews}");
        assert_eq!(again.len(), 2, "{case}: a second attempt: {again:?}");
        assert_eq!(again[1].0.model, first[0].0.model, "{case}: the kept model");
        assert_eq!(
            again[1].1, first[0].1,
            "{case}: the kept agent takes the findings"
        );
    }
}

/// A parked implementer whose effort is steered after a quota move moved its
/// slot is replaced, not resumed: the fresh agent is routed as any fresh
/// attempt is (I2), never held on the parked agent's spent provider.
#[tokio::test]
async fn an_effort_steered_kept_implementer_is_replaced_on_the_routed_provider() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    claude_then_codex(&h, 90, 0);
    let item = enqueue(&h, ROUTED, "effort steered after a quota move", tier_plan()).await;
    h.dispatch().await;
    let implementers = |stages: Vec<archon_core::run::StageRun>| -> Vec<_> {
        let of_implement = stages.into_iter().filter(|s| s.stage_id == "implement");
        of_implement.map(|s| (s.choice, s.aether_agent)).collect()
    };
    let first = implementers(h.stages(&item.id).await);
    assert_eq!(first[0].0.model.provider, "claude-code", "{first:?}");
    let effort = archon_core::model::Effort::Low;
    assert_ne!(first[0].0.effort, effort, "the steer changes the effort");
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let steer = archon_core::steer::Steer {
        stage: Some("implement".into()),
        effort: Some(effort),
        ..Default::default()
    };
    let request = Request::SteerItem {
        item: item.id.clone(),
        steer,
    };
    h.cli(request).await.expect("the steer is taken");
    let failed = archon_core::run::StageOutcome::Completed {
        summary: "the greeting is never tested\nVERDICT: fail".into(),
        structured: Some(serde_json::json!({"verdict": "fail", "findings": []})),
    };
    h.settle_running(|_| failed.clone()).await;
    h.dispatch().await;

    let again = implementers(h.stages(&item.id).await);
    assert_eq!(again.len(), 2, "a second attempt: {again:?}");
    let (choice, agent) = &again[1];
    assert_ne!(agent, &first[0].1, "a fresh agent");
    assert_eq!(choice.effort, effort, "the steered effort");
    assert_eq!(choice.model.provider, "codex", "routed by quota: {again:?}");
}

/// A kept implementer that cannot go on after a quota move, for findings or
/// on an infrastructure wake Aether refuses, is replaced by a fresh agent
/// routed as any fresh stage is (I2): on the provider that has quota.
#[tokio::test]
async fn a_kept_implementer_that_cannot_go_on_is_replaced_on_the_routed_provider() {
    for findings in [true, false] {
        let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
        claude_then_codex(&h, 90, 0);
        let item = enqueue(&h, ROUTED, "replaced after a quota move", tier_plan()).await;
        h.dispatch().await;
        let implementers = |stages: Vec<archon_core::run::StageRun>| -> Vec<_> {
            let of_implement = stages.into_iter().filter(|s| s.stage_id == "implement");
            of_implement.map(|s| (s.choice, s.aether_agent)).collect()
        };
        let first = implementers(h.stages(&item.id).await);
        assert_eq!(first[0].0.model.provider, "claude-code", "{first:?}");
        let agent = first[0].1.clone().expect("the implementer's agent");
        if findings {
            h.settle_running(|_| completed("implemented")).await;
            h.dispatch().await;
            claude_then_codex(&h, 10, 90);
            h.backend.kill(&agent);
            let failed = archon_core::run::StageOutcome::Completed {
                summary: "the greeting is never tested\nVERDICT: fail".into(),
                structured: Some(serde_json::json!({"verdict": "fail", "findings": []})),
            };
            h.settle_running(|_| failed.clone()).await;
        } else {
            claude_then_codex(&h, 10, 90);
            h.settle_running(|_| archon_core::run::StageOutcome::Failed {
                error: "aetherd lost the agent".into(),
                infra: true,
            })
            .await;
        }
        h.dispatch().await;
        h.dispatch().await;

        let case = format!("findings: {findings}");
        let again = implementers(h.stages(&item.id).await);
        // The attempt reserved for the kept agent creates none: its replacement does.
        assert_eq!(again.len(), 3, "{case}: {again:?}");
        assert_eq!(
            again[1].1, None,
            "{case}: nothing created on the kept model"
        );
        let (routed, fresh) = &again[2];
        assert_eq!(routed.model.provider, "codex", "{case}: {again:?}");
        assert!(
            fresh.is_some() && fresh.as_ref() != Some(&agent),
            "{case}: a fresh agent"
        );
        let created = h.backend.requests().pop().expect("the fresh implementer");
        assert_eq!(&created.choice, routed, "{case}: created as recorded");
    }
}

/// A pin superseded while the run waits for it, though Aether still lists it,
/// leaves the stored models as they were: its reviewers start on their pins.
#[tokio::test]
async fn a_repair_that_moves_no_model_reserves_the_pending_stages() {
    let h = harness(untiered_fallback()).await;
    let pins = serde_json::json!({
        "implement": [{"model": "claude-code/sonnet"}],
        "review": [{"model": "claude-code/opus"}, {"model": "claude-code/sonnet"}],
        "ship": [{"model": "claude-code/haiku"}],
    });
    let item = enqueue(
        &h,
        ROUTED,
        "pins only",
        serde_json::from_value(pins).unwrap(),
    )
    .await;
    h.dispatch().await;
    assert_eq!(
        spawned_on(&h, &item, "implement").await,
        ["claude-code/sonnet"]
    );
    list_beside(
        &h,
        ("claude-code/opus", "claude-code/opus-next"),
        Beside::Superseded,
    )
    .await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;

    let reviews = spawned_on(&h, &item, "review").await;
    assert_eq!(reviews, ["claude-code/opus", "claude-code/sonnet"]);
    let stored = h.item(&item.id).await;
    assert_eq!(
        stored.model_plan.0["review"][0].model.to_string(),
        "claude-code/opus",
        "a pin is never rewritten"
    );
}

const PLANNED: &str = "planned";
const PLANNED_TOML: &str = r#"[pipeline]
name = "planned"
description = "a planner before a kept implementer and its reviewer"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "plan"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
gate = "all_pass"
timeout_minutes = 30
on_fail = { goto = "implement", max_loops = 1 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

/// A quota move while a stage before the implementer runs starts the
/// implementer on the provider it moved to; the review's findings then go to
/// that same agent, on the model it ran, never to a replacement on another.
#[tokio::test]
async fn a_quota_move_before_the_implementer_keeps_its_agent_through_the_fix() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    h.add_pipeline(PLANNED, PLANNED_TOML);
    claude_then_codex(&h, 90, 0);
    let plan = serde_json::json!({
        "plan": [{"tier": "strong"}],
        "implement": [{"tier": "strong"}],
        "review": [{"tier": "strong"}],
        "ship": [{"tier": "fast"}],
    });
    let item = enqueue(
        &h,
        PLANNED,
        "moved mid-plan",
        serde_json::from_value(plan).unwrap(),
    )
    .await;
    h.dispatch().await;
    assert_eq!(spawned_on(&h, &item, "plan").await, ["claude-code/sonnet"]);
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("planned")).await;
    h.dispatch().await;
    let first: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "implement")
        .collect();
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(
        first[0].choice.model.to_string(),
        "codex/sol",
        "routed by quota"
    );

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|_| common::verdict(false)).await;
    h.dispatch().await;

    let again: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "implement")
        .collect();
    assert_eq!(again.len(), 2, "the fix is a second attempt: {again:?}");
    assert_eq!(
        again[1].choice.model, first[0].choice.model,
        "the agent's model"
    );
    assert_eq!(
        again[1].aether_agent, first[0].aether_agent,
        "the same agent"
    );
    assert!(
        again.iter().all(|s| s.choice.model.provider == "codex"),
        "no replacement on a model the agent never ran: {again:?}"
    );
}

/// A fresh infrastructure retry of a stage a quota move routed elsewhere runs
/// where the move took it, as every fresh attempt is routed.
#[tokio::test]
async fn a_fresh_infra_retry_of_a_quota_moved_stage_follows_the_move() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    claude_then_codex(&h, 90, 0);
    let item = enqueue(&h, ROUTED, "retried after a move", tier_plan()).await;
    h.dispatch().await;
    assert_eq!(
        spawned_on(&h, &item, "implement").await,
        ["claude-code/sonnet"]
    );
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviews = spawned_on(&h, &item, "review").await;
    assert!(
        reviews.iter().all(|m| m.starts_with("codex/")),
        "{reviews:?}"
    );

    h.settle_running(|s| match s.fan_index {
        0 => archon_core::run::StageOutcome::Failed {
            error: "aetherd lost the agent".into(),
            infra: true,
        },
        _ => common::verdict(true),
    })
    .await;
    h.dispatch().await;
    let retried: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review" && s.fan_index == 0)
        .map(|s| s.choice.model.to_string())
        .collect();
    assert_eq!(retried.len(), 2, "the slot is retried: {retried:?}");
    assert_eq!(
        retried[1], retried[0],
        "the retry runs where the move took it"
    );
}

fn lost_agent() -> archon_core::run::StageOutcome {
    archon_core::run::StageOutcome::Failed {
        error: "aetherd lost the agent".into(),
        infra: true,
    }
}

/// A kept implementer resumed on its own model after a quota move, then lost
/// by Aether, is woken in place on that model: no fresh agent on the moved one.
#[tokio::test]
async fn a_resumed_kept_implementer_lost_after_a_quota_move_is_woken_on_its_model() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    kept_implementer_woken_after_a_quota_move(&h).await;
}

/// The implements of `stages`, each with its model and agent, in order.
fn implementers(
    stages: Vec<archon_core::run::StageRun>,
) -> Vec<(
    archon_core::model::ModelRef,
    Option<archon_core::aether_ids::AetherAgentId>,
)> {
    let of_implement = stages.into_iter().filter(|s| s.stage_id == "implement");
    of_implement
        .map(|s| (s.choice.model, s.aether_agent))
        .collect()
}

/// An item whose kept implementer, resumed on its own model after a quota
/// move took the stored plan to codex, was lost and woken in place.
async fn kept_implementer_woken_after_a_quota_move(h: &Harness) -> QueueItem {
    claude_then_codex(h, 90, 0);
    let item = enqueue(h, ROUTED, "woken after a move", tier_plan()).await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    claude_then_codex(h, 10, 90);
    h.settle_running(|_| common::verdict(false)).await;
    h.dispatch().await;
    let resumed = implementers(h.stages(&item.id).await);
    assert_eq!(resumed.len(), 2, "the fix resumes: {resumed:?}");
    assert_eq!(resumed[1], resumed[0], "on the agent's own model");

    h.backend
        .fail_agent(resumed[0].1.as_ref().expect("an agent"));
    h.settle_running(|_| lost_agent()).await;
    h.dispatch().await;
    let woken = implementers(h.stages(&item.id).await);
    assert_eq!(woken.len(), 3, "the lost attempt is replaced: {woken:?}");
    assert_eq!(woken[2], woken[0], "the same agent on the model it ran");
    let agent = woken[0].1.clone().expect("an agent");
    assert_eq!(h.backend.woken(), [agent], "woken, not replaced");
    item
}

/// The cold retry of a resumed implementer whose wake was spent is a fresh
/// agent, routed by quota as any fresh attempt is: off a spent provider.
#[tokio::test]
async fn a_cold_retry_of_a_resumed_implementer_moves_off_a_spent_provider() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    let item = kept_implementer_woken_after_a_quota_move(&h).await;
    claude_then_codex(&h, 90, 0);
    h.settle_running(|_| lost_agent()).await;
    h.dispatch().await;
    let cold = implementers(h.stages(&item.id).await);
    assert_eq!(cold.len(), 4, "the woken attempt is replaced: {cold:?}");
    assert_ne!(cold[3].1, cold[0].1, "a fresh agent");
    assert_eq!(
        cold[3].0.provider, "claude-code",
        "the fresh agent leaves spent codex: {cold:?}"
    );
}

/// A cold replacement skips the quota wait, so it never ends a note that a
/// pending attempt of its stage waits for quota, moved by quota or not.
#[tokio::test]
async fn a_cold_replacement_leaves_its_stage_s_quota_wait_note() {
    for moved in [false, true] {
        cold_replacement_beside_a_wait(moved).await;
    }
}

async fn cold_replacement_beside_a_wait(moved: bool) {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    claude_then_codex(&h, 90, 0);
    let item = enqueue(&h, ROUTED, "a sibling waits", tier_plan()).await;
    h.dispatch().await;
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    if moved {
        claude_then_codex(&h, 90, 0);
    }
    let waits = "stage review waits for provider quota";
    {
        let mut store = h.ctx.store.lock().await;
        let mut held = store.item(&item.id).unwrap();
        let note =
            format!("2026-11-19 18:00 UTC: {waits}: every provider meeting its floor is out");
        held.provider_switch = Some(note);
        store.update_item(&held).unwrap();
    }
    h.settle_running(|s| match s.fan_index {
        0 => archon_core::run::StageOutcome::Failed {
            error: "agent stalled: no events for 30 min (engine state not reported)".into(),
            infra: true,
        },
        _ => common::verdict(true),
    })
    .await;
    h.dispatch().await;
    let first: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "review" && s.fan_index == 0)
        .collect();
    assert_eq!(first.len(), 2, "a cold replacement: {first:?}");
    let to = if moved { "claude-code/" } else { "codex/" };
    let on = first[1].choice.model.to_string();
    assert!(
        on.starts_with(to),
        "moved {moved}: the replacement runs on {on}"
    );
    let note = h.item(&item.id).await.provider_switch.unwrap_or_default();
    assert!(
        note.contains(waits),
        "moved {moved}: the wait still stands: {note}"
    );
}

/// A daemon choice the catalog supersedes while still listing it, with no
/// eligible model to move to, runs: no refused create, no loop.
#[tokio::test]
async fn a_superseded_daemon_choice_with_no_successor_allowed_runs() {
    let mut config = untiered_fallback();
    let old = common::MODEL.to_owned();
    let next = format!("{old}-next");
    config.models.allow = vec![old.parse().unwrap()];
    let h = harness(config).await;
    let pins = serde_json::json!({
        "review": [{"model": old}, {"model": old}],
        "ship": [{"model": old}],
    });
    let item = enqueue(
        &h,
        ROUTED,
        "one model allowed",
        serde_json::from_value(pins).unwrap(),
    )
    .await;
    let filled = &crate::plan_origins::shown(&h, &item).await["implement"][0].model;
    assert_eq!(
        (filled.source, filled.by),
        (PlanSource::Fallback, Authority::Daemon)
    );
    assert_eq!(item.model_plan.0["implement"][0].model.to_string(), old);
    list_beside(&h, (&old, &next), Beside::Superseded).await;
    // A create refused on the kept model, reserved again, would never end.
    for _ in 0..2 {
        let bounded = tokio::time::timeout(std::time::Duration::from_secs(10), h.dispatch());
        bounded.await.expect("dispatch settles");
    }

    let implements: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(|s| s.stage_id == "implement")
        .collect();
    assert_eq!(implements.len(), 1, "one attempt: {implements:?}");
    assert_eq!(implements[0].choice.model.to_string(), old);
    assert_eq!(
        implements[0].state,
        archon_core::run::StageState::Running,
        "{implements:?}"
    );
    let created = h.backend.requests();
    assert_eq!(created.len(), 1, "one create");
    assert_eq!(created[0].choice.model.to_string(), old);
}

/// A fresh retry is routed by quota as a fresh stage is: once the provider a
/// move took a reviewer to is spent, its cold retry goes where quota is.
#[tokio::test]
async fn a_cold_retry_on_a_spent_provider_moves_to_one_with_quota() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    claude_then_codex(&h, 90, 0);
    let item = enqueue(&h, ROUTED, "a cold retry routed", tier_plan()).await;
    h.dispatch().await;
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviews = spawned_on(&h, &item, "review").await;
    assert!(
        reviews.iter().all(|m| m.starts_with("codex/")),
        "{reviews:?}"
    );

    claude_then_codex(&h, 90, 0);
    let first_review = |s: &archon_core::run::StageRun| s.stage_id == "review" && s.fan_index == 0;
    let reviewer = (h.stages(&item.id).await.into_iter()).find(first_review);
    h.backend
        .fail_agent(&reviewer.and_then(|s| s.aether_agent).expect("an agent"));
    for _ in 0..2 {
        h.settle_running(|s| match s.stage_id == "review" && s.fan_index == 0 {
            true => lost_agent(),
            false => common::verdict(true),
        })
        .await;
        h.dispatch().await;
    }
    let retried: Vec<_> = (h.stages(&item.id).await.into_iter())
        .filter(first_review)
        .map(|s| s.choice.model.to_string())
        .collect();
    assert_eq!(retried.len(), 3, "a wake, then a cold retry: {retried:?}");
    assert_eq!(retried[1], retried[0], "the wake keeps its agent's model");
    assert!(
        retried[2].starts_with("claude-code/"),
        "the cold retry leaves the spent provider: {retried:?}"
    );
}

/// A stage that waited for quota, then starts unmoved once its provider has
/// quota again, no longer reads as waiting.
#[tokio::test]
async fn a_stage_that_stops_waiting_for_quota_drops_its_wait_note() {
    let h = harness(weighted(&[("claude-code", 1), ("codex", 1)])).await;
    claude_then_codex(&h, 90, 0);
    let item = enqueue(&h, ROUTED, "waits, then starts", tier_plan()).await;
    h.dispatch().await;
    claude_then_codex(&h, 10, 90);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    claude_then_codex(&h, 0, 0);
    h.settle_running(|_| common::verdict(true)).await;
    h.dispatch().await;
    let waiting = h.item(&item.id).await.provider_switch.unwrap_or_default();
    assert!(
        waiting.contains("stage ship waits for provider quota"),
        "{waiting}"
    );
    assert!(spawned_on(&h, &item, "ship").await.is_empty(), "ship waits");

    claude_then_codex(&h, 0, 90);
    h.dispatch().await;
    let ship = spawned_on(&h, &item, "ship").await;
    assert_eq!(ship.len(), 1, "ship starts: {ship:?}");
    let note = h.item(&item.id).await.provider_switch.unwrap_or_default();
    assert!(
        !note.contains("stage ship waits for provider quota"),
        "{note}"
    );
    assert!(
        note.contains(&format!(
            "stage ship no longer waits for provider quota; it runs on {}",
            ship[0]
        )),
        "{note}"
    );
}
