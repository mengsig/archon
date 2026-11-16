use crate::common::{self, Harness};
use archon_aether::fake::{FakeAether, Scripted};
use archon_core::acceptance::AcceptanceStatus;
use archon_core::autonomy::{ClarificationPolicy, Directive, DirectiveSource};
use archon_core::intent::{ApprovedIntent, IntentBinding};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::run::{StageOutcome, StageState};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

async fn fixture(references: bool) -> (Harness, FakeAether, QueueItem) {
    let mut config = archon_core::config::Config::default();
    config.review.auto_repair = false;
    fixture_on(config, references, &[common::MODEL, common::REVIEW_MODEL]).await
}

/// A running ledger item under `config`, Aether listing `vision` models.
pub(crate) async fn fixture_on(
    mut config: archon_core::config::Config,
    references: bool,
    vision: &[&str],
) -> (Harness, FakeAether, QueueItem) {
    config.worktrees.keep_on_success = true;
    let h = Harness::with_config(config).await;
    let fake = common::connect_aether_for_delegations(&h).await;
    if references {
        fake.set_features(vec!["references".into()]);
        let client = archon_aether::AetherClient::connect(&fake.endpoint())
            .await
            .unwrap();
        h.ctx
            .aether
            .replace(Some(std::sync::Arc::new(client)))
            .unwrap();
    }
    let models: Vec<_> = vision.iter().map(|model| {
        json!({"model":model.parse::<archon_core::model::ModelRef>().unwrap(),"configured":true,"tier":"strong","capabilities":{"vision":true}})
    }).collect();
    fake.set_default("list_models", Scripted::reply("models", json!(models)));
    let intent = record(&h);
    let binding = IntentBinding {
        id: intent.id.clone(),
        version: 1,
        requirements: vec![],
    };
    h.ctx
        .store
        .lock()
        .await
        .save_approved_intent(&intent)
        .unwrap();
    let mut plan = common::plan();
    plan.0.insert(
        "intent-acceptance".into(),
        vec![common::choice(
            common::REVIEW_MODEL,
            archon_core::model::Effort::High,
        )],
    );
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":common::PIPELINE,"title":"preserve approved design","brief":"Use the approved ledger","intent":binding,"model_plan":plan})).unwrap();
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("item")
    };
    h.dispatch().await;
    assert_eq!(h.running().await[0].stage_id, "implement");
    assert!(item.model_plan.0.contains_key("intent-acceptance"));
    (h, fake, item)
}

pub(crate) fn record(h: &Harness) -> ApprovedIntent {
    let path = h.dir.path().join("reference.png");
    let bytes = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7ioAAAAASUVORK5CYII=").unwrap();
    let bytes = match std::env::var_os("ARCHON_INTENT_EVIDENCE_DIR") {
        Some(dir) => std::fs::read(std::path::PathBuf::from(dir).join("reference.png")).unwrap(),
        None => bytes,
    };
    std::fs::write(&path, &bytes).unwrap();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    serde_json::from_value(json!({"id":"approved_layout","version":1,"source":{"kind":"inbox","seq":7},"operator_words":"Keep ISO labels visible","artifacts":[],"images":[{"id":"mockup","path":path,"sha256":hash,"viewport":viewport(),"theme":"light","state":"loaded"}],"requirements":[{"id":"R1","text":"Keep ISO labels visible","kind":"visual","owner":"demo","images":["mockup"]}]})).unwrap()
}

pub(crate) async fn evidence(h: &Harness, item: &QueueItem, status: &str) -> Value {
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    let run = runs.last().expect("the item has a run");
    std::fs::copy(
        h.dir.path().join("reference.png"),
        run.workspace.join("built.png"),
    )
    .unwrap();
    json!({"requirement_results":[{"id":"R1","status":status,"evidence":"ISO labels shown; compared at the approved viewport","visual_pairs":[{"reference":"mockup","built":"built.png","viewport":viewport(),"theme":"light","state":"loaded"}]}]})
}

