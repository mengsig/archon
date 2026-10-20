use super::*;

/// The smallest file each sniffed type accepts: its magic bytes.
fn sample(media_type: &str) -> Vec<u8> {
    match media_type {
        "image/png" => b"\x89PNG\r\n\x1a\nrest".to_vec(),
        "image/jpeg" => vec![0xff, 0xd8, 0xff, 0xe0, 0x00],
        "image/gif" => b"GIF89a-rest".to_vec(),
        "image/webp" => b"RIFF\x04\x00\x00\x00WEBPVP8 ".to_vec(),
        other => panic!("no sample for {other}"),
    }
}

fn png_of(size: usize, name: &str) -> ImageAttachment {
    let mut bytes = sample("image/png");
    bytes.resize(size, 0);
    ImageAttachment::encode(&bytes, Some(name)).expect("a PNG within the limit")
}

#[test]
fn each_supported_type_is_recognised_by_its_bytes() {
    for media_type in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
        assert_eq!(sniff_media_type(&sample(media_type)), Some(media_type));
    }
    assert_eq!(sniff_media_type(b"%PDF-1.7"), None);
    assert_eq!(sniff_media_type(b"RIFF\x04\x00\x00\x00WAVEfmt "), None);
    assert_eq!(sniff_media_type(b""), None);
}

#[test]
fn an_encoded_image_checks_back_to_its_reference() {
    let bytes = sample("image/png");
    let image = ImageAttachment::encode(&bytes, Some("/home/me/shot.png")).expect("a PNG");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.size_bytes(), bytes.len() as u64);
    let checked = check_images(std::slice::from_ref(&image)).expect("accepted");
    assert_eq!(checked, vec![image.reference()]);
    assert_eq!(checked[0].name.as_deref(), Some("shot.png"));
    assert_eq!(
        checked[0].label(),
        format!("[image: shot.png, {} B]", bytes.len())
    );
    assert!(
        !format!("{image:?}").contains(&image.data),
        "no payload in logs"
    );
}

#[test]
fn a_file_that_is_not_an_image_is_refused_by_name() {
    let error = ImageAttachment::encode(b"plain text", Some("notes.txt")).unwrap_err();
    assert_eq!(
        error,
        AttachmentError::UnsupportedType {
            label: "notes.txt".into()
        }
    );
    assert!(error.to_string().contains("not PNG, JPEG, GIF or WebP"));
}

#[test]
fn an_image_at_the_limit_passes_and_one_byte_more_does_not() {
    let at_limit = png_of(MAX_IMAGE_BYTES, "big.png");
    assert!(check_images(&[at_limit]).is_ok());
    let mut bytes = sample("image/png");
    bytes.resize(MAX_IMAGE_BYTES + 1, 0);
    let error = ImageAttachment::encode(&bytes, Some("big.png")).unwrap_err();
    assert_eq!(
        error,
        AttachmentError::TooLarge {
            label: "big.png".into(),
            size: MAX_IMAGE_BYTES as u64 + 1
        }
    );
    assert!(error.to_string().contains("3.8 MB"), "{error}");
}

#[test]
fn a_message_is_refused_past_the_image_count_or_total() {
    let small = png_of(64, "a.png");
    let eleven = vec![small.clone(); MAX_IMAGES_PER_MESSAGE + 1];
    assert_eq!(
        check_images(&eleven).unwrap_err(),
        AttachmentError::TooMany { count: 11 }
    );
    assert!(check_images(&eleven[..MAX_IMAGES_PER_MESSAGE]).is_ok());
    let big = png_of(MAX_IMAGE_BYTES, "big.png");
    let three = vec![big; 3];
    let error = check_images(&three).unwrap_err();
    assert_eq!(
        error,
        AttachmentError::TotalTooLarge {
            size: 3 * MAX_IMAGE_BYTES as u64
        }
    );
}

#[test]
fn a_sent_image_must_be_base64_and_what_it_claims() {
    let mut forged = png_of(64, "shot.png");
    forged.media_type = "image/gif".into();
    assert!(matches!(
        check_images(&[forged]).unwrap_err(),
        AttachmentError::TypeMismatch {
            actual: "image/png",
            ..
        }
    ));
    let garbled = ImageAttachment {
        media_type: "image/png".into(),
        data: "not base64!".into(),
        name: None,
    };
    let error = check_images(&[garbled]).unwrap_err();
    assert!(
        matches!(&error, AttachmentError::NotBase64 { label, .. } if label == "#1"),
        "{error:?}"
    );
    let empty = ImageAttachment {
        media_type: "image/png".into(),
        data: String::new(),
        name: Some("blank.png".into()),
    };
    assert_eq!(
        check_images(&[empty]).unwrap_err(),
        AttachmentError::Empty {
            label: "blank.png".into()
        }
    );
}

#[test]
fn names_are_one_safe_line_and_labels_follow_the_text() {
    assert_eq!(
        clean_name("C:\\shots\\a\u{202e}b.png").as_deref(),
        Some("ab.png")
    );
    assert_eq!(clean_name(" \n "), None);
    assert_eq!(clean_name(&"x".repeat(500)).map(|n| n.len()), Some(120));
    let image = ImageRef {
        media_type: "image/png".into(),
        size_bytes: 2048,
        name: None,
    };
    assert_eq!(
        with_image_labels(" look ", std::slice::from_ref(&image)),
        "look\n[image: image/png, 2.0 KB]"
    );
    assert_eq!(
        with_image_labels("", &[image]),
        "[image: image/png, 2.0 KB]"
    );
    assert_eq!(with_image_labels("just text", &[]), "just text");
}

#[test]
fn an_attachment_is_aethers_wire_shape() {
    let image = png_of(16, "shot.png");
    let wire = serde_json::to_value(&image).expect("serializes");
    assert_eq!(wire["media_type"], "image/png");
    assert_eq!(wire["name"], "shot.png");
    assert!(wire["data"].is_string());
    let unnamed = ImageAttachment {
        name: None,
        ..image
    };
    let wire = serde_json::to_value(&unnamed).expect("serializes");
    assert!(wire.get("name").is_none(), "absent, not null");
}
