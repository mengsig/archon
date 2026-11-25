//! A live design's reference records how it was taken, and acceptance holds
//! the built image to it. S4 of the 1.6.5 validation: the reference was a
//! 2880x1800 picture of a 1440x900 viewport at scale 2, the build was taken
//! at 720x450 and scale 2 to make 1440x900 pixels, and the judge failed an
//! unchanged layout because the narrower viewport reflowed the page.

use crate::common::{self, Harness};
use crate::intent_acceptance::{fixture_of, judgment, readiness, settle};
use archon_core::acceptance::AcceptanceStatus;
use archon_core::intent::ApprovedIntent;
use archon_core::queue::QueueItem;
use archon_core::run::StageState;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The header of a PNG `width` × `height` pixels: enough to be read as one.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// The ledger of S4: heading D3, its final picture taken at 1440x900, scale 2.
fn s4_ledger(h: &Harness) -> ApprovedIntent {
    let path = h.dir.path().join("D3-final.png");
    let bytes = png(2880, 1800);
    std::fs::write(&path, &bytes).unwrap();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let size = json!({"width": 1440, "height": 900});
    let capture = json!({"method": "page_screenshot", "viewport": size,
        "area": {"x": 0, "y": 0, "width": 1440, "height": 900},
        "pixels": {"width": 2880, "height": 1800}});
    let ledger: ApprovedIntent = serde_json::from_value(json!({"id": "s4", "version": 1,
        "source": {"kind": "gate", "item": "design", "gate": "design:1"},
        "operator_words": "Change the heading to Garden journal.", "artifacts": [],
        "images": [{"id": "D3-final", "path": path, "sha256": hash, "viewport": size,
            "theme": "light", "state": "loaded", "capture": capture}],
        "requirements": [{"id": "D3", "text": "Change the heading to Garden journal.",
            "kind": "visual", "owner": "operator", "images": ["D3-final"]}]}))
    .unwrap();
    ledger.validate().unwrap();
    ledger
}

async fn s4_item() -> (Harness, archon_aether::fake::FakeAether, QueueItem) {
    let mut config = archon_core::config::Config::default();
    config.review.auto_repair = false;
    let pipeline = (common::PIPELINE, None, common::plan());
    let vision = [common::MODEL, common::REVIEW_MODEL];
    fixture_of(config, pipeline, (true, &vision), s4_ledger).await
}

/// The build's results, its D3 picture written at `width` × `height` pixels.
async fn built(h: &Harness, item: &QueueItem, (width, height): (u32, u32)) -> Value {
    let run = h.run(&item.id).await;
    std::fs::write(run.workspace.join("built.png"), png(width, height)).unwrap();
    json!({"requirement_results": [{"id": "D3", "status": "implemented",
        "evidence": "The heading reads Garden journal",
        "visual_pairs": [{"reference": "D3-final", "built": "built.png",
            "viewport": {"width": 1440, "height": 900}, "theme": "light", "state": "loaded"}]}]})
}

async fn built_alike_to_acceptance(h: &Harness, item: &QueueItem) {
    settle(h, built(h, item, (2880, 1800)).await).await;
    settle(h, json!({"verdict": "pass", "findings": []})).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "stages: {:?}", h.stages(&item.id).await);
    assert_eq!(running[0].stage_id, "intent-acceptance");
}

/// The S4 build picture, 1440x900 pixels for a 2880x1800 reference, is sent
/// back naming how to retake it, before any judge compares unlike images.
#[tokio::test]
async fn s4_a_build_picture_taken_at_another_viewport_is_refused_before_judging() {
    let (h, _fake, item) = s4_item().await;
    let brief = h.backend.requests().pop().unwrap().instructions;
    assert!(brief.contains("\"pixels\""), "{brief}");
    settle(&h, built(&h, &item, (1440, 900)).await).await;
    let stages = h.stages(&item.id).await;
    let StageState::Failed { error, .. } = &stages[0].state else {
        panic!("the unlike build picture is refused: {stages:?}");
    };
    assert!(error.contains("it is 1440x900 pixels"), "{error}");
    assert!(error.contains("1440x900 CSS px viewport"), "{error}");
    assert!(error.contains("device scale factor of 2"), "{error}");
    assert!(error.contains("2880x1800 pixels"), "{error}");
    assert!(!stages.iter().any(|s| s.stage_id == "intent-acceptance"));
}

/// Taken the way its reference was, the unchanged layout passes acceptance.
#[tokio::test]
async fn s4_a_build_picture_taken_like_its_reference_passes_acceptance() {
    let (h, _fake, item) = s4_item().await;
    built_alike_to_acceptance(&h, &item).await;
    settle(&h, judgment_of(&h, &item, "pass").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
}

/// Like with like, a layout that really differs still fails acceptance: the
/// capture check refuses unlike pictures, never a judged difference.
#[tokio::test]
async fn a_real_layout_deviation_taken_alike_still_fails_acceptance() {
    let (h, _fake, item) = s4_item().await;
    built_alike_to_acceptance(&h, &item).await;
    settle(&h, judgment_of(&h, &item, "fail").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Failed);
    assert!(
        !h.stages(&item.id)
            .await
            .iter()
            .any(|s| s.stage_id == "ship")
    );
}

/// [`judgment`] of D3, the S4 requirement.
async fn judgment_of(h: &Harness, item: &QueueItem, verdict: &str) -> Value {
    let mut judged = judgment(h, item, verdict).await;
    judged["acceptance"]["requirements"][0]["id"] = json!("D3");
    judged
}
