use crate::{common, link_common};
use archon_aether::fake::Scripted;
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::plan_origin::{Authority, PlanSource, PlanWriter};
use archon_core::queue::QueueState;
use archon_daemon::executor;
use link_common::Harness;

fn models(h: &Harness, names: &[&str]) {
    let models: Vec<_> = names
        .iter()
        .map(|name| serde_json::json!({"model": name, "configured": true}))
        .collect();
    h.fake.set_default(
        "list_models",
        Scripted::reply("models", serde_json::json!(models)),
    );
}

async fn queued(h: &Harness, model: &str, daemon: bool) -> archon_core::id::QueueItemId {
    let repo = h.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    common::init_repo(&repo);
    let id = link_common::queued_item(&h.ctx, &repo).await;
    let mut store = h.ctx.store.lock().await;
    let mut item = store.item(&id).unwrap();
    let mut plan = ModelPlan::default();
    plan.0.insert(
        "implement".into(),
        vec![ModelChoice::pinned(model.parse().unwrap(), Effort::Medium)],
    );
    item.model_plan = if daemon {
        let mut writer = PlanWriter::new(&id, archon_core::now());
        writer.filled(plan, (PlanSource::Fallback, PlanSource::Fallback));
        writer.finish()
    } else {
        let draft =
            serde_json::from_value(serde_json::json!({"implement": [{"model": model}]})).unwrap();
        let mut writer = PlanWriter::new(&id, archon_core::now());
        writer.request(&draft, plan, Authority::Operator);
        writer.finish()
    };
    store.update_item(&item).unwrap();
    id
}

#[tokio::test]
async fn unavailable_catalog_holds_without_a_run_and_recovers_on_reconnect() {
    let h = Harness::new().await;
    models(&h, &[]);
    h.connect().await;
    let id = queued(&h, "p/old", false).await;
    executor::start_item(&h.ctx, &id).await.unwrap();
    {
        let store = h.ctx.store.lock().await;
        assert_eq!(store.item(&id).unwrap().state, QueueState::Queued);
        assert!(store.runs_for_item(&id).unwrap().is_empty());
    }
    assert!(h.fake.received_named("create_agent").is_empty());
    models(&h, &["p/old"]);
    h.connect().await;
    executor::start_item(&h.ctx, &id).await.unwrap();
    let store = h.ctx.store.lock().await;
    assert_eq!(store.runs_for_item(&id).unwrap().len(), 1);
    assert!(matches!(
        store.item(&id).unwrap().state,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn removed_pins_block_but_daemon_choices_use_an_eligible_default() {
    let h = Harness::new().await;
    models(&h, &["p/new"]);
    h.connect().await;
    let id = queued(&h, "p/old", false).await;
    executor::start_item(&h.ctx, &id).await.unwrap();
    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    assert!(matches!(item.state, QueueState::Blocked { ref reason } if reason.contains("re-plan")));
    assert!(store.runs_for_item(&id).unwrap().is_empty());
    drop(store);
    assert!(h.fake.received_named("create_agent").is_empty());

    let h = Harness::new().await;
    models(&h, &["p/new"]);
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    executor::start_item(&h.ctx, &id).await.unwrap();
    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    assert_eq!(item.model_plan.0["implement"][0].model.to_string(), "p/new");
    assert_eq!(
        item.model_plan.origin(&id, "implement", 0).model.source,
        PlanSource::Migrated
    );
    assert_eq!(store.runs_for_item(&id).unwrap().len(), 1);
}

#[tokio::test]
async fn dispatch_blocks_a_removed_pin_before_creating_an_attempt() {
    let h = Harness::new().await;
    models(&h, &["p/new"]);
    h.connect().await;
    let id = queued(&h, "p/old", false).await;
    archon_daemon::dispatcher::dispatch_once(&h.ctx)
        .await
        .unwrap();
    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    assert!(matches!(item.state, QueueState::Blocked { ref reason } if reason.contains("re-plan")));
    assert!(store.runs_for_item(&id).unwrap().is_empty());
    assert!(h.fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn daemon_choice_waits_when_no_allowed_replacement_exists() {
    let mut config = archon_core::config::Config::default();
    config.models.deny.push("p/new".parse().unwrap());
    let h = Harness::with(config, None).await;
    models(&h, &["p/new"]);
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    executor::start_item(&h.ctx, &id).await.unwrap();
    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    assert_eq!(item.state, QueueState::Queued);
    assert_eq!(item.model_plan.0["implement"][0].model.to_string(), "p/old");
    assert!(store.runs_for_item(&id).unwrap().is_empty());
    assert!(h.fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn a_removed_tier_resolution_moves_within_the_requested_tier() {
    use archon_core::model_tier::{Tier, TierChoice};
    let h = Harness::new().await;
    h.fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            serde_json::json!([
                {"model": "p/new", "configured": true, "tier": "strong"}
            ]),
        ),
    );
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    {
        let mut store = h.ctx.store.lock().await;
        let mut item = store.item(&id).unwrap();
        let tiered = ModelChoice {
            model: "p/old".parse().unwrap(),
            effort: Effort::High,
            tier: Some(TierChoice {
                requested: Tier::Strong,
                resolved: Tier::Strong,
                provider: None,
            }),
        };
        let mut plan = ModelPlan::default();
        plan.0.insert("implement".into(), vec![tiered]);
        let draft = serde_json::from_value(
            serde_json::json!({"implement": [{"tier": "strong", "effort": "high"}]}),
        )
        .unwrap();
        let mut writer = PlanWriter::new(&id, archon_core::now());
        writer.request(&draft, plan, Authority::Operator);
        item.model_plan = writer.finish();
        store.update_item(&item).unwrap();
    }
    executor::start_item(&h.ctx, &id).await.unwrap();
    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    let choice = &item.model_plan.0["implement"][0];
    assert_eq!(choice.model.to_string(), "p/new");
    let origin = item.model_plan.origin(&id, "implement", 0).model;
    assert_eq!(origin.source, PlanSource::Migrated);
    assert!(origin.note.as_deref().unwrap().contains("p/old"));
    assert_eq!(choice.effort, Effort::High);
    assert_eq!(choice.tier.as_ref().unwrap().requested, Tier::Strong);
    assert_eq!(store.runs_for_item(&id).unwrap().len(), 1);
}
