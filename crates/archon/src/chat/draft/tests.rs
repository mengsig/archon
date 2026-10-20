use archon_core::attachment::MAX_IMAGE_BYTES;

use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\npixels";

fn png(name: &str) -> ImageAttachment {
    ImageAttachment::encode(PNG, Some(name)).unwrap()
}

fn big_png() -> ImageAttachment {
    let mut bytes = PNG.to_vec();
    bytes.resize(MAX_IMAGE_BYTES, 0);
    ImageAttachment::encode(&bytes, Some("big.png")).unwrap()
}

#[test]
fn attached_images_leave_with_the_line_that_holds_their_chips() {
    let mut draft = Draft::default();
    assert_eq!(draft.attach(png("a.png"), "").unwrap(), "[image #1]");
    assert_eq!(
        draft.attach(png("b.png"), "[image #1] ").unwrap(),
        "[image #2]"
    );
    let (text, images) = draft.take("[image #2] [image #1] which is newer?");
    assert_eq!(text, "which is newer?");
    let names: Vec<_> = images.iter().map(|i| i.name.as_deref().unwrap()).collect();
    assert_eq!(
        names,
        ["b.png", "a.png"],
        "in the order the line shows them"
    );
    assert_eq!(
        draft.take("[image #1] again"),
        ("[image #1] again".into(), Vec::new())
    );
}

#[test]
fn deleting_a_chip_removes_its_image() {
    let mut draft = Draft::default();
    draft.attach(png("a.png"), "").unwrap();
    draft.attach(png("b.png"), "[image #1] ").unwrap();
    let (text, images) = draft.take("only [image #2]");
    assert_eq!((text.as_str(), images.len()), ("only", 1));
    assert_eq!(images[0].name.as_deref(), Some("b.png"));
    assert_eq!(chip_before("look [image #12]"), Some("[image #12]".len()));
    assert_eq!(
        chip_before("look [image #1] "),
        None,
        "only right before the cursor"
    );
    assert_eq!(chip_before("[image #x]"), None);
    assert_eq!(chip_before("[image #]"), None);
}

#[test]
fn a_draft_refuses_more_images_than_a_message_takes() {
    let mut draft = Draft::default();
    let mut line = String::new();
    for _ in 0..MAX_IMAGES_PER_MESSAGE {
        let chip = draft.attach(png("a.png"), &line).unwrap();
        line.push_str(&chip);
    }
    let refused = draft.attach(png("a.png"), &line).unwrap_err();
    assert!(refused.contains("at most 10 images"), "{refused}");
    // Deleting a chip frees its place.
    let fewer = line.replacen("[image #1]", "", 1);
    assert_eq!(draft.attach(png("a.png"), &fewer).unwrap(), "[image #11]");
}

#[test]
fn a_draft_refuses_images_past_the_message_total() {
    let mut draft = Draft::default();
    let first = draft.attach(big_png(), "").unwrap();
    let second = draft.attach(big_png(), &first).unwrap();
    let line = format!("{first} {second}");
    let refused = draft.attach(big_png(), &line).unwrap_err();
    assert!(refused.contains("total 10.0 MB"), "{refused}");
    assert_eq!(draft.chips(), "[image #1] [image #2]");
    draft.keep("[image #2] /status");
    assert_eq!(
        draft.chips(),
        "[image #2]",
        "a line that sends nothing keeps its chips"
    );
    draft.keep("/status");
    assert_eq!(draft.chips(), "");
}

#[test]
fn ctrl_v_attaches_an_image_and_inserts_text() {
    let mut draft = Draft::default();
    let image = paste(&mut draft, "", Ok(Clipboard::Image(png("shot.png"))), None);
    assert_eq!(image.insert.as_deref(), Some("[image #1] "));
    assert_eq!(image.notice, None);
    let text = paste(
        &mut draft,
        "[image #1] ",
        Ok(Clipboard::Text("hi".into())),
        None,
    );
    assert_eq!(
        text,
        Pasted {
            insert: Some("hi".into()),
            notice: None
        }
    );
    let failed = paste(&mut draft, "", Err("no display".into()), None);
    assert_eq!(failed.insert, None);
    assert_eq!(failed.notice.as_deref(), Some("nothing pasted: no display"));
}

#[test]
fn ctrl_v_on_a_copied_image_path_attaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shot.png");
    std::fs::write(&path, PNG).unwrap();
    let mut draft = Draft::default();
    let copied = Clipboard::Text(format!("'{}'", path.display()));
    let pasted = paste(&mut draft, "", Ok(copied), None);
    assert_eq!(pasted.insert.as_deref(), Some("[image #1] "));
    let (_, images) = draft.take("[image #1]");
    assert_eq!(images[0].name.as_deref(), Some("shot.png"));
    let fake = dir.path().join("fake.png");
    std::fs::write(&fake, "not an image").unwrap();
    let copied = Clipboard::Text(fake.display().to_string());
    let pasted = paste(&mut draft, "", Ok(copied), None);
    assert_eq!(pasted.insert, Some(fake.display().to_string()));
    assert!(
        pasted
            .notice
            .unwrap()
            .starts_with("pasted as text, not attached")
    );
}

#[test]
fn the_history_keeps_a_line_without_its_chips() {
    assert_eq!(
        without_chips("[image #1] what is [image #12]this?"),
        "what is this?"
    );
    assert_eq!(without_chips("[image #1]"), "");
    let not_chips = "keep [image #x] and [image # 2]";
    assert_eq!(without_chips(not_chips), not_chips);
    assert_eq!(without_chips("[image #3"), "[image #3");
}
