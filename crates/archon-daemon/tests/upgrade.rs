//! Items stored by Archon 1.0 name a `ship` stage in their model plan and
//! reasons. The built-ins ship through archond now, so a queued or retried
//! 1.0 item drops that entry when it is read for a run; a run already going
//! keeps its frozen pipeline, ship agent included, even when 1.1.0 removed
//! its built-in, and custom pipelines are left as their authors wrote them.

mod common;

use std::collections::BTreeMap;

use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_daemon::executor;
use common::{Harness, MODEL, REVIEW_MODEL, choice, completed, verdict};

/// A built-in that 1.0.1 ended in a ship agent and 1.1.0 keeps.
const BUILTIN: &str = "refactor";
/// A built-in 1.1.0 removed.
const RETIRED: &str = "default-pr-review";

/// The 1.0.1 `default-pr-review`, which ended in a ship agent.
const V101_PIPELINE: &str = r#"[pipeline]
name = "default-pr-review"
description = "Implement, one independent review with a bounded fix loop, then ship"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 120

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
gate = "all_pass"
timeout_minutes = 60
on_fail = { goto = "implement", max_loops = 2 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

/// What 1.0.1 stored for a `refactor` item: a choice and a reason for every
/// stage, its ship agent's included.
fn v101_plan() -> ModelPlan {
    ModelPlan(BTreeMap::from([
        ("baseline".into(), vec![choice(MODEL, Effort::Medium)]),
        ("refactor".into(), vec![choice(MODEL, Effort::Medium)]),
        ("verify".into(), vec![choice(REVIEW_MODEL, Effort::Medium)]),
        ("review".into(), vec![choice(REVIEW_MODEL, Effort::Medium)]),
        ("ship".into(), vec![choice(MODEL, Effort::Low)]),
    ]))
}

/// What 1.0.1 stored for a `default-pr-review` item.
fn v101_retired_plan() -> ModelPlan {
    ModelPlan(BTreeMap::from([
        ("implement".into(), vec![choice(MODEL, Effort::Medium)]),
        ("review".into(), vec![choice(REVIEW_MODEL, Effort::Medium)]),
        ("ship".into(), vec![choice(MODEL, Effort::Low)]),
    ]))
}

fn v101_reasons() -> BTreeMap<String, String> {
    v101_plan()
        .0
        .into_keys()
        .map(|stage| (stage.to_owned(), format!("why {stage}")))
        .collect()
}

/// An item on the built-in, rewritten in the store to what 1.0.1 left there.
async fn v101_item(h: &Harness, title: &str) -> QueueItem {
    let mut plan = v101_plan();
    plan.0.remove("ship");
    let mut item = h.enqueue_in(BUILTIN, title, plan).await.expect("enqueue");
    item.model_plan = v101_plan();
    item.model_reasons = Some(v101_reasons());
    h.ctx
        .store
        .lock()
        .await
        .update_item(&item)
        .expect("1.0.1 shape");
    item
}

#[tokio::test]
async fn a_queued_1_0_item_starts_on_the_built_in_without_its_ship_stage() {
    let h = Harness::new().await;
    let item = v101_item(&h, "queued before the upgrade").await;

    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(running[0].stage_id, "baseline");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    let stored = h.item(&item.id).await;
    assert_eq!(
        stored.model_plan,
        v101_plan(),
        "the repair is made as the item is read, not written back"
    );
}

#[tokio::test]
async fn a_retried_1_0_item_keeps_its_plan_without_the_ship_stage() {
    let h = Harness::new().await;
    let item = v101_item(&h, "failed before the upgrade").await;
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
    };
    h.cli(retry).await.expect("a 1.0 item is retried");
    let stored = h.item(&item.id).await;
    let mut expected = v101_plan();
    expected.0.remove("ship");
    assert_eq!(stored.model_plan, expected, "the 1.0 choices are kept");
    let reasons = stored.model_reasons.expect("reasons kept");
    assert_eq!(
        reasons.keys().collect::<Vec<_>>(),
        ["baseline", "refactor", "review", "verify"],
        "the ship stage's reason went with it"
    );
    let repo = stored.repo.as_ref().expect("a repository item");
    let key = format!("learning/static/{}/{BUILTIN}", repo.as_str());
    let baseline = h
        .kv(&key)
        .await
        .expect("routing measures against a baseline");
    let baseline: ModelPlan = serde_json::from_str(&baseline).expect("a plan");
    assert_eq!(
        baseline.0.keys().collect::<Vec<_>>(),
        ["baseline", "refactor", "review", "verify"],
        "routing's baseline has no retired ship stage either"
    );
    h.dispatch().await;
    assert_eq!(h.running().await[0].stage_id, "baseline");
}

#[tokio::test]
async fn a_run_started_by_1_0_finishes_with_its_frozen_ship_agent() {
    let mut h = Harness::new().await;
    h.add_pipeline(RETIRED, V101_PIPELINE);
    let item = h
        .enqueue_in(RETIRED, "running across the upgrade", v101_retired_plan())
        .await
        .expect("enqueue on the 1.0.1 pipeline");
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "the implementer runs");
    // The upgrade: the 1.0.1 pipeline is gone, and so is its built-in.
    let dir = h.repo.join(".archon/pipelines").join(RETIRED);
    std::fs::remove_dir_all(dir).expect("upgrade the pipeline");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    let ship = h.running().await;
    assert_eq!(ship.len(), 1, "the frozen run still ships by agent");
    assert_eq!(ship[0].stage_id, "ship");
    assert_eq!(ship[0].choice, choice(MODEL, Effort::Low));
    h.settle_running(|_| completed("shipped")).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.item(&item.id).await.model_plan, v101_retired_plan());
}

#[tokio::test]
async fn a_queued_1_0_item_on_a_removed_built_in_fails_naming_its_replacement() {
    let h = Harness::new().await;
    h.add_pipeline(RETIRED, V101_PIPELINE);
    let item = h
        .enqueue_in(RETIRED, "queued across the upgrade", v101_retired_plan())
        .await
        .expect("enqueue on the 1.0.1 pipeline");
    // The upgrade: the 1.0.1 pipeline is gone before the item started.
    let dir = h.repo.join(".archon/pipelines").join(RETIRED);
    std::fs::remove_dir_all(dir).expect("upgrade the pipeline");

    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("an item on a removed built-in cannot start");
    };
    assert!(error.contains("direct with review standard"), "{error}");
    assert!(h.running().await.is_empty());
}

/// A repository pipeline without a ship stage, as an operator may write.
const CUSTOM: &str = r#"[pipeline]
name = "custom-one"
description = "one stage of the operator's own"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30
"#;

#[tokio::test]
async fn a_custom_pipeline_s_plan_naming_ship_is_not_repaired() {
    let h = Harness::new().await;
    h.add_pipeline("custom-one", CUSTOM);
    let mut plan = v101_retired_plan();
    plan.0.retain(|stage, _| stage == "implement");
    let item = h
        .enqueue_in("custom-one", "custom", plan)
        .await
        .expect("enqueue");
    let mut stored = h.item(&item.id).await;
    stored.model_plan = v101_retired_plan();
    stored.model_plan.0.remove("review");
    h.ctx.store.lock().await.update_item(&stored).expect("plan");

    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("a plan naming a stage the custom pipeline lacks fails the start");
    };
    assert!(error.contains("\"ship\""), "{error}");
    assert!(h.running().await.is_empty());
}
