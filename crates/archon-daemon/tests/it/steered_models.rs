//! A model named for a stage while its item waits is the model that runs:
//! a steer, an `update_item` model plan and a reviewer plan each pin it, and
//! no tier resolves it to another model at dispatch.

use crate::common;

use archon_core::autonomy::AutonomyPatch;
use archon_core::config::Config;
use archon_core::id::QueueItemId;
use archon_core::model::{Effort, ModelChoice, ModelPlanDraft};
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
use archon_core::protocol::{EnqueueRequest, ItemPatch, PauseTarget, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::Steer;
use common::{Harness, PIPELINE, REPO_NAME};

const BALANCED: &str = "claude-code/claude-sonnet-5";
const FAST: &str = "claude-code/claude-haiku-5";

fn catalog() -> ModelCatalog {
    let entry = |model: &str, tier| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(tier),
        is_default: false,
    };
    ModelCatalog::new(vec![
        entry(FAST, Tier::Fast),
        entry(BALANCED, Tier::Balanced),
    ])
}

async fn harness() -> Harness {
    let h = Harness::with_config(Config::default()).await;
    h.set_catalog(catalog()).await;
    h
}

/// Every key of `keys` asks for the balanced tier, which is [`BALANCED`].
fn balanced(keys: &[&str]) -> ModelPlanDraft {
    let tier = serde_json::json!([{"tier": "balanced", "provider": "claude-code"}]);
    let plan: serde_json::Map<_, _> = keys.iter().map(|k| ((*k).into(), tier.clone())).collect();
    serde_json::from_value(plan.into()).unwrap()
}

