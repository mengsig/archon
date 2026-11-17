use super::intent_acceptance::{models_listed, record, settle};
use crate::common::{self, Harness};
use archon_aether::fake::FakeAether;
use archon_core::intent::{ApprovedIntent, IntentArtifact, ReferenceImage};
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::reference::ReferenceKind;
use serde_json::json;
use sha2::{Digest, Sha256};

async fn fixture(
    images: usize,
    artifacts: usize,
) -> (Harness, FakeAether, QueueItem, ApprovedIntent) {
    let h = Harness::new().await;
    let fake = models_listed(&h, &[common::MODEL, common::REVIEW_MODEL]).await;
    fake.set_features(vec!["references".into()]);
    let client = archon_aether::AetherClient::connect(&fake.endpoint())
        .await
        .unwrap();
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(client)))
        .unwrap();
    let intent = many_references(&h, images, artifacts);
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
    let request = serde_json::from_value(json!({"repo":common::REPO_NAME,"pipeline":common::PIPELINE,"title":"many approved images","brief":"Use every approved image","intent":{"id":intent.id,"version":1,"requirements":[]},"model_plan":plan})).unwrap();
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("item")
    };
    assert!(item.intent.is_some());
    assert!(h.backend.requests().is_empty());
    (h, fake, item, intent)
}

fn many_references(h: &Harness, images: usize, artifacts: usize) -> ApprovedIntent {
    let mut intent = record(h);
    let template = intent.images[0].clone();
    intent.images = (0..images)
        .map(|n| {
            let mut image = template.clone();
            image.id = format!("image_{n}");
            image.path = h.dir.path().join(format!("image_{n}.png"));
            std::fs::copy(&template.path, &image.path).unwrap();
            image
        })
        .collect();
    intent.artifacts = (0..artifacts)
        .map(|n| {
            let path = h.dir.path().join(format!("proposal_{n}.html"));
            let bytes = format!("<!doctype html><h1>Proposal {n}</h1>");
            std::fs::write(&path, &bytes).unwrap();
            let sha256 = Sha256::digest(bytes.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            IntentArtifact { path, sha256 }
        })
        .collect();
    intent.requirements[0].images = intent.images.iter().map(|i| i.id.clone()).collect();
    intent.validate().unwrap();
    assert_eq!(intent.images.len(), images);
    assert_eq!(intent.artifacts.len(), artifacts);
    intent
}

fn check_references(
    request: &SpawnRequest,
    intent: &ApprovedIntent,
    artifacts: usize,
    images: usize,
) {
    assert_eq!(request.references.len(), artifacts + images);
    assert!(request.references.len() <= 16);
    assert!(
        request.references[..artifacts]
            .iter()
            .all(|r| r.kind == ReferenceKind::File)
    );
    assert!(
        request.references[artifacts..]
            .iter()
            .all(|r| r.kind == ReferenceKind::Image)
    );
    for (reference, image) in request.references[artifacts..].iter().zip(&intent.images) {
        assert_eq!(reference.label.as_deref(), Some(image.id.as_str()));
        assert!(reference.path.is_file());
    }
    if images == intent.images.len() {
        return;
    }
    let (_, listed) = request
        .instructions
        .split_once("Reference images (path only): ")
        .unwrap();
    let listed: Vec<ReferenceImage> = serde_json::from_str(listed).unwrap();
    assert_eq!(listed.len(), intent.images.len() - images);
    for (image, listed) in intent.images[images..].iter().zip(listed) {
        assert_eq!(listed.id, image.id);
        assert_eq!(listed.sha256, image.sha256);
        let path = request
            .workspace
            .join(".archon/intent")
            .join(&intent.id)
            .join("1")
            .join(format!("{}.png", image.id));
        assert_eq!(listed.path, path);
        assert!(path.is_file());
        assert!(request.instructions.contains(path.to_str().unwrap()));
        assert!(request.instructions.contains(&image.id));
        assert!(request.instructions.contains(&image.sha256));
    }
    assert!(request.instructions.contains("unchanged ledger"));
}

#[tokio::test]
async fn fifty_two_images_reach_build_review_and_acceptance_with_bounded_attachments() {
    let (h, _fake, item, intent) = fixture(52, 1).await;
    h.dispatch().await;
    check_references(&h.backend.requests()[0], &intent, 1, 8);
    let run = h.run(&item.id).await;
    let built = run.workspace.join("built.png");
    std::fs::copy(&intent.images[0].path, &built).unwrap();
    let pairs: Vec<_> = intent.images.iter().map(|image| json!({"reference":image.id,"built":built,"viewport":{"width":1,"height":1},"theme":"light","state":"loaded"})).collect();
    settle(&h, json!({"requirement_results":[{"id":"R1","status":"implemented","evidence":"all references checked","visual_pairs":pairs}]})).await;
    assert!(h.running().await.iter().all(|s| s.stage_id == "review"));
    settle(&h, json!({"verdict":"pass","findings":[]})).await;
    assert_eq!(h.running().await[0].stage_id, "intent-acceptance");
    let requests = h.backend.requests();
    assert!(requests.len() >= 3);
    for request in requests {
        check_references(&request, &intent, 1, 8);
        assert!(request.instructions.contains("Open them, and explicitly pass these paths and the unchanged ledger to every worker and reviewer"));
    }
}

#[tokio::test]
async fn three_images_are_all_attached_after_the_artifact() {
    let (h, _fake, _, intent) = fixture(3, 1).await;
    h.dispatch().await;
    check_references(&h.backend.requests()[0], &intent, 1, 3);
}

#[tokio::test]
async fn artifacts_take_priority_and_share_the_total_reference_cap() {
    for artifacts in [12, 16, 17] {
        let (h, _fake, _, intent) = fixture(52, artifacts).await;
        h.dispatch().await;
        let attached = artifacts.min(16);
        check_references(&h.backend.requests()[0], &intent, attached, 16 - attached);
    }
}

#[tokio::test]
async fn changed_hash_is_refused_for_attached_and_path_only_images() {
    for changed in [0, 51] {
        let (h, _fake, _, intent) = fixture(52, 1).await;
        let mut bytes = std::fs::read(&intent.images[changed].path).unwrap();
        bytes.extend_from_slice(b"changed after approval");
        std::fs::write(&intent.images[changed].path, bytes).unwrap();
        let error = archon_daemon::dispatcher::dispatch_once(&h.ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed"), "{error}");
        assert!(error.contains(&intent.images[changed].id), "{error}");
        assert!(h.backend.requests().is_empty());
    }
}
