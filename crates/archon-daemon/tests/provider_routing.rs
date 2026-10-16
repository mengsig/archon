//! Provider weights: an item holds one provider for its tier-only choices,
//! picked by weight, through dispatch and retries; a stage's capability floor
//! moves it off that provider; with no weights the orchestrator's provider
//! wins as before. Quota headroom scales the weights: an item waits while
//! every provider is out and a running one moves off a provider that ran out.

mod common;

use archon_core::Timestamp;
use archon_core::config::Config;
use archon_core::model::{ModelPlan, ModelPlanDraft};
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
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
    let codex_reset = archon_core::now() + TimeDelta::minutes(40);
    // Claude keeps 10%, under the orchestrator's 15% reserve; Codex is spent.
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(2))),
            ("codex", window(0, TimeDelta::minutes(40))),
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
    let codex_reset = archon_core::now() + TimeDelta::hours(1);
    read(
        &h,
        TimeDelta::zero(),
        vec![
            ("claude-code", window(10, TimeDelta::hours(3))),
            ("codex", window(0, TimeDelta::hours(1))),
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