pub(crate) async fn settle(h: &Harness, data: Value) {
    tokio::time::timeout(
        Duration::from_secs(5),
        h.settle_running(|_| StageOutcome::Completed {
            summary: "VERDICT: pass".into(),
            structured: Some(data.clone()),
        }),
    )
    .await
    .expect("settling must not deadlock");
    tokio::time::timeout(Duration::from_secs(5), h.dispatch())
        .await
        .expect("dispatch must not deadlock");
}

async fn to_acceptance(h: &Harness, item: &QueueItem) {
    settle(h, evidence(h, item, "implemented").await).await;
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
    settle(h, json!({"verdict":"pass","findings":[]})).await;
    let running = h.running().await;
    assert!(
        !running.is_empty(),
        "item state: {:?}, stages: {:?}",
        h.state(&item.id).await,
        h.stages(&item.id).await
    );
    let stage = &running[0];
    assert_eq!(stage.stage_id, "intent-acceptance");
    assert_ne!(stage.choice.model.to_string(), common::MODEL);
}

pub(crate) async fn judgment(h: &Harness, item: &QueueItem, verdict: &str) -> Value {
    let run = h.run(&item.id).await;
    json!({"verdict":verdict,"acceptance":{"head":common::git(&run.workspace,&["rev-parse","HEAD"]),"tree":common::git(&run.workspace,&["rev-parse","HEAD^{tree}"]),"intent":item.intent,"requirements":[{"id":"R1","verdict":verdict,"reason":"Compared approved and built images; checked ISO labels"}]}})
}

pub(crate) async fn readiness(h: &Harness, item: &QueueItem) -> AcceptanceStatus {
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    detail.acceptance.status
}

#[tokio::test]
async fn missing_status_fails_build_and_old_daemon_receives_paths() {
    let (h, _fake, item) = fixture(false).await;
    let request = h.backend.requests().pop().unwrap();
    assert!(request.references.is_empty());
    assert!(
        request
            .instructions
            .contains("Aether lacks references support")
    );
    assert!(request.instructions.contains("Keep ISO labels visible"));
    settle(&h, json!({})).await;
    assert!(matches!(
        h.stages(&item.id).await[0].state,
        StageState::Failed { .. }
    ));
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Pending);
}

#[tokio::test]
async fn references_reach_build_review_and_independent_acceptance() {
    let (h, _fake, item) = fixture(true).await;
    to_acceptance(&h, &item).await;
    for request in h.backend.requests() {
        assert_eq!(request.references.len(), 1);
        assert!(request.references[0].path.is_file());
        assert!(request.instructions.contains("inherit_references=true"));
        assert!(request.instructions.contains("Keep ISO labels visible"));
    }
    settle(&h, judgment(&h, &item, "pass").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
    let run = h.run(&item.id).await;
    let page = std::fs::read_to_string(run.workspace.join(".archon/reports/acceptance/index.html"))
        .unwrap();
    assert!(page.contains("Approved reference") && page.contains("Built / proposed"));
    assert!(page.contains("PASS"));
    capture(&run, "acceptance");
    common::git(
        &h.repo,
        &[
            "worktree",
            "remove",
            "--force",
            run.workspace.to_str().unwrap(),
        ],
    );
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
    common::git(
        &h.repo,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "test: change head without changing tree",
        ],
    );
    let new_head = common::git(&h.repo, &["rev-parse", "HEAD"]);
    common::git(
        &h.repo,
        &[
            "update-ref",
            &format!("refs/heads/{}", run.branch),
            &new_head,
        ],
    );
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Stale);
}

#[tokio::test]
async fn independent_gate_failure_cannot_become_readiness() {
    let (h, _fake, item) = fixture(false).await;
    to_acceptance(&h, &item).await;
    settle(&h, judgment(&h, &item, "fail").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Failed);
    assert!(
        !h.stages(&item.id)
            .await
            .iter()
            .any(|s| s.stage_id == "ship")
    );
}

