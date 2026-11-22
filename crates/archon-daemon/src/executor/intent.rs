//! Validate proposal results and freeze the ledger displayed before their gate opens.
use super::pages;
use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::intent::{
    ApprovedIntent, IntentArtifact, IntentSource, ReferenceImage, Requirement,
};
use archon_core::run::{OperatorGate, PipelineRun, StageOutcome, StageRun, StageState};
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
    #[serde(default)]
    linked_images: bool,
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
    assert_eq!(gate.run, run.id, "freeze the gate's own run");
    assert_eq!(gate.item, run.item, "freeze the gate's own item");
    if store.kv_get(&key(gate))?.is_some() {
        let saved = pending(store, gate)?;
        verify_page(&saved, &saved.page)?;
        return Ok(saved.page);
    }
    let stage = stages
        .iter()
        .find(|s| {
            s.stage_id == gate.stage_id
                && s.attempt == gate.attempt
                && s.state == StageState::Completed
        })
        .ok_or_else(|| ArchonError::NotFound("proposal stage".into()))?;
    let (mut record, html) = prepare_record(ctx, run, stage, original)?;
    let destination = ctx.paths.data_dir.join("approved-intent-artifacts");
    let page = render_page(&run.workspace, original, &html, &record, gate.attempt)?;
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
        linked_images: true,
    };
    let json = serde_json::to_string(&saved)?;
    store.kv_set(&key(gate), &json)?;
    Ok(page)
}

fn make_record(run: &PipelineRun, stage: &StageRun) -> Result<ApprovedIntent, ArchonError> {
    assert_eq!(stage.run, run.id, "the proposal belongs to this run");
    assert!(stage.attempt > 0, "proposal attempts are positive");
    let draft = stage
        .structured
        .as_ref()
        .and_then(|s| s.get("approved_intent"))
        .ok_or_else(|| {
            ArchonError::Invalid("proposal must supply structured.approved_intent".into())
        })?;
    let draft: Draft = serde_json::from_value(draft.clone())
        .map_err(|e| ArchonError::Invalid(format!("proposal intent draft: {e}")))?;
    let record = ApprovedIntent {
        id: run.id.to_string(),
        version: stage.attempt,
        operator_words: draft.operator_words,
        source: IntentSource::Gate {
            item: run.item.to_string(),
            gate: format!("{}:{}", stage.stage_id, stage.attempt),
        },
        artifacts: draft.artifacts,
        images: draft.images,
        requirements: draft.requirements,
    };
    Ok(record)
}

fn prepare_record(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    original: &Path,
) -> Result<(ApprovedIntent, String), ArchonError> {
    let mut record = make_record(run, stage)?;
    let html =
        std::fs::read_to_string(original).map_err(|e| ArchonError::io("read proposal page", e))?;
    let destination = ctx.paths.data_dir.join("approved-intent-artifacts");
    let rewritten =
        super::intent_html::extract(&html, original, &run.workspace, &destination, &mut record)?;
    let rewritten = super::intent_images::extract(&rewritten, &destination, &mut record)?;
    record.validate()?;
    preserve_files(ctx, &run.workspace, &mut record)?;
    let hash = Sha256::digest(html.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let preserved = crate::handlers::intent::preserve(&destination, original, &hash)?;
    if !record.artifacts.iter().any(|a| a.sha256 == hash) {
        record.artifacts.push(IntentArtifact {
            path: preserved,
            sha256: hash,
        });
    }
    assert!(
        !record.requirements.is_empty(),
        "a validated ledger has requirements"
    );
    assert!(record.artifacts.iter().all(|a| a.path.is_absolute()));
    Ok((record, rewritten))
}

const REFUSAL: &str = "proposal intent draft refused: ";

pub(super) fn is_refusal(error: &str) -> bool {
    error.starts_with(REFUSAL)
}

pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    assert_eq!(stage.run, run.id, "verify the proposal in its own run");
    assert!(
        run.workspace.is_absolute(),
        "proposal paths use an absolute workspace"
    );
    let StageOutcome::Completed { structured, .. } = &outcome else {
        return Ok(outcome);
    };
    let proposal = {
        let table = ctx.runs.lock().await;
        table
            .drivers
            .get(&run.id)
            .is_some_and(|d| d.gates().proposal.as_deref() == Some(&stage.stage_id))
    };
    if !proposal {
        return Ok(outcome);
    }
    let item = ctx.store.lock().await.item(&run.item)?;
    if item.intent.is_none() && !item.model_plan.0.contains_key("intent-acceptance") {
        return Ok(outcome);
    }
    let mut completed = stage.clone();
    completed.structured = structured.clone();
    let page = pages::listed_pages(run, &completed)
        .into_iter()
        .find_map(Result::ok)
        .unwrap_or_else(|| {
            run.workspace.join(pages::conventional_page(
                archon_core::run::GateSubject::Proposal,
            ))
        });
    match prepare_record(ctx, run, &completed, &page) {
        Ok(_) => Ok(outcome),
        Err(error) if error.is_store() => Err(error),
        Err(error) => Ok(StageOutcome::Failed {
            error: format!(
                "{REFUSAL}{error}. Correct structured.approved_intent and its files \
                in the existing worktree, then resubmit the proposal result. \
                Allowed requirement kinds: visual, behaviour, wording, data (behavior is also accepted)."
            ),
            infra: false,
        }),
    }
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
    workspace: &Path,
    original: &Path,
    html: &str,
    record: &ApprovedIntent,
    attempt: u32,
) -> Result<PathBuf, ArchonError> {
    let content = archon_lavish::intent::proposal(record, html)?;
    let directory = original.parent().expect("the proposal page has a parent");
    pages::inside_workspace(workspace, &directory.to_string_lossy())
        .map_err(ArchonError::Invalid)?;
    for image in &record.images {
        let saved = crate::handlers::intent::preserve(directory, &image.path, &image.sha256)?;
        assert_eq!(saved.file_name(), image.path.file_name());
    }
    let page = original.with_file_name(format!("archon-intent-{attempt}.html"));
    crate::handlers::intent::save_bytes(&page, content.as_bytes())?;
    assert_eq!(page.parent(), Some(directory));
    Ok(page)
}

