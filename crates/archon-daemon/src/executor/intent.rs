//! Freeze the ledger displayed with a proposal before its gate opens.
use super::pages;
use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::intent::{
    ApprovedIntent, IntentArtifact, IntentSource, ReferenceImage, Requirement,
};
use archon_core::run::{OperatorGate, PipelineRun, StageRun};
use archon_queue::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Draft {
    operator_words: String,
    artifacts: Vec<IntentArtifact>,
    images: Vec<ReferenceImage>,
    requirements: Vec<Requirement>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Pending {
    pub record: ApprovedIntent,
    pub page: PathBuf,
    pub page_sha256: String,
}

fn key(gate: &OperatorGate) -> String {
    format!(
        "proposal_intent:{}:{}:{}",
        gate.run, gate.stage_id, gate.attempt
    )
}

pub(super) fn has_pending(store: &Store, gate: &OperatorGate) -> Result<bool, ArchonError> {
    Ok(store.kv_get(&key(gate))?.is_some())
}

pub(super) fn pending(store: &Store, gate: &OperatorGate) -> Result<Pending, ArchonError> {
    let raw = store
        .kv_get(&key(gate))?
        .ok_or_else(|| ArchonError::NotFound("proposal intent draft".into()))?;
    serde_json::from_str(&raw).map_err(|e| ArchonError::Corrupt(format!("proposal intent: {e}")))
}

pub(super) fn prepare(
    ctx: &DaemonContext,
    store: &mut Store,
    run: &PipelineRun,
    gate: &OperatorGate,
    stages: &[StageRun],
    original: &Path,
) -> Result<PathBuf, ArchonError> {
    if store.kv_get(&key(gate))?.is_some() {
        let saved = pending(store, gate)?;
        verify_page(&saved)?;
        return Ok(saved.page);
    }
    let mut record = make_record(run, gate, stages)?;
    let original_html =
        std::fs::read_to_string(original).map_err(|e| ArchonError::io("read proposal page", e))?;
    let destination = ctx.paths.data_dir.join("approved-intent-artifacts");
    super::intent_images::extract(&original_html, &destination, &mut record)?;
    super::intent_html::extract(
        &original_html,
        original,
        &run.workspace,
        &destination,
        &mut record,
    )?;
    record.validate()?;
    preserve_files(ctx, &run.workspace, &mut record)?;
    let original_hash = Sha256::digest(original_html.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let preserved = crate::handlers::intent::preserve(&destination, original, &original_hash)?;
    if !record.artifacts.iter().any(|a| a.sha256 == original_hash) {
        record.artifacts.push(IntentArtifact {
            path: preserved,
            sha256: original_hash,
        });
    }
    let page = render_page(original, &record, gate.attempt)?;
    let bytes =
        std::fs::read(&page).map_err(|e| ArchonError::io("read proposal ledger page", e))?;
    let page_sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let preserved = crate::handlers::intent::preserve(&destination, &page, &page_sha256)?;
    record.artifacts.push(IntentArtifact {
        path: preserved,
        sha256: page_sha256.clone(),
    });
    let saved = Pending {
        record,
        page: page.clone(),
        page_sha256,
    };
    let json = serde_json::to_string(&saved)?;
    store.kv_set(&key(gate), &json)?;
    Ok(page)
}

fn make_record(
    run: &PipelineRun,
    gate: &OperatorGate,
    stages: &[StageRun],
) -> Result<ApprovedIntent, ArchonError> {
    let draft = stages
        .iter()
        .find(|s| s.stage_id == gate.stage_id && s.attempt == gate.attempt)
        .and_then(|s| s.structured.as_ref())
        .and_then(|s| s.get("approved_intent"))
        .ok_or_else(|| {
            ArchonError::Invalid("proposal must supply structured.approved_intent".into())
        })?;
    let draft: Draft = serde_json::from_value(draft.clone())
        .map_err(|e| ArchonError::Invalid(format!("proposal intent draft: {e}")))?;
    let record = ApprovedIntent {
        id: run.id.to_string(),
        version: gate.attempt,
        operator_words: draft.operator_words,
        source: IntentSource::Gate {
            item: gate.item.to_string(),
            gate: format!("{}:{}", gate.stage_id, gate.attempt),
        },
        artifacts: draft.artifacts,
        images: draft.images,
        requirements: draft.requirements,
    };
    Ok(record)
}

fn preserve_files(
    ctx: &DaemonContext,
    workspace: &Path,
    record: &mut ApprovedIntent,
) -> Result<(), ArchonError> {
    let dest = ctx.paths.data_dir.join("approved-intent-artifacts");
    for artifact in &mut record.artifacts {
        let path = source_path(workspace, &dest, &artifact.path)?;
        artifact.path = crate::handlers::intent::preserve(&dest, &path, &artifact.sha256)?;
    }
    for image in &mut record.images {
        let path = source_path(workspace, &dest, &image.path)?;
        image.path = crate::handlers::intent::preserve(&dest, &path, &image.sha256)?;
    }
    Ok(())
}

fn source_path(workspace: &Path, destination: &Path, path: &Path) -> Result<PathBuf, ArchonError> {
    if path.is_absolute() && path.parent() == Some(destination) {
        return Ok(path.to_path_buf());
    }
    pages::inside_workspace(workspace, &path.to_string_lossy()).map_err(ArchonError::Invalid)
}

fn render_page(
    original: &Path,
    record: &ApprovedIntent,
    attempt: u32,
) -> Result<PathBuf, ArchonError> {
    let html =
        std::fs::read_to_string(original).map_err(|e| ArchonError::io("read proposal page", e))?;
    let ledger = archon_lavish::intent::render(record)?;
    let page = original.with_file_name(format!("archon-intent-{attempt}.html"));
    let content = match html.rfind("</body>") {
        Some(index) => format!("{}{}{}", &html[..index], ledger, &html[index..]),
        None => format!("{html}{ledger}"),
    };
    crate::handlers::intent::save_bytes(&page, content.as_bytes())?;
    Ok(page)
}

pub(super) fn verify_page(saved: &Pending) -> Result<(), ArchonError> {
    let bytes =
        std::fs::read(&saved.page).map_err(|e| ArchonError::io("read proposal ledger page", e))?;
    if Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        != saved.page_sha256
    {
        return Err(ArchonError::Invalid(
            "proposal ledger page changed after it was shown".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{Requirement, RequirementKind};

    #[test]
    fn frozen_page_cannot_be_changed_after_presentation() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("index.html");
        std::fs::write(&original, "<body>Proposal</body>").unwrap();
        let record = ApprovedIntent {
            id: "x".into(),
            version: 1,
            operator_words: "Build this".into(),
            source: IntentSource::Inbox { seq: 1 },
            artifacts: vec![],
            images: vec![],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Title".into(),
                kind: RequirementKind::Wording,
                images: vec![],
                owner: "app".into(),
            }],
        };
        let page = render_page(&original, &record, 1).unwrap();
        let bytes = std::fs::read(&page).unwrap();
        let saved = Pending {
            record,
            page,
            page_sha256: Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        };
        assert!(verify_page(&saved).is_ok());
        std::fs::write(&saved.page, "changed").unwrap();
        assert!(verify_page(&saved).is_err());
    }
}

pub(crate) fn extract_artifact(
    source: &Path,
    destination: &Path,
    record: &mut ApprovedIntent,
) -> Result<(), ArchonError> {
    if !source
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "html" | "htm"))
    {
        return Ok(());
    }
    let html =
        std::fs::read_to_string(source).map_err(|e| ArchonError::io("read approved HTML", e))?;
    let parent = source
        .parent()
        .ok_or_else(|| ArchonError::Invalid("approved HTML has no directory".into()))?;
    super::intent_images::extract(&html, destination, record)?;
    super::intent_html::extract(&html, source, parent, destination, record)?;
    record.validate()?;
    Ok(())
}
