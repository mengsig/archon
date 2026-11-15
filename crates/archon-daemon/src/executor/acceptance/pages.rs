use archon_core::acceptance::{RequirementResult, RequirementStatus};
use archon_core::error::ArchonError;
use archon_core::intent::ApprovedIntent;
use archon_core::run::PipelineRun;
use serde_json::Value;
use std::path::Path;

pub(super) fn write(
    run: &PipelineRun,
    intent: &ApprovedIntent,
    results: &[RequirementResult],
    judgments: Option<&Value>,
    name: &str,
) -> Result<String, ArchonError> {
    assert!(matches!(name, "acceptance" | "deviations"));
    assert!(run.workspace.is_absolute());
    let directory = run.workspace.join(".archon/reports").join(name);
    directory_in_workspace(&run.workspace, &directory)?;
    let title = if name == "deviations" {
        "Deviations to review"
    } else {
        "Approved intent · acceptance"
    };
    let mut body = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>{title}</title><style>{STYLE}</style><main><p class=eyebrow>APPROVED INTENT · {} / V{}</p><h1>{title}</h1><blockquote>{}</blockquote>",
        escape(&intent.id),
        intent.version,
        escape(&intent.operator_words)
    );
    for (index, result) in results.iter().enumerate() {
        if name == "deviations" && result.status != RequirementStatus::Deviation {
            continue;
        }
        let requirement = intent
            .requirements
            .iter()
            .find(|r| r.id == result.id)
            .ok_or_else(|| ArchonError::Invalid(format!("unknown requirement {}", result.id)))?;
        let (badge, reason) = judgment(judgments, result);
        body.push_str(&format!("<section><header><h2>{} · {}</h2><strong class='badge {}'>{}</strong></header><p>{}</p><p class=reason>{}</p>", escape(&result.id), escape(&requirement.text), badge, badge.to_uppercase(), escape(&result.evidence), escape(&reason)));
        pairs(&mut body, run, intent, result, &directory, index)?;
        body.push_str("</section>");
    }
    body.push_str("</main></html>");
    let path = directory.join("index.html");
    save(&path, body.as_bytes())?;
    Ok(path.display().to_string())
}

fn judgment(data: Option<&Value>, result: &RequirementResult) -> (&'static str, String) {
    let judgment = data
        .and_then(|s| s.get("acceptance"))
        .and_then(|s| s.get("requirements"))
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(&result.id))
        });
    if let Some(row) = judgment {
        let badge = if row.get("verdict").and_then(Value::as_str) == Some("pass") {
            "pass"
        } else {
            "fail"
        };
        return (
            badge,
            row.get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Missing reason")
                .into(),
        );
    }
    if data.is_some() {
        return (
            "fail",
            "Independent acceptance omitted this requirement.".into(),
        );
    }
    match result.status {
        RequirementStatus::Deviation => (
            "deviation",
            "Operator decision or standing directive required; see recorded disposition.".into(),
        ),
        _ => ("pending", "Independent acceptance has not passed.".into()),
    }
}

fn pairs(
    body: &mut String,
    run: &PipelineRun,
    intent: &ApprovedIntent,
    result: &RequirementResult,
    directory: &Path,
    index: usize,
) -> Result<(), ArchonError> {
    for (pair_index, pair) in result.visual_pairs.iter().enumerate() {
        let reference = intent
            .images
            .iter()
            .find(|r| r.id == pair.reference)
            .ok_or_else(|| ArchonError::Invalid(format!("missing reference {}", pair.reference)))?;
        let built = super::super::pages::inside_workspace(&run.workspace, &pair.built)
            .map_err(ArchonError::Invalid)?;
        let before = copy(
            &reference.path,
            directory,
            &format!("{index}-{pair_index}-reference"),
        )?;
        let after = copy(&built, directory, &format!("{index}-{pair_index}-built"))?;
        body.push_str(&format!("<p class=meta>{} × {} · {} · {}</p><div class=pair><figure><figcaption>Approved reference</figcaption><img src='{before}' alt='Approved reference for {}'></figure><figure><figcaption>Built / proposed</figcaption><img src='{after}' alt='Built evidence for {}'></figure></div>", pair.viewport.width, pair.viewport.height, escape(&pair.theme), escape(&pair.state), escape(&result.id), escape(&result.id)));
    }
    Ok(())
}