async fn enqueue(h: &Harness, pipeline: &str, plan: ModelPlanDraft, list: &str) -> QueueItem {
    let request = EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: "an answer".into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan,
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch::parse_list(list).expect("knobs"),
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await {
        Ok(Reply::Item(item)) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

fn global() -> PauseTarget {
    PauseTarget::Global { hard: false }
}

/// Dispatch while paused starts nothing; after resume it starts the item.
async fn resume_and_dispatch(h: &Harness, item: &QueueItemId) {
    h.dispatch().await;
    assert_eq!(
        h.state(item).await,
        QueueState::Queued,
        "paused: nothing starts"
    );
    h.cli(Request::Resume(global())).await.expect("resume");
    h.dispatch().await;
    assert!(
        matches!(h.state(item).await, QueueState::Running { .. }),
        "resumed: the item runs"
    );
}

fn fast(effort: Effort) -> ModelChoice {
    ModelChoice::pinned(FAST.parse().unwrap(), effort)
}

#[tokio::test]
async fn a_model_steered_onto_a_paused_item_is_the_model_dispatch_runs() {
    let h = harness().await;
    h.cli(Request::Pause(global())).await.expect("pause");
    let plan = balanced(&["implement", "review", "ship"]);
    let item = enqueue(&h, PIPELINE, plan, "").await;
    let asked = &item.model_plan.0["implement"][0];
    assert_eq!(asked.model.to_string(), BALANCED, "the tier resolves");
    assert!(asked.tier.is_some(), "and is kept to resolve again");

    let steer = Steer {
        stage: Some("implement".into()),
        model: Some(FAST.parse().unwrap()),
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    let request = Request::SteerItem {
        item: item.id.clone(),
        steer,
    };
    let Ok(Reply::Item(steered)) = h.cli(request).await else {
        panic!("the steer is taken");
    };
    assert_eq!(steered.model_plan.0["implement"], [fast(Effort::Low)]);

    resume_and_dispatch(&h, &item.id).await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "the implementer runs");
    assert_eq!(
        stages[0].choice,
        fast(Effort::Low),
        "the steered model runs"
    );
    let spawned = h.backend.requests().pop().expect("a spawn");
    assert_eq!(spawned.choice, fast(Effort::Low), "Aether is asked for it");
    let stored = h.item(&item.id).await;
    assert_eq!(stored.model_plan.0["implement"], [fast(Effort::Low)]);
    let review = &stored.model_plan.0["review"][0];
    assert_eq!(
        review.model.to_string(),
        BALANCED,
        "an unsteered tier stays"
    );
}

#[tokio::test]
async fn an_update_item_model_plan_and_reviewer_plan_pin_their_models_through_dispatch() {
    let h = harness().await;
    h.cli(Request::Pause(global())).await.expect("pause");
    let plan = balanced(&["implement", "implement.review"]);
    let item = enqueue(&h, "direct", plan, "review=standard").await;
    let reviewer = &item.model_plan.0["implement.review"][0];
    assert!(reviewer.tier.is_some(), "the reviewers ask by tier");

    let pinned = serde_json::json!({
        "implement": [{"model": FAST, "effort": "low"}],
        "implement.review": [{"model": FAST, "effort": "low"}],
    });
    let patch = ItemPatch {
        model_plan: Some(serde_json::from_value(pinned).unwrap()),
        ..ItemPatch::default()
    };
    let request = Request::UpdateItem {
        item: item.id.clone(),
        patch,
    };
    let Ok(Reply::Item(updated)) = h.cli(request).await else {
        panic!("the update is taken");
    };
    assert_eq!(
        updated.model_plan.0["implement.review"],
        [fast(Effort::Low)]
    );

    resume_and_dispatch(&h, &item.id).await;
    let spawned = h.backend.requests().pop().expect("the implementer");
    assert_eq!(spawned.choice, fast(Effort::Low), "the planned model runs");
    // By default a planned reviewer effort is a floor ("low or higher").
    let named = format!("taken in turn: {FAST} at effort low");
    assert!(
        spawned.instructions.contains(&named),
        "its reviewers run on the planned model: {}",
        spawned.instructions
    );
    let stored = h.item(&item.id).await;
    assert_eq!(stored.model_plan.0["implement.review"], [fast(Effort::Low)]);
}

#[tokio::test]
async fn request_checks_worker_and_reviewer_pins_before_policy_and_storage() {
    use archon_aether::fake::Scripted;
    use serde_json::json;
    let h = Harness::new().await;
    let canonical = "claude-code/claude-opus-5-5";
    let listed = json!([
        {"model": canonical, "configured": true, "aliases": ["opus"]},
        {"model": "codex/gpt-6-astra", "configured": true}
    ]);
    h.catalog_server
        .set_default("list_models", Scripted::reply("models", listed));
    let item = enqueue(
        &h,
        "direct",
        serde_json::from_value(json!({
            "implement": [{"model":"claude-code/opus"}]
        }))
        .unwrap(),
        "review=none",
    )
    .await;
    assert_eq!(
        h.item(&item.id).await.model_plan.0["implement"][0]
            .model
            .to_string(),
        canonical
    );
    for key in ["implement", "implement.workers", "implement.review"] {
        let unknown = "codex/gpt-6.1-astra";
        let patch = ItemPatch {
            model_plan: Some(
                serde_json::from_value(json!({ (key): [{"model": unknown}] })).unwrap(),
            ),
            ..Default::default()
        };
        let error = h
            .cli(Request::UpdateItem {
                item: item.id.clone(),
                patch,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(unknown) && error.contains("nearest: codex/gpt-6-astra"),
            "{error}"
        );
    }
    h.repo_config(&format!("[models]\ndeny = [\"{canonical}\"]\n"));
    let steer = Steer {
        model: Some("claude-code/opus".parse().unwrap()),
        stage: Some("implement".into()),
        ..Default::default()
    };
    let error = h
        .cli(Request::SteerItem {
            item: item.id.clone(),
            steer,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(canonical) && error.contains("not allowed"),
        "{error}"
    );
}

#[tokio::test]
async fn a_superseded_pin_stays_pinned_and_the_reply_names_its_successor() {
    use archon_aether::fake::Scripted;
    use serde_json::json;
    let h = Harness::new().await;
    let old = "claude-code/claude-fable-5-1";
    let successor = "claude-code/claude-fable-5-2";
    h.catalog_server.set_default(
        "list_models",
        Scripted::reply(
            "models",
            json!([
                {"model": old, "configured": true, "superseded_by": "claude-fable-5-2"},
                {"model": successor, "configured": true}
            ]),
        ),
    );
    let plan = serde_json::from_value(json!({"implement": [{"model": old}]})).unwrap();
    let item = enqueue(&h, "direct", plan, "review=none").await;
    assert_eq!(item.model_plan.0["implement"][0].model.to_string(), old);
    let warning = item.model_plan.catalog_check.as_ref().unwrap().to_string();
    assert!(
        warning.contains(old) && warning.contains(successor),
        "{warning}"
    );
    assert_eq!(
        h.item(&item.id).await.model_plan.catalog_check,
        item.model_plan.catalog_check
    );
}

#[tokio::test]
async fn an_outage_defers_an_explicit_pin_but_cannot_resolve_a_tier() {
    use archon_aether::fake::Scripted;
    use serde_json::json;
    let h = Harness::new().await;
    h.catalog_server.set_default(
        "list_models",
        Scripted::refuse("unavailable", "catalog offline"),
    );
    let plan = serde_json::from_value(json!({"implement": [{"model": FAST}]})).unwrap();
    let item = enqueue(&h, "direct", plan, "review=none").await;
    let deferred = item.model_plan.catalog_check.as_ref().unwrap();
    assert!(deferred.to_string().contains("not checked"));
    assert!(deferred.to_string().contains("catalog offline"));
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("expected item detail");
    };
    assert_eq!(
        detail.item.model_plan.catalog_check.as_ref(),
        Some(deferred)
    );
    let patch = ItemPatch {
        model_plan: Some(balanced(&["implement"])),
        ..Default::default()
    };
    let refused = h
        .cli(Request::UpdateItem {
            item: item.id.clone(),
            patch,
        })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("tier"), "{refused}");
}
