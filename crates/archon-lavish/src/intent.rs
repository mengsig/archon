//! Render a reviewable requirement ledger with its reference images.
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, ReferenceImage};
use sha2::{Digest, Sha256};

pub fn render(intent: &ApprovedIntent) -> Result<String, ArchonError> {
    intent.validate()?;
    let mut html = format!(
        "<style>{STYLE}</style><section class=approved-ledger><p class=intent-kicker>APPROVED INTENT · {} / V{}</p><h2>Approved requirement ledger</h2>",
        escape(&intent.id),
        intent.version
    );
    html.push_str("<h3>Operator words</h3><pre style='white-space:pre-wrap'>");
    html.push_str(&escape(&intent.operator_words));
    html.push_str("</pre>");
    for requirement in &intent.requirements {
        html.push_str(&format!(
            "<article class=intent-row><div><h3>{}: {}</h3><p>{:?} · {}</p></div><div class=intent-images>",
            escape(&requirement.id),
            escape(&requirement.text),
            requirement.kind,
            escape(&requirement.owner)
        ));
        for id in &requirement.images {
            let image = intent
                .images
                .iter()
                .find(|image| &image.id == id)
                .expect("validated image reference");
            html.push_str(&render_image(image)?);
        }
        html.push_str("</div></article>");
    }
    let unused: Vec<_> = intent
        .images
        .iter()
        .filter(|image| {
            !intent
                .requirements
                .iter()
                .any(|r| r.images.contains(&image.id))
        })
        .collect();
    if !unused.is_empty() {
        html.push_str("<h3>Additional reference images</h3>");
        for image in unused {
            html.push_str(&render_image(image)?);
        }
    }
    html.push_str("</section>");
    Ok(html)
}

/// Render the ledger with sibling image files and isolate the proposal's scripts and styles from the ledger being approved.
pub fn proposal(intent: &ApprovedIntent, original: &str) -> Result<String, ArchonError> {
    let ledger = render(intent)?;
    Ok(format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Approve intent</title><body style=\"margin:0;background:#eef2f6\">{ledger}<details style=\"max-width:1440px;margin:24px auto;padding:24px;font:16px system-ui\"><summary>Original proposal</summary><iframe title=\"Original proposal\" sandbox=\"allow-scripts\" style=\"width:100%;height:80vh;border:1px solid #d6dee8;margin-top:16px\" srcdoc=\"{}\"></iframe></details></body></html>",
        escape(original)
    ))
}

fn render_image(image: &ReferenceImage) -> Result<String, ArchonError> {
    let metadata = std::fs::metadata(&image.path)
        .map_err(|e| ArchonError::io(format!("inspect image {}", image.path.display()), e))?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
        return Err(ArchonError::Invalid(format!(
            "image {} is not a file within the limit",
            image.id
        )));
    }
    let data = std::fs::read(&image.path)
        .map_err(|e| ArchonError::io(format!("read image {}", image.path.display()), e))?;
    if data.len() > 32 * 1024 * 1024
        || Sha256::digest(&data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            != image.sha256
    {
        return Err(ArchonError::Invalid(format!(
            "image {} exceeds the limit or has changed",
            image.id
        )));
    }
    if archon_core::attachment::sniff_media_type(&data).is_none() {
        return Err(ArchonError::Invalid(format!(
            "unsupported image {}",
            image.id
        )));
    }
    let name = image
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .ok_or_else(|| ArchonError::Invalid(format!("invalid filename for image {}", image.id)))?;
    let details = image_details(image);
    Ok(format!(
        "<figure><img style=\"max-width:100%\" alt=\"{}\" src=\"./{name}\"><figcaption>{}{}</figcaption></figure>",
        escape(&image.id),
        escape(&image.id),
        details
    ))
}

fn image_details(image: &ReferenceImage) -> String {
    format!(
        "{}{}{}",
        image
            .viewport
            .as_ref()
            .map(|v| format!(" · {} × {}", v.width, v.height))
            .unwrap_or_default(),
        image
            .theme
            .as_ref()
            .map(|t| format!(" · {}", escape(t)))
            .unwrap_or_default(),
        image
            .state
            .as_ref()
            .map(|s| format!(" · {}", escape(s)))
            .unwrap_or_default()
    )
}

const STYLE: &str = ".approved-ledger{font:16px/1.5 system-ui,sans-serif;color:#172b40;background:#eef2f6;padding:32px;margin:32px auto;max-width:1440px;border-radius:16px}.approved-ledger h2{font-size:32px;margin:0 0 24px}.approved-ledger h3{font-size:20px;margin:0 0 12px}.approved-ledger pre{background:white;padding:20px;border-left:4px solid #486df0}.approved-ledger .intent-kicker{font-size:13px;color:#536982;letter-spacing:.08em}.approved-ledger .intent-row{display:grid;grid-template-columns:minmax(220px,1fr) minmax(0,2fr);gap:28px;background:white;border:1px solid #d6dee8;border-radius:12px;padding:24px;margin:20px 0}.approved-ledger figure{margin:0 0 16px}.approved-ledger img{display:block;width:100%;height:auto;border:1px solid #d6dee8;border-radius:8px}.approved-ledger figcaption{font-size:13px;color:#536982;margin-top:8px}@media(max-width:650px){.approved-ledger .intent-row{grid-template-columns:1fr}.approved-ledger{padding:16px}}";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, Requirement, RequirementKind};

    #[test]
    fn ledger_escapes_words_and_places_image_with_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mock.png");
        let bytes = b"\x89PNG\r\n\x1a\nexample";
        std::fs::write(&path, bytes).unwrap();
        let intent = ApprovedIntent {
            id: "design".into(),
            version: 1,
            operator_words: "<keep this>".into(),
            source: IntentSource::Inbox { seq: 1 },
            artifacts: vec![],
            images: vec![ReferenceImage {
                id: "screen".into(),
                path,
                sha256: Sha256::digest(bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                viewport: None,
                theme: None,
                state: None,
            }],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Match screen".into(),
                kind: RequirementKind::Visual,
                images: vec!["screen".into()],
                owner: "ui".into(),
            }],
        };
        let html = render(&intent).unwrap();
        assert!(html.contains("&lt;keep this&gt;"));
        assert!(html.contains("src=\"./mock.png\""));
        assert!(html.find("Match screen") < html.find("./mock.png"));
        let original =
            "<style>body{display:none}</style><script>parent.document.body.remove()</script>";
        let isolated = proposal(&intent, original).unwrap();
        assert!(isolated.contains("sandbox=\"allow-scripts\""));
        assert!(
            !isolated.contains(original),
            "proposal code cannot affect the ledger document"
        );
        assert!(isolated.contains("&lt;style&gt;body{display:none}"));
    }
}