pub(super) fn verify_page(saved: &Pending, page: &Path) -> Result<(), ArchonError> {
    assert!(
        page.is_absolute(),
        "a frozen page names the exact file on disk"
    );
    let bytes = std::fs::read(page).map_err(|e| ArchonError::io("read proposal ledger page", e))?;
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
    if saved.linked_images {
        for image in &saved.record.images {
            let path =
                page.with_file_name(image.path.file_name().expect("preserved image filename"));
            if super::acceptance::images::reference_hash(&path)? != image.sha256 {
                return Err(ArchonError::Invalid(format!(
                    "proposal image {} changed after it was shown",
                    image.id
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{Requirement, RequirementKind};

    #[test]
    fn frozen_page_and_images_cannot_be_changed_after_presentation() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("index.html");
        std::fs::write(&original, "<body>Proposal</body>").unwrap();
        let image = b"\x89PNG\r\n\x1a\nexample";
        let hash = Sha256::digest(image)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let asset = format!("{hash}.png");
        let sources = dir.path().join("sources");
        std::fs::create_dir(&sources).unwrap();
        std::fs::write(sources.join(&asset), image).unwrap();
        let record = ApprovedIntent {
            id: "x".into(),
            version: 1,
            operator_words: "Build this".into(),
            source: IntentSource::Inbox { seq: 1 },
            artifacts: vec![],
            images: vec![ReferenceImage {
                id: "screen".into(),
                path: sources.join(&asset),
                sha256: hash,
                viewport: None,
                theme: None,
                state: None,
            }],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Title".into(),
                kind: RequirementKind::Wording,
                images: vec![],
                owner: "app".into(),
            }],
        };
        let page = render_page(dir.path(), &original, "<body>Proposal</body>", &record, 1).unwrap();
        let bytes = std::fs::read(&page).unwrap();
        let saved = Pending {
            record,
            page,
            page_sha256: Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            linked_images: true,
        };
        assert!(verify_page(&saved, &saved.page).is_ok());
        let asset = saved.page.with_file_name(asset);
        std::fs::write(&asset, b"\x89PNG\r\n\x1a\nchanged").unwrap();
        assert!(
            verify_page(&saved, &saved.page)
                .unwrap_err()
                .to_string()
                .contains("proposal image screen changed")
        );
        std::fs::write(&asset, image).unwrap();
        std::fs::write(&saved.page, "changed").unwrap();
        assert!(
            verify_page(&saved, &saved.page)
                .unwrap_err()
                .to_string()
                .contains("ledger page changed")
        );
    }
}
