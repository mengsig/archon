//! Approved-intent gates apply only to operator-approved ledgers, and
//! a ledger item that slips is repaired or judged honestly, not burned.
use crate::common::{self, Harness};
use crate::intent_acceptance::{
    evidence, fixture_in, fixture_on, judgment, readiness, record, settle,
};
use archon_aether::fake::{FakeAether, Scripted};
use archon_core::acceptance::AcceptanceStatus;
use archon_core::config::Config;
use archon_core::intent::{ApprovedIntent, IntentBinding};
use archon_core::model::Effort;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{RecordRepair, StageOutcome, StageRun, StageState};
use archon_queue::store::keys;
use serde_json::json;
use sha2::Digest;

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

/// A running ledger item whose build stage also hands in the ship record
/// archond delivers; a refused run fails, with no automatic repair.
async fn ledger_delivery() -> (Harness, FakeAether, QueueItem) {
    let mut config = Config::default();
    config.review.auto_repair = false;
    let vision = [common::MODEL, common::REVIEW_MODEL];
    let mut plan = common::plan();
    plan.0.remove("ship");
    plan.0.get_mut("review").expect("planned").truncate(1);
    let pipeline = (common::REVIEWED, Some(common::REVIEWED_TOML), plan);
    fixture_in(config, pipeline, false, &vision).await
}

/// The running ledger build completes with `summary` and no `structured`.
async fn settle_bare(h: &Harness, summary: &str) {
    let outcome = StageOutcome::Completed {
        summary: summary.to_owned(),
        structured: None,
    };
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
}

/// A ledger build that hands in no `structured` at all has a broken delivery
/// record: it gets its repair turn in the same run before its requirement
/// results are checked, and the repaired result is checked as any other.
#[tokio::test]
async fn a_ledger_build_without_structured_gets_its_repair_turn_in_the_same_run() {
    let (h, _fake, item) = ledger_delivery().await;
    let first = h.running().await.remove(0);
    settle_bare(&h, "Built it.").await;
    let repair = h.running().await.remove(0);
    assert_eq!(repair.run, first.run, "the same run");
    assert_eq!(repair.stage_id, "implement");
    assert_eq!(repair.record_repair, Some(RecordRepair::Spent));
    let stages = h.stages(&item.id).await;
    let broken = stages.iter().find(|s| s.id == first.id).expect("the build");
    assert_eq!(broken.state, StageState::Completed, "settled as sent");
    assert_eq!(broken.record_repair, Some(RecordRepair::Open));
    // A record without the results the ledger asks for is refused as before.
    settle(&h, json!({})).await;
    let stages = h.stages(&item.id).await;
    let repaired = stages.iter().find(|s| s.id == repair.id).expect("it");
    let StageState::Failed { error } = &repaired.state else {
        panic!("the repaired result is verified: {:?}", repaired.state)
    };
    assert!(error.contains("requirement_results refused"), "{error}");
}

