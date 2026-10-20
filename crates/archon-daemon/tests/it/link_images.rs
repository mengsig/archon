//! Images the operator sends with a chat line: checked as Aether checks
//! them, recorded in the feed by reference only, and delivered to the
//! orchestrator on Aether's `send_message.attachments`.

use crate::link_common;

use archon_aether::fake::{self, Scripted};
use archon_core::attachment::{ImageAttachment, ImageRef, MAX_IMAGES_PER_MESSAGE};
use archon_core::error::ArchonError;
use archon_core::notify::FeedEntry;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::{handlers, supervisor};
use archon_queue::store::keys;
use link_common::Harness;
use serde_json::json;

const ORCH: &str = "agt_0001";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\npixels";

fn shot() -> ImageAttachment {
    ImageAttachment::encode(PNG, Some("shot.png")).expect("a PNG")
}

/// A harness whose running orchestrator takes every message.
async fn listening() -> Harness {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut agent = fake::agent(ORCH, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({"state": "running"});
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", agent));
    harness
        .fake
        .set_default("send_message", Scripted::reply("message_sent", json!({})));
    harness
}

async fn say(
    harness: &Harness,
    text: &str,
    images: Vec<ImageAttachment>,
) -> Result<Reply, ArchonError> {
    let request = Request::Say {
        text: text.into(),
        images,
    };
    handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await
}

fn operator_lines(feed: Vec<FeedEntry>) -> Vec<(String, Vec<ImageRef>)> {
    feed.into_iter()
        .filter_map(|entry| match entry {
            FeedEntry::Operator { text, images } => Some((text, images)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_image_reaches_the_orchestrator_and_the_feed_keeps_only_its_reference() {
    let harness = listening().await;
    say(&harness, " what is this? ", vec![shot()])
        .await
        .unwrap();
    let sent = harness.fake.received_named("send_message").remove(0);
    assert_eq!(sent["to"], ORCH);
    assert_eq!(sent["content"], "what is this?");
    assert_eq!(sent["attachments"], json!([shot()]));
    let lines = operator_lines(harness.feed().await);
    assert_eq!(
        lines,
        vec![("what is this?".to_owned(), vec![shot().reference()])]
    );
    assert_eq!(lines[0].1[0].label(), "[image: shot.png, 14 B]");
    let recorded = serde_json::to_string(&harness.feed().await).unwrap();
    assert!(
        !recorded.contains(&shot().data),
        "no image bytes in the feed"
    );
}

#[tokio::test]
async fn an_image_needs_no_text_but_an_empty_line_is_still_refused() {
    let harness = listening().await;
    say(&harness, "  ", vec![shot()]).await.unwrap();
    let sent = harness.fake.received_named("send_message").remove(0);
    assert_eq!(sent["content"], "");
    let refused = say(&harness, "  ", Vec::new()).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused}");
    assert_eq!(operator_lines(harness.feed().await).len(), 1);
}

#[tokio::test]
async fn a_text_only_line_goes_out_as_it_always_did() {
    let harness = listening().await;
    say(&harness, "status?", Vec::new()).await.unwrap();
    let sent = harness.fake.received_named("send_message").remove(0);
    assert!(sent.get("attachments").is_none(), "{sent}");
    let entry = serde_json::to_value(&harness.feed().await[0]).unwrap();
    assert!(entry.get("images").is_none(), "{entry}");
}

#[tokio::test]
async fn images_aether_would_refuse_are_refused_before_anything_is_recorded() {
    let harness = listening().await;
    let too_many = vec![shot(); MAX_IMAGES_PER_MESSAGE + 1];
    let mut forged = shot();
    forged.media_type = "image/jpeg".into();
    let not_an_image = ImageAttachment {
        media_type: "image/png".into(),
        data: "aGVsbG8=".into(),
        name: Some("notes.txt".into()),
    };
    for (images, says) in [
        (too_many, "too many images"),
        (vec![forged], "declared as image/jpeg"),
        (vec![not_an_image], "notes.txt is not a supported type"),
    ] {
        let refused = say(&harness, "look", images).await.unwrap_err();
        assert!(
            matches!(&refused, ArchonError::Invalid(why) if why.contains(says)),
            "{refused}"
        );
    }
    assert!(operator_lines(harness.feed().await).is_empty());
    assert!(harness.fake.received_named("send_message").is_empty());
}
