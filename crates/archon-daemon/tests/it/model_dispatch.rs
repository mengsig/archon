use crate::{common, link_common};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use archon_aether::fake::{self, Scripted};
use archon_core::id::QueueItemId;
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::plan_origin::{Authority, PlanSource, PlanWriter};
use archon_core::queue::QueueState;
use archon_core::run::{RunState, StageState};
use archon_daemon::executor;
use link_common::Harness;
use serde_json::json;

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
        writer.finish(archon_core::plan_origin::Sizing::Off { policy_cap: None })
    } else {
        let draft =
            serde_json::from_value(serde_json::json!({"implement": [{"model": model}]})).unwrap();
        let mut writer = PlanWriter::new(&id, archon_core::now());
        writer.request(&draft, plan, Authority::Operator);
        writer.finish(archon_core::plan_origin::Sizing::Off { policy_cap: None })
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
        item.model_plan = writer.finish(archon_core::plan_origin::Sizing::Off { policy_cap: None });
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

#[derive(Clone, Copy)]
struct CreateRefusal {
    count: usize,
    on_node: bool,
    code: &'static str,
    refresh_fails: bool,
}

const REMOVED_AT_CREATE: CreateRefusal = CreateRefusal {
    count: 1,
    on_node: false,
    code: "unavailable",
    refresh_fails: false,
};

fn remove_model_at_create(h: &Harness, refusal: CreateRefusal) {
    assert!((1..=2).contains(&refusal.count));
    let models = ["old", "new", "last"];
    let removed = Arc::new(AtomicUsize::new(0));
    let catalog = removed.clone();
    h.fake.set_responder(
        "list_models",
        Arc::new(move |_| {
            let current = catalog.load(Ordering::Relaxed);
            if current > 0 && refusal.refresh_fails {
                return Scripted::refuse("unavailable", "catalog offline");
            }
            let model = format!("p/{}", models[current]);
            Scripted::reply("models", json!([{"model": model, "configured": true}]))
        }),
    );
    h.fake.set_responder(
        "create_agent",
        Arc::new(move |frame| {
            let starts = frame["start"]
                .as_bool()
                .expect("create says whether to start");
            if !starts && !refusal.on_node {
                return Scripted::reply("agent", fake::agent("agt_node", None));
            }
            let current = removed.load(Ordering::Relaxed);
            let model = models[current];
            assert_eq!(frame["model"], json!({"provider": "p", "model": model}));
            if current < refusal.count {
                removed.fetch_add(1, Ordering::Relaxed);
                return Scripted::refuse(
                    refusal.code,
                    &format!("unknown model `{model}` for provider `p`"),
                );
            }
            if !starts {
                return Scripted::reply("agent", fake::agent("agt_node", None));
            }
            Scripted::reply("agent", fake::agent("agt_new", Some("tsk_new")))
        }),
    );
    h.fake.set_default("cancel", Scripted::ok());
    h.fake.set_default(
        "get_agent",
        Scripted::reply("agent", fake::agent("agt_node", None)),
    );
    h.fake
        .set_default("list_agents", Scripted::reply("agents", json!([])));
    h.fake.set_default(
        "get_task",
        Scripted::reply(
            "task",
            fake::task(
                "tsk_new",
                Some("agt_new"),
                fake::FAKE_RUN,
                json!({"state": "active"}),
            ),
        ),
    );
}

async fn issue_admitted_slot(h: &Harness, id: &QueueItemId) {
    tokio::time::timeout(link_common::WAIT, async {
        executor::start_item(&h.ctx, id).await.unwrap();
        let runs = h.ctx.store.lock().await.runs_for_item(id).unwrap();
        assert_eq!(runs.len(), 1, "admission starts one run");
        assert!(h.fake.received_named("create_agent").is_empty());
        executor::spawn_admitted(&h.ctx, &runs[0].id, 1)
            .await
            .unwrap();
    })
    .await
    .expect("dispatch finishes");
}

#[tokio::test]
async fn create_refusal_repairs_a_movable_slot_and_runs_it() {
    assert_movable_repair(REMOVED_AT_CREATE).await;
}

#[tokio::test]
async fn create_refusal_at_the_run_node_repairs_a_movable_slot_and_runs_it() {
    for code in ["unavailable", "invalid_argument"] {
        assert_movable_repair(CreateRefusal {
            on_node: true,
            code,
            ..REMOVED_AT_CREATE
        })
        .await;
    }
}

async fn assert_movable_repair(refusal: CreateRefusal) {
    let mut config = archon_core::config::Config::default();
    config.limits.infra_retries = 0;
    let h = Harness::with(config, None).await;
    remove_model_at_create(&h, refusal);
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    issue_admitted_slot(&h, &id).await;

    let store = h.ctx.store.lock().await;
    let item = store.item(&id).unwrap();
    assert_eq!(item.model_plan.0["implement"][0].model.to_string(), "p/new");
    let runs = store.runs_for_item(&id).unwrap();
    assert_eq!(runs.len(), 1);
    let stages = store.stage_runs(&runs[0].id).unwrap();
    assert_eq!(stages.len(), 2, "{stages:?}");
    let creates = h.fake.received_named("create_agent");
    assert!(
        stages
            .iter()
            .any(|s| s.state == StageState::Running && s.choice.model.to_string() == "p/new"),
        "{stages:?}, creates: {creates:?}"
    );
    assert_eq!(creates.len(), 3, "one refused create, run node, and stage");
    let pair = if refusal.on_node {
        &creates[..2]
    } else {
        &creates[1..]
    };
    assert_eq!(pair[0]["start"], pair[1]["start"]);
    assert_ne!(pair[0]["idempotency_key"], pair[1]["idempotency_key"]);
}

#[tokio::test]
async fn create_refusal_names_the_pin_and_keeps_the_failed_run() {
    let h = Harness::new().await;
    remove_model_at_create(&h, REMOVED_AT_CREATE);
    h.connect().await;
    let id = queued(&h, "p/old", false).await;
    issue_admitted_slot(&h, &id).await;

    let store = h.ctx.store.lock().await;
    let runs = store.runs_for_item(&id).unwrap();
    assert_eq!(runs.len(), 1);
    let RunState::Failed { error, .. } = &runs[0].state else {
        panic!("expected a failed run, got {:?}", runs[0].state);
    };
    assert!(
        error.contains("p/old") && error.contains("Re-plan"),
        "{error}"
    );
    assert_eq!(
        store.item(&id).unwrap().model_plan.0["implement"][0]
            .model
            .to_string(),
        "p/old"
    );
    assert!(
        store
            .model_catalog()
            .check(&"p/new".parse().unwrap())
            .is_ok()
    );
    assert_eq!(h.fake.received_named("create_agent").len(), 2);
}

#[tokio::test]
async fn create_refusal_does_not_repair_the_same_slot_twice() {
    let mut config = archon_core::config::Config::default();
    config.limits.infra_retries = 3;
    let h = Harness::with(config, None).await;
    remove_model_at_create(
        &h,
        CreateRefusal {
            count: 2,
            ..REMOVED_AT_CREATE
        },
    );
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    issue_admitted_slot(&h, &id).await;

    let store = h.ctx.store.lock().await;
    let runs = store.runs_for_item(&id).unwrap();
    assert_eq!(runs.len(), 1);
    let RunState::Failed { error, .. } = &runs[0].state else {
        panic!("expected a failed run, got {:?}", runs[0].state);
    };
    assert!(
        error.contains("p/new") && error.contains("already retried"),
        "{error}"
    );
    assert!(
        store
            .model_catalog()
            .check(&"p/last".parse().unwrap())
            .is_ok()
    );
    assert_eq!(h.fake.received_named("create_agent").len(), 3);
}

#[tokio::test]
async fn create_refusal_explains_a_failed_catalog_refresh() {
    let h = Harness::new().await;
    remove_model_at_create(
        &h,
        CreateRefusal {
            refresh_fails: true,
            ..REMOVED_AT_CREATE
        },
    );
    h.connect().await;
    let id = queued(&h, "p/old", true).await;
    issue_admitted_slot(&h, &id).await;

    let store = h.ctx.store.lock().await;
    let runs = store.runs_for_item(&id).unwrap();
    assert_eq!(runs.len(), 1);
    let RunState::Failed { error, .. } = &runs[0].state else {
        panic!("expected a failed run, got {:?}", runs[0].state);
    };
    assert!(
        error.contains("p/old") && error.contains("catalog offline"),
        "{error}"
    );
    assert_eq!(store.stage_runs(&runs[0].id).unwrap().len(), 1);
    assert_eq!(h.fake.received_named("create_agent").len(), 2);
}