/// A ledger build that states a failure with no `structured` settles as its
/// agent sent it: no requirement check rewrites the escalation into a
/// refusal, so nothing repairs it and its stage fails on what it said.
#[tokio::test]
async fn a_ledger_build_that_states_a_failure_settles_as_sent() {
    let (h, _fake, item) = ledger_delivery().await;
    let first = h.running().await.remove(0);
    let said = "ESCALATE: the ledger needs a schema migration first";
    settle_bare(&h, said).await;
    let stages = h.stages(&item.id).await;
    let stated = stages.iter().find(|s| s.id == first.id).expect("the build");
    assert_eq!(stated.state, StageState::Completed, "settled as sent");
    assert_eq!(stated.summary.as_deref(), Some(said));
    let item = h.item(&item.id).await;
    let QueueState::Failed { error, .. } = &item.state else {
        panic!("a stated failure fails its run: {:?}", item.state)
    };
    assert!(error.contains("stated a failure"), "{error}");
    assert!(!error.contains("requirement_results"), "{error}");
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

/// Whether ApproveIntent of `ledger`, quoting inbox message 7, is refused
/// with an error containing `refusal`; `None` approves it.
async fn approval(h: &Harness, ledger: &ApprovedIntent, refusal: Option<&str>) {
    let approved = h.orchestrator(Request::ApproveIntent(ledger.clone())).await;
    let binding = IntentBinding {
        id: ledger.id.clone(),
        version: 1,
        requirements: vec![],
    };
    let stored = h.ctx.store.lock().await.approved_intent(&binding).unwrap();
    match refusal {
        None => assert!(approved.is_ok() && stored.is_some(), "{approved:?}"),
        Some(refusal) => {
            let error = approved.unwrap_err().to_string();
            assert!(error.contains(refusal), "{error}");
            assert!(stored.is_none(), "a refused ledger stores nothing");
        }
    }
}

#[tokio::test]
async fn only_an_image_the_operator_attached_makes_an_inbox_ledger() {
    let h = Harness::new().await;
    let mut ledger = record(&h);
    let written = ledger.requirements.clone();
    ledger.requirements.clear();
    let context = ledger.images[0].clone();
    ledger.images[0] = archon_core::intent::ReferenceImage {
        viewport: None,
        theme: None,
        state: None,
        ..context.clone()
    };
    operator_said(&h, 7, &ledger.operator_words, &[]).await;
    let unattached = Some("is not an image the operator attached");
    approval(&h, &ledger, unattached).await;
    let path = ledger.images[0].path.display().to_string();
    ledger.operator_words = format!("Keep ISO labels visible, as `{path}`.");
    operator_said(&h, 7, &ledger.operator_words, &[]).await;
    approval(&h, &ledger, unattached).await;
    let digest = ledger.images[0].sha256.clone();
    operator_said(&h, 7, &ledger.operator_words, &[&digest]).await;
    let mut mislabelled = ledger.clone();
    mislabelled.requirements = written;
    mislabelled.requirements[0].text = "Rewrite the login flow".into();
    approval(&h, &mislabelled, Some("\"R1\" is written by the caller")).await;
    let mut invented = ledger.clone();
    invented.images[0] = context;
    let unproven = Some("\"mockup\" carries a viewport, theme or state");
    approval(&h, &invented, unproven).await;
    let other = "c".repeat(64);
    operator_said(&h, 7, &ledger.operator_words, &[&digest, &other]).await;
    let omitted = Some("attached 2 image(s) and the record lists 1");
    approval(&h, &ledger, omitted).await;
    operator_said(&h, 7, &ledger.operator_words, &[&digest]).await;
    approval(&h, &ledger, None).await;
    let stored = h.ctx.store.lock().await.approved_intent(&IntentBinding {
        id: ledger.id.clone(),
        version: 1,
        requirements: vec![],
    });
    let requirements = stored.unwrap().unwrap().requirements;
    assert_eq!(requirements.len(), 1);
    assert_eq!(
        (
            requirements[0].id.as_str(),
            requirements[0].images.as_slice()
        ),
        ("mockup", ["mockup".to_owned()].as_slice())
    );
}

#[tokio::test]
async fn an_inbox_artifact_is_refused_even_when_the_operator_names_it() {
    let h = Harness::new().await;
    let mut ledger = record(&h);
    ledger.requirements.clear();
    (
        ledger.images[0].viewport,
        ledger.images[0].theme,
        ledger.images[0].state,
    ) = (None, None, None);
    ledger.id = "page".into();
    let page = b"<!doctype html><title>Approved</title>";
    let path = h.dir.path().join("design.html");
    std::fs::write(&path, page).unwrap();
    let digest = sha2::Sha256::digest(page)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    ledger.artifacts.push(archon_core::intent::IntentArtifact {
        path: path.clone(),
        sha256: digest,
    });
    ledger.operator_words = format!("Keep ISO labels visible per {}", path.display());
    let image = ledger.images[0].sha256.clone();
    operator_said(&h, 7, &ledger.operator_words, &[&image]).await;
    approval(&h, &ledger, Some("design.html is not an attachment")).await;
}

/// An inbox ledger without images whose requirements are `texts`.
fn listed(words: &str, texts: &[&str]) -> ApprovedIntent {
    let requirements: Vec<_> = texts.iter().enumerate().map(|(i, text)| {
        json!({"id":format!("R{}", i + 1),"images":[],"kind":"behaviour","owner":"operator","text":text})
    }).collect();
    serde_json::from_value(
        json!({"id":"listed","version":1,"source":{"kind":"inbox","seq":7},
        "operator_words":words,"artifacts":[],"images":[],"requirements":requirements}),
    )
    .unwrap()
}

#[tokio::test]
async fn a_prose_requirements_list_creates_no_ledger() {
    let h = Harness::new().await;
    let words = "Approve these requirements:\n- Add caching\n- Keep the API";
    operator_said(&h, 7, words, &[]).await;
    let whole = listed(words, &["Add caching", "Keep the API"]);
    approval(&h, &whole, Some("carries no attached reference image")).await;
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

/// Another model of the builder's vendor, and one of another vendor.
const SAME_VENDOR: &str = "claude-code/opus-pin";
const OTHER_VENDOR: &str = "codex/gpt-6-luna";

/// A ledger item whose plan pins `pin` for acceptance, Aether listing every
/// model of `vision` with vision, run until its review is the one running.
async fn pinned_at_review(
    config: Config,
    pin: &str,
    vision: &[&str],
) -> (Harness, FakeAether, QueueItem) {
    let mut plan = common::plan();
    plan.0.insert(
        "intent-acceptance".into(),
        vec![common::choice(pin, Effort::High)],
    );
    let (h, fake, item) = fixture_in(config, (common::PIPELINE, None, plan), false, vision).await;
    settle(&h, evidence(&h, &item, "implemented").await).await;
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
    (h, fake, item)
}

/// Aether's reading that each of `providers` has no usage left.
fn spent_providers(h: &Harness, providers: &[&str]) {
    reading(h, &providers.iter().map(|p| (*p, 0)).collect::<Vec<_>>());
}

/// Aether's reading of each provider's remaining usage, in percent.
fn reading(h: &Harness, providers: &[(&str, u64)]) {
    use archon_core::quota::{ProviderHeadroom, QuotaWindow};
    let headroom = |remaining_percent| ProviderHeadroom {
        windows: vec![QuotaWindow {
            name: "5h".into(),
            window_minutes: Some(300),
            remaining_percent,
            resets_at: Some(archon_core::now() + chrono::TimeDelta::hours(2)),
            resets_in_seconds: None,
        }],
        observed_at: Some(archon_core::now()),
        age_seconds: Some(0),
        stale: false,
    };
    h.ctx.quota.set_reading(archon_daemon::quota::Reading {
        read_at: archon_core::now(),
        providers: providers
            .iter()
            .map(|(p, left)| ((*p).to_owned(), headroom(*left)))
            .collect(),
    });
}

fn no_repair() -> Config {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config
}

#[tokio::test]
async fn a_pinned_acceptance_model_judges_and_is_labelled_when_it_shares_the_builder_s_vendor() {
    let vision = [common::MODEL, common::REVIEW_MODEL, SAME_VENDOR];
    let (h, _fake, item) = pinned_at_review(no_repair(), SAME_VENDOR, &vision).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(running[0].stage_id, "intent-acceptance");
    assert_eq!(
        running[0].choice.model.to_string(),
        SAME_VENDOR,
        "the pin, not another vendor's model"
    );
    assert_eq!(running[0].choice.effort, Effort::High);
    settle(&h, judgment(&h, &item, "pass").await).await;
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
    assert_eq!(record.model.to_string(), SAME_VENDOR);
    assert!(!record.independent, "the builder's vendor is labelled");
}

#[tokio::test]
async fn independent_acceptance_refuses_a_pin_that_shares_the_builder_s_vendor() {
    let mut config = no_repair();
    config.review.independent_acceptance = true;
    let vision = [common::MODEL, common::REVIEW_MODEL, SAME_VENDOR];
    let (h, _fake, item) = pinned_at_review(config, SAME_VENDOR, &vision).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the strict knob refuses the pin")
    };
    assert!(error.contains("plan pins"), "{error}");
    assert!(error.contains(SAME_VENDOR), "{error}");
}

#[tokio::test]
async fn independent_acceptance_keeps_a_pin_of_another_vendor() {
    let mut config = no_repair();
    config.review.independent_acceptance = true;
    let vision = [common::MODEL, common::REVIEW_MODEL, OTHER_VENDOR];
    let (h, _fake, _item) = pinned_at_review(config, OTHER_VENDOR, &vision).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let running = h.running().await;
    assert_eq!(running[0].choice.model.to_string(), OTHER_VENDOR);
}

#[tokio::test]
async fn a_judge_whose_provider_is_out_of_quota_is_skipped() {
    let vision = [common::MODEL, common::REVIEW_MODEL, SAME_VENDOR];
    let (h, _fake, item) = pinned_at_review(no_repair(), common::REVIEW_MODEL, &vision).await;
    spent_providers(&h, &["codex"]);
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(running[0].stage_id, "intent-acceptance");
    assert_eq!(
        running[0].choice.model.to_string(),
        SAME_VENDOR,
        "the pin's provider has no usage left"
    );
}

#[tokio::test]
async fn an_acceptance_retry_leaves_the_provider_that_hit_its_usage_limit() {
    let vision = [common::MODEL, common::REVIEW_MODEL, SAME_VENDOR];
    let (h, _fake, item) = pinned_at_review(no_repair(), common::REVIEW_MODEL, &vision).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let first = h.running().await;
    assert_eq!(first[0].stage_id, "intent-acceptance");
    assert_eq!(first[0].choice.model.to_string(), common::REVIEW_MODEL);
    h.settle_running(|_| StageOutcome::Failed {
        error: "usage limit reached; try again at Nov 24th 4:07 PM".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    let retry = h.running().await;
    assert_eq!(retry.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(retry[0].stage_id, "intent-acceptance");
    assert_eq!(retry[0].choice.model.to_string(), SAME_VENDOR);
}

#[tokio::test]
async fn acceptance_with_every_judge_out_of_quota_fails_with_a_clear_error() {
    let vision = [common::MODEL, common::REVIEW_MODEL];
    let (h, _fake, item) = pinned_at_review(no_repair(), common::REVIEW_MODEL, &vision).await;
    spent_providers(&h, &["codex", "claude-code"]);
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("no judge can run: {:?}", h.state(&item.id).await)
    };
    assert!(error.contains("no eligible acceptance judge"), "{error}");
    assert!(error.contains("out of quota"), "{error}");
    assert!(
        error.contains("codex") && error.contains("claude-code"),
        "{error}"
    );
}

const USAGE_LIMIT: &str = "usage limit reached; try again at Nov 24th 4:07 PM";

/// Fail the running acceptance attempt on a usage limit and dispatch the
/// retry: the stage that runs now, which is a new attempt, not the failed one.
async fn retry_after_usage_limit(h: &Harness) -> Vec<StageRun> {
    let failed: Vec<_> = h.running().await.iter().map(|s| s.id.clone()).collect();
    h.settle_running(|_| StageOutcome::Failed {
        error: USAGE_LIMIT.into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    let retry: Vec<_> = h
        .running()
        .await
        .into_iter()
        .filter(|s| s.stage_id == "intent-acceptance")
        .collect();
    assert!(
        retry.iter().all(|s| !failed.contains(&s.id)),
        "a new attempt runs, not the failed one"
    );
    retry
}

#[tokio::test]
async fn a_lone_judge_that_hit_its_usage_limit_is_retried() {
    let vision = [common::MODEL, common::REVIEW_MODEL];
    let (h, _fake, _item) = pinned_at_review(no_repair(), common::REVIEW_MODEL, &vision).await;
    // The reading shows the builder's provider spent: codex is the only judge.
    spent_providers(&h, &["claude-code"]);
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    assert_eq!(
        h.running().await[0].choice.model.to_string(),
        common::REVIEW_MODEL
    );
    let retry = retry_after_usage_limit(&h).await;
    assert_eq!(
        retry.len(),
        1,
        "no other judge can run, so the lone one does"
    );
    assert_eq!(retry[0].choice.model.to_string(), common::REVIEW_MODEL);
}

#[tokio::test]
async fn a_quota_reading_overrides_the_failure_history() {
    let vision = [common::MODEL, common::REVIEW_MODEL, SAME_VENDOR];
    let (h, _fake, item) = pinned_at_review(no_repair(), common::REVIEW_MODEL, &vision).await;
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    assert_eq!(
        h.running().await[0].choice.model.to_string(),
        common::REVIEW_MODEL
    );
    // As the attempt fails Aether reads usage left on codex; without the
    // reading the failure would move the retry to the other provider, as in
    // the test above.
    reading(&h, &[("codex", 80)]);
    let retry = retry_after_usage_limit(&h).await;
    assert_eq!(retry.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(
        retry[0].choice.model.to_string(),
        common::REVIEW_MODEL,
        "the reading shows usage left, so the failure does not move the judge"
    );
}