fn copy(source: &Path, directory: &Path, name: &str) -> Result<String, ArchonError> {
    let bytes = super::images::read(source)?;
    let extension = if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.starts_with(b"RIFF") {
        "webp"
    } else {
        "jpg"
    };
    let name = format!("{name}.{extension}");
    save(&directory.join(&name), &bytes)?;
    assert!(!name.contains('/'));
    assert!(directory.join(&name).is_file());
    Ok(name)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const STYLE: &str = "*{box-sizing:border-box}body{margin:0;background:#eef2f6;color:#172b40;font:16px/1.5 system-ui,sans-serif}main{max-width:1440px;margin:auto;padding:40px 24px}h1{font-size:clamp(28px,4vw,48px);margin:0 0 20px}h2{font-size:20px;margin:0;overflow-wrap:anywhere}blockquote{border-left:4px solid #486df0;margin:24px 0;padding:16px 24px;background:white}section{margin:24px 0;padding:24px;background:white;border:1px solid #d6dee8;border-radius:14px}header{display:flex;align-items:start;gap:20px;justify-content:space-between}.badge{border-radius:6px;padding:5px 12px;font-size:13px;letter-spacing:.06em}.pass{color:#126c39;background:#daf5e6}.fail{color:#a51e35;background:#ffe4e8}.deviation,.pending{color:#774d00;background:#fff0c2}.pair{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:20px}figure{min-width:0;margin:0;border:1px solid #d6dee8;border-radius:8px;overflow:hidden}figcaption{padding:10px 14px;background:#f4f7fb;font-weight:650}img{display:block;width:100%;height:auto}.eyebrow,.meta{font-size:13px;letter-spacing:.04em;color:#536982}.reason{color:#3d5269}p,blockquote{overflow-wrap:anywhere}@media(max-width:650px){.pair{grid-template-columns:minmax(0,1fr)}header{flex-wrap:wrap}main{padding:24px 14px}section{padding:16px}}";

pub(super) fn append_decisions(
    page: &str,
    assumptions: &str,
    decisions: &str,
) -> Result<(), ArchonError> {
    let body =
        std::fs::read_to_string(page).map_err(|e| ArchonError::io("read deviations report", e))?;
    let section = format!(
        "<section><h2>Decisions and assumptions</h2><pre style='white-space:pre-wrap'>{}\n{}</pre></section>",
        escape(assumptions),
        escape(decisions)
    );
    let body = if body.contains("</main>") {
        body.replace("</main>", &format!("{section}</main>"))
    } else {
        format!("{body}{section}")
    };
    save(Path::new(page), body.as_bytes())?;
    Ok(())
}

pub(super) fn directory_in_workspace(
    workspace: &Path,
    directory: &Path,
) -> Result<(), ArchonError> {
    let root = workspace
        .canonicalize()
        .map_err(|e| ArchonError::io("resolve workspace", e))?;
    let existing = directory
        .ancestors()
        .find(|path| path.exists())
        .ok_or_else(|| ArchonError::Invalid("report has no existing ancestor".into()))?;
    let ancestor = existing
        .canonicalize()
        .map_err(|e| ArchonError::io("resolve report ancestor", e))?;
    if !directory.starts_with(workspace) || !ancestor.starts_with(&root) {
        return Err(ArchonError::Invalid(
            "intent output directory escapes the workspace".into(),
        ));
    }
    std::fs::create_dir_all(directory)
        .map_err(|e| ArchonError::io("create intent output directory", e))?;
    assert!(directory.is_dir());
    assert!(directory.starts_with(workspace));
    Ok(())
}

fn save(path: &Path, bytes: &[u8]) -> Result<(), ArchonError> {
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.is_file() => {
            return Err(ArchonError::Invalid(format!(
                "intent output {} is not a regular file",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ArchonError::io("inspect intent output", error)),
    }
    std::fs::write(path, bytes).map_err(|e| ArchonError::io("write intent output", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_do_not_follow_links_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join(".archon")).unwrap();
        assert!(
            directory_in_workspace(workspace.path(), &workspace.path().join(".archon/intent"))
                .is_err()
        );
        assert!(!outside.path().join("intent").exists());
        let target = outside.path().join("image.png");
        std::fs::write(&target, b"preserved").unwrap();
        let link = workspace.path().join("image.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(save(&link, b"replacement").is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"preserved");
    }
}
