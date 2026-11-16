//! Approved-intent gates apply only to operator-approved ledgers, and
//! a ledger item that slips is repaired or judged honestly, not burned.
use crate::common::{self, Harness};
use crate::intent_acceptance::{evidence, fixture_on, judgment, readiness, record, settle};
use archon_aether::fake::{FakeAether, Scripted};
use archon_core::acceptance::AcceptanceStatus;
use archon_core::config::Config;
use archon_core::intent::{ApprovedIntent, IntentBinding};
use archon_core::model::Effort;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::StageState;
use archon_queue::store::keys;
use serde_json::json;

/// The release smoke's brief, as the operator sent it.
const BRIEF: &str = "Implement `retry_budget.py` according to `SPEC.md`. Preserve the documented \
                     API, handle every specified error and boundary case, and add focused tests. \
                     The existing tests cover basics only. Use the Python standard library and \
                     commit your work.";

/// The ledger the smoke's orchestrator wrote itself from [`BRIEF`].
fn invented_ledger() -> ApprovedIntent {
    serde_json::from_value(json!({"id":"intent-retry-budget-v1","version":1,
        "source":{"kind":"inbox","seq":2},"operator_words":BRIEF,"artifacts":[],"images":[],
        "requirements":[
            {"id":"R1","images":[],"kind":"behaviour","owner":"operator","text":"Implement retry_delay exactly per SPEC.md: signature, keyword-only args, defaults, return int or None."},
            {"id":"R2","images":[],"kind":"behaviour","owner":"operator","text":"Validate all arguments before any early return; ints only (no bool/float)."}]}))
    .unwrap()
}

#[tokio::test]
async fn the_release_smoke_plain_brief_on_one_model_runs_with_no_intent_gate() {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.models.allow = vec![common::MODEL.parse().unwrap()];
    let h = Harness::with_config(config).await;
    let fake = common::connect_aether_for_delegations(&h).await;
    let listed = json!([{"model":common::MODEL,"configured":true,"tier":"strong","capabilities":{"vision":true}}]);
    fake.set_default("list_models", Scripted::reply("models", listed));
    {
        let mut store = h.ctx.store.lock().await;
        store
            .kv_set(keys::ORCHESTRATOR_AGENT, common::ORCHESTRATOR)
            .unwrap();
        let inbox = format!("intent_inbox:{}:2", common::ORCHESTRATOR);
        store.kv_set(&inbox, BRIEF).unwrap();
    }
    let refused = h
        .orchestrator(Request::ApproveIntent(invented_ledger()))
        .await;
    let error = refused.unwrap_err().to_string();
    assert!(
        error.contains("a plain brief creates no approved intent"),
        "{error}"
    );
    let mut plan = common::plan();
    for choices in plan.0.values_mut() {
        *choices = vec![common::choice(common::MODEL, Effort::Medium)];
    }
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":common::PIPELINE,"title":"retry budget","brief":BRIEF,"model_plan":plan})).unwrap();
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("item")
    };
    assert!(item.intent.is_none());
    assert!(!item.model_plan.0.contains_key("intent-acceptance"));
    h.dispatch().await;
    let build = h.backend.requests().pop().unwrap();
    assert!(!build.instructions.contains("requirement_results"));
    settle(&h, json!({})).await;
    let stages = h.stages(&item.id).await;
    assert!(
        stages
            .iter()
            .all(|s| !matches!(s.state, StageState::Failed { .. })),
        "{stages:?}"
    );
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let stages = h.stages(&item.id).await;
    assert!(
        stages.iter().all(|s| s.stage_id != "intent-acceptance"),
        "{stages:?}"
    );
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::NotRequired);
}

#[tokio::test]
async fn a_ledger_build_without_requirement_results_gets_one_clear_repair() {
    let (h, _fake, item) = fixture_on(
        Config::default(),
        false,
        &[common::MODEL, common::REVIEW_MODEL],
    )
    .await;
    let build = h.backend.requests().pop().unwrap();
    assert!(
        build
            .instructions
            .contains("## Requirement results this stage owes")
    );
    assert!(build.instructions.contains("\"requirement_results\""));
    assert!(build.instructions.contains("\"id\": \"R1\""));
    assert!(build.instructions.contains("\"reference\": \"mockup\""));
    settle(&h, json!({})).await;
    let repaired = h.item(&item.id).await;
    assert!(
        matches!(repaired.state, QueueState::Running { .. }),
        "{:?}",
        repaired.state
    );
    assert!(
        repaired.brief.contains("Automatic repair"),
        "{}",
        repaired.brief
    );
    assert!(repaired.brief.contains("missing requirement_results"));
    assert!(
        repaired
            .brief
            .contains("Requirement results this stage owes")
    );
    let retry = h.backend.requests().pop().unwrap();
    assert!(
        retry
            .instructions
            .contains("## Requirement results this stage owes")
    );
    assert_eq!(h.running().await[0].stage_id, "implement");
    settle(&h, evidence(&h, &item, "implemented").await).await;
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
}

/// A ledger item at its acceptance stage, Aether listing only the builder's model.
async fn alone_at_acceptance(config: Config) -> (Harness, FakeAether, QueueItem) {
    let (h, fake, item) = fixture_on(config, false, &[common::MODEL]).await;
    settle(&h, evidence(&h, &item, "implemented").await).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    (h, fake, item)
}

#[tokio::test]
async fn visual_acceptance_without_another_vision_model_falls_back_labelled() {
    let mut config = Config::default();
    config.review.auto_repair = false;
    let (h, _fake, item) = alone_at_acceptance(config).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(running[0].stage_id, "intent-acceptance");
    assert_eq!(running[0].choice.model.to_string(), common::MODEL);
    settle(&h, judgment(&h, &item, "pass").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    let record = detail.acceptance.record.expect("acceptance on record");
    assert!(
        !record.independent,
        "a builder's model never claims independence"
    );
    assert_eq!(record.model.to_string(), common::MODEL);
}

#[tokio::test]
async fn independent_acceptance_knob_keeps_the_strict_refusal() {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.review.independent_acceptance = true;
    let (h, _fake, item) = alone_at_acceptance(config).await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the strict knob refuses acceptance on the builder's model")
    };
    assert!(
        error.contains("independent acceptance of approved reference images"),
        "{error}"
    );
}

#[tokio::test]
async fn an_empty_requirement_selection_is_stored_spelled_out() {
    let (h, _fake, item) = fixture_on(Config::default(), false, &[common::MODEL]).await;
    let binding = item.intent.as_deref().expect("bound");
    assert_eq!(binding.requirements, vec!["R1".to_owned()]);
    let unknown = IntentBinding {
        id: binding.id.clone(),
        version: 1,
        requirements: vec!["R9".into()],
    };
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":common::PIPELINE,"title":"other","brief":"x","intent":unknown,"model_plan":common::plan()})).unwrap();
    let refused = h
        .cli(Request::Enqueue(request))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("unknown or duplicate requirement"),
        "{refused}"
    );
    assert_eq!(record(&h).requirements.len(), 1);
}