#[tokio::test]
async fn deviation_waits_for_pictured_operator_approval_then_resumes() {
    let (h, _fake, item) = fixture(false).await;
    settle(&h, evidence(&h, &item, "deviation").await).await;
    assert_eq!(h.running().await[0].stage_id, "implement");
    let questions = h.ctx.store.lock().await.questions(true).unwrap();
    assert_eq!(questions.len(), 1);
    assert!(questions[0].text.contains("Reference | proposed pictures"));
    h.cli(Request::Answer {
        question: questions[0].id.clone(),
        text: "approve".into(),
        chat: false,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), h.dispatch())
        .await
        .expect("approval resumes promptly");
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
}

#[tokio::test]
async fn proceed_directive_records_deviations_without_waiting_and_reverts_when_cleared() {
    let (h, _fake, item) = fixture(false).await;
    let mut directive = Directive::new(
        "Going to sleep; proceed",
        DirectiveSource::Operator,
        Some(1),
        archon_core::now(),
    )
    .unwrap();
    directive.clarifications = ClarificationPolicy::Proceed;
    h.ctx
        .store
        .lock()
        .await
        .insert_directive(&directive)
        .unwrap();
    settle(&h, evidence(&h, &item, "deviation").await).await;
    assert!(h.ctx.store.lock().await.questions(true).unwrap().is_empty());
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
    let run = h.run(&item.id).await;
    let recorded = h
        .ctx
        .store
        .lock()
        .await
        .kv_get(&format!("intent_deviations:{}", run.id))
        .unwrap()
        .unwrap();
    assert!(recorded.contains("proceed under active operator directive"));
    h.ctx
        .store
        .lock()
        .await
        .clear_directives(Some(&directive.id))
        .unwrap();
    assert!(
        !archon_daemon::clarifications::proceed_allowed(
            &*h.ctx.store.lock().await,
            None,
            archon_core::now()
        )
        .unwrap()
    );
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    settle(&h, judgment(&h, &item, "pass").await).await;
    settle(&h, json!({})).await;
    let page = std::fs::read_to_string(run.workspace.join(".archon/reports/deviations/index.html"))
        .unwrap();
    assert!(page.contains("Deviations to review"));
    capture(&run, "deviations");
    assert!(page.contains("proceed under active operator directive"));
}

fn viewport() -> Value {
    if std::env::var_os("ARCHON_INTENT_EVIDENCE_DIR").is_some() {
        json!({"width":1200,"height":800})
    } else {
        json!({"width":1,"height":1})
    }
}

fn capture(run: &archon_core::run::PipelineRun, name: &str) {
    let Some(directory) = std::env::var_os("ARCHON_INTENT_EVIDENCE_DIR") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let dest = directory.join(name);
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(run.workspace.join(".archon/reports").join(name)).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
    }
}

async fn proposed_item() -> (Harness, FakeAether, QueueItem) {
    let h = Harness::new().await;
    let fake = common::connect_aether_for_delegations(&h).await;
    let mut plan = common::plan();
    plan.0.insert(
        "intent-acceptance".into(),
        vec![common::choice(
            common::REVIEW_MODEL,
            archon_core::model::Effort::High,
        )],
    );
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":common::PIPELINE,"title":"Choose a layout","brief":"Keep ISO labels visible","autonomy":{"proposal":"lavish"},"model_plan":plan})).unwrap();
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("item")
    };
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let page = run.workspace.join(".archon/reports/proposal/index.html");
    std::fs::create_dir_all(page.parent().unwrap()).unwrap();
    std::fs::write(page, "<!doctype html><body><h1>Proposal</h1></body>").unwrap();
    assert!(item.intent.is_none());
    assert_eq!(h.running().await[0].stage_id, "propose");
    (h, fake, item)
}

