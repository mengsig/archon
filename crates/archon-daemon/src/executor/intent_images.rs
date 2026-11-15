//! Preserve image data URLs embedded in approved proposal pages.
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, ReferenceImage};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) fn extract(
    html: &str,
    destination: &Path,
    record: &mut ApprovedIntent,
) -> Result<(), ArchonError> {
    let mut rest = html;
    let mut index = 1;
    while let Some(start) = rest.find("data:image/") {
        rest = &rest[start..];
        let comma = rest
            .find(',')
            .ok_or_else(|| ArchonError::Invalid("malformed embedded image".into()))?;
        let header = &rest[..comma];
        let payload_start = comma + 1;
        let end = rest[payload_start..]
            .find(['"', '\'', ')', ' ', '\n', '\r', '\t', '<', '>', ','])
            .map_or(rest.len(), |n| payload_start + n);
        let payload = &rest[payload_start..end];
        let extension = match header {
            "data:image/png;base64" => "png",
            "data:image/jpeg;base64" => "jpg",
            "data:image/gif;base64" => "gif",
            "data:image/webp;base64" => "webp",
            _ => {
                return Err(ArchonError::Invalid(format!(
                    "unsupported embedded image type {header}"
                )));
            }
        };
        let bytes = STANDARD
            .decode(payload)
            .map_err(|e| ArchonError::Invalid(format!("invalid embedded image: {e}")))?;
        if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 || index > 64 {
            return Err(ArchonError::Invalid(
                "embedded images exceed size or count limit".into(),
            ));
        }
        let hash = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = save(destination, &hash, extension, &bytes)?;
        let mut id = format!("embedded{index}");
        while record.images.iter().any(|image| image.id == id) {
            index += 1;
            id = format!("embedded{index}");
        }
        record.images.push(ReferenceImage {
            id,
            path,
            sha256: hash,
            viewport: None,
            theme: None,
            state: None,
        });
        index += 1;
        rest = &rest[end..];
    }
    Ok(())
}

fn save(
    directory: &Path,
    hash: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, ArchonError> {
    std::fs::create_dir_all(directory)
        .map_err(|e| ArchonError::io("create intent artifact directory", e))?;
    let path = directory.join(format!("{hash}.{extension}"));
    crate::handlers::intent::save_bytes(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, Requirement, RequirementKind};

    fn record() -> ApprovedIntent {
        ApprovedIntent {
            id: "test".into(),
            version: 1,
            operator_words: "Do this".into(),
            source: IntentSource::Inbox { seq: 1 },
            artifacts: vec![],
            images: vec![],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Show title".into(),
                kind: RequirementKind::Wording,
                images: vec![],
                owner: "app".into(),
            }],
        }
    }

    #[test]
    fn extracts_embedded_image_and_rejects_invalid_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let mut intent = record();
        extract(
            "<img src='data:image/png;base64,aW1hZ2U='>",
            dir.path(),
            &mut intent,
        )
        .unwrap();
        assert_eq!(std::fs::read(&intent.images[0].path).unwrap(), b"image");
        assert_eq!(
            intent.images[0].sha256,
            Sha256::digest(b"image")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        assert!(
            extract(
                "<img src='data:image/png;base64,%%%'>",
                dir.path(),
                &mut intent
            )
            .is_err()
        );
    }
}
