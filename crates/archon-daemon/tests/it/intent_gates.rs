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

/// Inbox message `seq` the orchestrator received from the operator, as the
/// pump records it: its words and the digests of its attached images.
async fn operator_said(h: &Harness, seq: u64, words: &str, images: &[&str]) {
    let mut store = h.ctx.store.lock().await;
    store
        .kv_set(keys::ORCHESTRATOR_AGENT, common::ORCHESTRATOR)
        .unwrap();
    let inbox = format!("intent_inbox:{}:{seq}", common::ORCHESTRATOR);
    store.kv_set(&inbox, words).unwrap();
    if !images.is_empty() {
        let attached = format!("intent_inbox_images:{}:{seq}", common::ORCHESTRATOR);
        store.kv_set(&attached, &json!(images).to_string()).unwrap();
    }
}

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
    operator_said(&h, 2, BRIEF, &[]).await;
    let mut excerpt = invented_ledger();
    excerpt.requirements.truncate(1);
    excerpt.requirements[0].text = "add focused tests".into();
    for ledger in [invented_ledger(), excerpt] {
        let refused = h.orchestrator(Request::ApproveIntent(ledger)).await;
        let error = refused.unwrap_err().to_string();
        assert!(
            error.contains("a plain brief creates no approved intent"),
            "{error}"
        );
    }
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
    assert!(
        repaired
            .brief
            .contains("requirement_results refused: missing")
    );
    let vocabulary = "exactly one of `implemented`, `deviation` or `blocked`";
    assert!(repaired.brief.contains(vocabulary), "{}", repaired.brief);
    assert!(build.instructions.contains("never `pass` or `fail`"));
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

#[tokio::test]
async fn only_an_image_the_operator_attached_makes_an_approved_reference() {
    let h = Harness::new().await;
    let ledger = record(&h);
    let binding = IntentBinding {
        id: ledger.id.clone(),
        version: 1,
        requirements: vec![],
    };
    operator_said(&h, 7, &ledger.operator_words, &[]).await;
    let refused = h
        .orchestrator(Request::ApproveIntent(ledger.clone()))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("neither attached to nor named"),
        "{refused}"
    );
    let stored = h.ctx.store.lock().await.approved_intent(&binding).unwrap();
    assert!(
        stored.is_none(),
        "an unrelated local image approves nothing"
    );
    operator_said(&h, 7, &ledger.operator_words, &[&ledger.images[0].sha256]).await;
    h.orchestrator(Request::ApproveIntent(ledger))
        .await
        .unwrap();
    let stored = h.ctx.store.lock().await.approved_intent(&binding).unwrap();
    assert_eq!(stored.expect("approved").images.len(), 1);
}

/// A change that spoils one requirement result.
type Spoiler = fn(&mut serde_json::Value);

/// `results` as the build hands them in, after `change` spoils them.
async fn spoiled(h: &Harness, item: &QueueItem, change: Spoiler) -> serde_json::Value {
    let mut results = evidence(h, item, "implemented").await;
    change(&mut results["requirement_results"][0]);
    results
}

#[tokio::test]
async fn empty_evidence_or_a_bad_visual_pair_gets_the_same_repair() {
    let spoilers: [(Spoiler, &str); 3] = [
        (
            |r| r["evidence"] = json!(" "),
            "R1 has no evidence or reason",
        ),
        (
            |r| r["visual_pairs"] = json!([]),
            "R1 needs a reference | built pair",
        ),
        (
            |r| r["visual_pairs"][0]["built"] = json!("missing.png"),
            "R1 built image missing.png",
        ),
    ];
    for (change, refusal) in spoilers {
        let models = [common::MODEL, common::REVIEW_MODEL];
        let (h, _fake, item) = fixture_on(Config::default(), false, &models).await;
        settle(&h, spoiled(&h, &item, change).await).await;
        let repaired = h.item(&item.id).await;
        assert!(
            matches!(repaired.state, QueueState::Running { .. }),
            "{refusal}: {:?}",
            repaired.state
        );
        assert!(
            repaired.brief.contains("Automatic repair"),
            "{}",
            repaired.brief
        );
        assert!(repaired.brief.contains(refusal), "{}", repaired.brief);
        assert_eq!(h.running().await[0].stage_id, "implement");
    }
}

const VALIDATION: &str = r#"[pipeline]
name = "validate"
description = "judge an existing head against approved intent"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "check"
kind = "review"
prompt = "implement.md"
fan_out = 1
gate = "all_pass"
timeout_minutes = 30
"#;

#[tokio::test]
async fn a_validation_only_acceptance_stage_owes_and_repairs_its_results() {
    let h = Harness::with_config(Config::default()).await;
    let _fake =
        crate::intent_acceptance::models_listed(&h, &[common::MODEL, common::REVIEW_MODEL]).await;
    h.add_pipeline("validate", VALIDATION);
    let ledger = record(&h);
    h.ctx
        .store
        .lock()
        .await
        .save_approved_intent(&ledger)
        .unwrap();
    let binding = IntentBinding {
        id: ledger.id.clone(),
        version: 1,
        requirements: vec![],
    };
    let mut plan = archon_core::model::ModelPlan::default();
    for (stage, effort) in [
        ("check", Effort::Medium),
        ("intent-acceptance", Effort::High),
    ] {
        plan.0.insert(
            stage.into(),
            vec![common::choice(common::REVIEW_MODEL, effort)],
        );
    }
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":"validate","title":"judge the head","brief":"Judge the approved ledger","intent":binding,"model_plan":plan})).unwrap();
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("item")
    };
    for attempt in 0..2 {
        h.dispatch().await;
        assert_eq!(h.running().await[0].stage_id, "check");
        settle(&h, json!({"verdict":"pass","findings":[]})).await;
        assert_eq!(h.running().await[0].stage_id, "intent-acceptance");
        let judge = h.backend.requests().pop().unwrap();
        assert!(
            judge
                .instructions
                .contains("## Requirement results this stage owes")
        );
        assert!(judge.instructions.contains("\"id\": \"R1\""));
        let mut result = judgment(&h, &item, "pass").await;
        if attempt == 1 {
            result["requirement_results"] =
                evidence(&h, &item, "implemented").await["requirement_results"].take();
        }
        settle(&h, result).await;
        if attempt == 0 {
            let repaired = h.item(&item.id).await;
            assert!(
                matches!(repaired.state, QueueState::Running { .. }),
                "{:?}",
                repaired.state
            );
            assert!(
                repaired
                    .brief
                    .contains("requirement_results refused: missing"),
                "{}",
                repaired.brief
            );
            assert!(
                repaired.brief.contains("Change no tracked file"),
                "{}",
                repaired.brief
            );
        }
    }
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
}