#[tokio::test]
async fn proposal_approval_freezes_the_visible_ledger_and_delivers_it_to_build() {
    let (h, _fake, item) = proposed_item().await;
    let mut approved = record(&h);
    let image = h
        .run(&item.id)
        .await
        .workspace
        .join("proposal-reference.png");
    std::fs::copy(&approved.images[0].path, &image).unwrap();
    approved.images[0].path = image;
    let draft = json!({"approved_intent": approved});
    settle(&h, draft).await;
    let gate = h
        .ctx
        .store
        .lock()
        .await
        .gates_for_item(&item.id)
        .unwrap()
        .remove(0);
    let html = std::fs::read_to_string(gate.page.as_ref().unwrap()).unwrap();
    assert!(html.contains("Approved requirement ledger"));
    assert!(html.contains("R1: Keep ISO labels visible"));
    assert!(html.contains("sandbox=\"allow-scripts\""));
    if let Some(directory) = std::env::var_os("ARCHON_INTENT_EVIDENCE_DIR") {
        std::fs::write(
            std::path::PathBuf::from(directory).join("proposal.html"),
            &html,
        )
        .unwrap();
    }
    assert!(h.item(&item.id).await.intent.is_none(), "not approved yet");
    h.cli(Request::Answer {
        question: gate.question.unwrap(),
        text: "yes".into(),
        chat: false,
    })
    .await
    .unwrap();
    h.dispatch().await;
    let approved = h.item(&item.id).await.intent.unwrap();
    let stored = h
        .ctx
        .store
        .lock()
        .await
        .approved_intent(&approved)
        .unwrap()
        .unwrap();
    assert_eq!(stored.operator_words, "Keep ISO labels visible");
    assert!(
        stored
            .artifacts
            .iter()
            .all(|artifact| artifact.path.is_file())
    );
    assert_eq!(h.running().await[0].stage_id, "implement");
    assert!(
        h.backend
            .requests()
            .last()
            .unwrap()
            .instructions
            .contains("Keep ISO labels visible")
    );
}

#[tokio::test]
async fn missing_proposal_ledger_fails_promptly_instead_of_waiting_forever() {
    let (h, _fake, item) = proposed_item().await;
    settle(&h, json!({})).await;
    assert!(matches!(
        h.state(&item.id).await,
        archon_core::queue::QueueState::Failed { .. }
    ));
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .gates_for_item(&item.id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn changing_built_image_after_submission_fails_independent_acceptance() {
    let (h, _fake, item) = fixture(false).await;
    to_acceptance(&h, &item).await;
    let run = h.run(&item.id).await;
    let path = run.workspace.join("built.png");
    let mut image = std::fs::read(&path).unwrap();
    image.extend_from_slice(b"changed after submission");
    std::fs::write(path, image).unwrap();
    settle(&h, judgment(&h, &item, "pass").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Failed);
    assert!(matches!(
        h.state(&item.id).await,
        archon_core::queue::QueueState::Failed { .. }
    ));
}

#[tokio::test]
async fn a_stage_cannot_file_an_intent_even_with_the_operator_quote() {
    let (h, _, _) = fixture(false).await;
    let stage = h.running().await.remove(0);
    let mut approved = record(&h);
    approved.id = "stage_authored".into();
    for client in [
        archon_core::protocol::ClientKind::ArchonMcp,
        archon_core::protocol::ClientKind::ArchonCli,
    ] {
        let request = Request::ForAgent {
            caller: stage.aether_agent.clone().unwrap(),
            request: Box::new(Request::ApproveIntent(approved.clone())),
        };
        let result = archon_daemon::handlers::handle(&h.ctx, client, request).await;
        assert!(
            matches!(result, Err(archon_core::error::ArchonError::Forbidden(_))),
            "{result:?}"
        );
    }
    let binding = IntentBinding {
        id: approved.id,
        version: approved.version,
        requirements: vec![],
    };
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .approved_intent(&binding)
            .unwrap()
            .is_none()
    );
}
