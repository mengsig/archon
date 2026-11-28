//! A live design's reference records how it was taken, and acceptance holds
//! the built image to it. S4 of the 1.6.5 validation: the reference was a
//! 2880x1800 picture of a 1440x900 viewport (page at DPR 1, clip scale 2),
//! the build was taken at 720x450 and scale 2 to make 1440x900 pixels, and
//! the judge failed an unchanged layout because the narrower page reflowed.

use crate::common::{self, Harness};
use crate::design_gate::{self, Pictures};
use crate::intent_acceptance::{fixture_of, judgment, readiness, settle};
use archon_core::acceptance::AcceptanceStatus;
use archon_core::intent::{ApprovedIntent, ImageCapture};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::run::{StageRun, StageState};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// A card on a white page, by device px: the S4 heading's card.
#[derive(Clone, Copy)]
struct Card {
    x: u32,
    width: u32,
    /// The heading bar's length: "Garden journal" is longer than "notes".
    heading: u32,
}

/// The approved design, the build of it, and one that widens the card.
const APPROVED: Card = Card {
    x: 736,
    width: 1408,
    heading: 300,
};
const UNCHANGED: Card = Card {
    heading: 340,
    ..APPROVED
};
const WIDENED: Card = Card {
    x: 32,
    width: 2816,
    heading: 340,
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Rows of a 1-bit grey page `width` wide and `height` high showing `card`.
fn rows(width: u32, height: u32, card: Card) -> Vec<u8> {
    let row = |black: &[(u32, u32)]| {
        let mut bytes = vec![0xFF_u8; width.div_ceil(8) as usize];
        for &(from, to) in black {
            for x in from..to.min(width) {
                bytes[(x / 8) as usize] &= !(0x80 >> (x % 8));
            }
        }
        [&[0u8][..], &bytes].concat()
    };
    let (blank, heading) = (row(&[]), row(&[(card.x, card.x + card.heading)]));
    let body = row(&[(card.x, card.x + card.width)]);
    let pick = |y: u32| match y {
        96..=143 => &heading,
        160..=559 => &body,
        _ => &blank,
    };
    let raw: Vec<u8> = (0..height).flat_map(|y| pick(y).iter().copied()).collect();
    assert_eq!(raw.len(), (height * (1 + width.div_ceil(8))) as usize);
    raw
}

/// A valid PNG of the page: stored deflate blocks, so no encoder is needed.
fn png(width: u32, height: u32, card: Card) -> Vec<u8> {
    let raw = rows(width, height, card);
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xFFFF).collect();
    for (i, block) in blocks.iter().enumerate() {
        let len = block.len() as u16;
        zlib.push(u8::from(i + 1 == blocks.len()));
        zlib.extend(len.to_le_bytes().iter().chain(&(!len).to_le_bytes()));
        zlib.extend_from_slice(block);
    }
    let (a, b) = raw.iter().fold((1u32, 0u32), |(a, b), &x| {
        let a = (a + u32::from(x)) % 65521;
        (a, (b + a) % 65521)
    });
    zlib.extend(((b << 16) | a).to_be_bytes());
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([1, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(&b"IHDR"[..], &ihdr[..]), (b"IDAT", &zlib), (b"IEND", &[])] {
        out.extend((data.len() as u32).to_be_bytes());
        let typed = [kind, data].concat();
        out.extend_from_slice(&typed);
        out.extend(crc32(&typed).to_be_bytes());
    }
    out
}

/// How the S4 reference was taken: the preview's page at DPR 1, clip scale 2.
fn s4_capture() -> Value {
    json!({"method": "page_screenshot", "viewport": {"width": 1440, "height": 900},
        "device_scale_factor": 1.0, "mobile": false,
        "area": {"x": 0, "y": 0, "width": 1440, "height": 900},
        "screenshot_scale": 2.0, "pixels": {"width": 2880, "height": 1800}})
}

/// The ledger of S4: heading D3 and its final picture.
fn s4_ledger(h: &Harness) -> ApprovedIntent {
    let path = h.dir.path().join("D3-final.png");
    let bytes = png(2880, 1800, APPROVED);
    std::fs::write(&path, &bytes).unwrap();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let ledger: ApprovedIntent = serde_json::from_value(json!({"id": "s4", "version": 1,
        "source": {"kind": "gate", "item": "design", "gate": "design:1"},
        "operator_words": "Change the heading to Garden journal.", "artifacts": [],
        "images": [{"id": "D3-final", "path": path, "sha256": hash,
            "viewport": {"width": 1440, "height": 900}, "theme": "light", "state": "loaded",
            "capture": s4_capture()}],
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

/// The build's D3 result: `picture` written as its built image, which it
/// declares taken as `capture`.
async fn built(h: &Harness, item: &QueueItem, picture: &[u8], capture: Value) -> Value {
    let run = h.run(&item.id).await;
    std::fs::write(run.workspace.join("built.png"), picture).unwrap();
    json!({"requirement_results": [{"id": "D3", "status": "implemented",
        "evidence": "The heading reads Garden journal",
        "visual_pairs": [{"reference": "D3-final", "built": "built.png",
            "viewport": {"width": 1440, "height": 900}, "theme": "light", "state": "loaded",
            "capture": capture}]}]})
}

/// The build stage's refusal of its result.
async fn refusal(h: &Harness, item: &QueueItem) -> String {
    let stages = h.stages(&item.id).await;
    assert!(!stages.iter().any(|s| s.stage_id == "intent-acceptance"));
    match &stages[0].state {
        StageState::Failed { error, .. } => error.clone(),
        _ => panic!("the build picture is refused before judging: {stages:?}"),
    }
}

/// The S4 build picture, 1440x900 pixels for a 2880x1800 reference, is sent
/// back with the recipe to retake it, before any judge compares the two.
#[tokio::test]
async fn s4_a_build_picture_taken_at_another_viewport_is_refused_before_judging() {
    let (h, _fake, item) = s4_item().await;
    let brief = h.backend.requests().pop().unwrap().instructions;
    assert!(brief.contains("\"screenshot_scale\": 2.0"), "{brief}");
    let picture = png(1440, 900, WIDENED);
    settle(&h, built(&h, &item, &picture, s4_capture()).await).await;
    let error = refusal(&h, &item).await;
    for part in [
        "it is 1440x900 pixels",
        "1440x900 CSS px viewport with deviceScaleFactor 1 and mobile false",
        "scale: 2}",
        "2880x1800 pixels",
    ] {
        assert!(error.contains(part), "{part}: {error}");
    }
}

/// The reviewer's probe: the page at DPR 2 and the clip at scale 1 makes the
/// reference's 2880x1800 pixels, but another layout. Declared, it is refused.
#[tokio::test]
async fn a_build_picture_with_the_page_dpr_in_place_of_the_clip_scale_is_refused() {
    let (h, _fake, item) = s4_item().await;
    let mut swapped = s4_capture();
    swapped["device_scale_factor"] = json!(2.0);
    swapped["screenshot_scale"] = json!(1.0);
    let picture = png(2880, 1800, WIDENED);
    settle(&h, built(&h, &item, &picture, swapped).await).await;
    let error = refusal(&h, &item).await;
    assert!(
        error.contains("D3 reference and built capture differ"),
        "{error}"
    );
    assert!(
        error.contains("declares a 1440x900 viewport at deviceScaleFactor 2"),
        "{error}"
    );
}

/// Both builds are 2880x1800 pictures declaring the reference's capture: the
/// judge, told what the gate checked, compares the two by their layout.
async fn built_alike_to_acceptance(h: &Harness, item: &QueueItem, card: Card) {
    let picture = png(2880, 1800, card);
    assert!(picture.starts_with(b"\x89PNG") && picture != png(2880, 1800, APPROVED));
    settle(h, built(h, item, &picture, s4_capture()).await).await;
    settle(h, json!({"verdict": "pass", "findings": []})).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "stages: {:?}", h.stages(&item.id).await);
    assert_eq!(running[0].stage_id, "intent-acceptance");
    let brief = h.backend.requests().pop().unwrap().instructions;
    assert!(brief.contains("`capture mismatch:`"), "{brief}");
}

/// Taken the way its reference was, the unchanged layout passes acceptance.
#[tokio::test]
async fn s4_a_build_picture_taken_like_its_reference_passes_acceptance() {
    let (h, _fake, item) = s4_item().await;
    built_alike_to_acceptance(&h, &item, UNCHANGED).await;
    settle(&h, judgment_of(&h, &item, "pass").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Passed);
}

/// Like with like, a build that widens the card still fails acceptance: the
/// capture check refuses unlike pictures, never a judged difference.
#[tokio::test]
async fn a_real_layout_deviation_taken_alike_still_fails_acceptance() {
    let (h, _fake, item) = s4_item().await;
    built_alike_to_acceptance(&h, &item, WIDENED).await;
    settle(&h, judgment_of(&h, &item, "fail").await).await;
    assert_eq!(readiness(&h, &item).await, AcceptanceStatus::Failed);
    let stages = h.stages(&item.id).await;
    assert!(!stages.iter().any(|s| s.stage_id == "ship"));
}

/// [`judgment`] of D3, the S4 requirement.
async fn judgment_of(h: &Harness, item: &QueueItem, verdict: &str) -> Value {
    let mut judged = judgment(h, item, verdict).await;
    judged["acceptance"]["requirements"][0]["id"] = json!("D3");
    judged
}

/// S4 through the live flow: finish takes the final picture in the preview,
/// approval keeps how in the ledger, the build is told to take it so, and a
/// build picture taken at 720x450 is refused.
#[tokio::test]
async fn s4_a_live_approval_records_the_capture_and_holds_the_build_to_it() {
    let h = Harness::with_config(design_gate::live_config()).await;
    let live = design_gate::live();
    let item = design_gate::enqueue(&h, live, None).await.unwrap();
    let (run, session) = design_gate::designing(&h, &item).await;
    let preview = Request::DesignPreview {
        url: "http://127.0.0.1:5173/".into(),
        recipe: None,
        caller: session.agent.clone(),
    };
    Box::pin(archon_daemon::handlers::handle(
        &h.ctx,
        ClientKind::ArchonMcp,
        preview,
    ))
    .await
    .expect("preview reported");
    let words = "Change the heading to Garden journal";
    design_gate::note_with(&h, &session, &run, words, Pictures::default()).await;
    finish_and_approve(&h, &session).await;
    let intent = design_gate::intent_of(&h, &item.id).await;
    let expected: ImageCapture = serde_json::from_value(s4_capture()).unwrap();
    assert_eq!(intent.images.len(), 1, "{intent:?}");
    assert_eq!(intent.images[0].capture, Some(expected));
    let brief = h.backend.requests().pop().unwrap().instructions;
    for part in [
        "deviceScaleFactor 1 and mobile false",
        "scale: 2}",
        "\"capture\": {",
    ] {
        assert!(brief.contains(part), "{part}: {brief}");
    }
    let image = intent.images[0].id.clone();
    let requirement = intent.requirements[0].id.clone();
    std::fs::write(run.workspace.join("built.png"), png(1440, 900, UNCHANGED)).unwrap();
    let result = json!({"requirement_results": [{"id": requirement, "status": "implemented",
        "evidence": "The heading reads Garden journal",
        "visual_pairs": [{"reference": image, "built": "built.png",
            "viewport": {"width": 1440, "height": 900}, "capture": s4_capture()}]}]});
    let build = |_: &StageRun| archon_core::run::StageOutcome::Completed {
        summary: "built".into(),
        structured: Some(result.clone()),
    };
    h.settle_running(build).await;
    let stages = h.stages(&item.id).await;
    let refused = stages.iter().find_map(|s| match &s.state {
        StageState::Failed { error, .. } if s.stage_id != "design" => Some(error.clone()),
        _ => None,
    });
    let error = refused.unwrap_or_else(|| panic!("the build is refused: {stages:?}"));
    assert!(error.contains("it is 1440x900 pixels"), "{error}");
    assert!(error.contains("2880x1800 pixels"), "{error}");
}

/// Finish the live session through the browser, confirm every card and
/// approve the gate, as the operator does.
async fn finish_and_approve(h: &Harness, session: &archon_core::design::DesignSession) {
    let capture = Request::DesignFinishCapture {
        session: session.id.clone(),
        cancel_queued: false,
        recapture: false,
    };
    let Reply::DesignFinish(view) = h.cli(capture).await.expect("finish") else {
        panic!("a finish review");
    };
    assert!(view.cards.iter().all(|c| c.asset.is_some()), "{view:?}");
    h.cli(Request::DesignFinishConfirm {
        session: session.id.clone(),
        capture: view.capture.clone(),
        include: view.cards.iter().map(|c| (c.note.clone(), true)).collect(),
        superseded: std::collections::BTreeMap::new(),
        confirm_no_change: false,
    })
    .await
    .expect("confirmed");
    h.settle_running(design_gate::forged).await;
    h.dispatch().await;
    let gate = design_gate::open_proposal(h, &session.item).await;
    design_gate::approve(h, &gate).await;
}
