//! Runtime enforcement of approved intent, independent of review and CI.
mod deviations;
mod images;
mod pages;
mod records;
mod spawn;

pub(super) use deviations::{pending, resume_decisions};
pub(crate) use records::{readiness, readiness_text, require_current};
pub(super) use spawn::{attach, section, select_model};

use archon_core::acceptance::{AcceptanceRecord, AcceptanceResult, RequirementResult};
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, IntentBinding};
use archon_core::queue::QueueItem;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_pipeline::acceptance::STAGE;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildEvidence {
    head: String,
    tree: String,
    intent: IntentBinding,
    requirements: Vec<RequirementResult>,
    images: Vec<images::ImageProof>,
}

fn intent(store: &Store, item: &QueueItem) -> Result<Option<ApprovedIntent>, ArchonError> {
    let Some(binding) = &item.intent else {
        return Ok(None);
    };
    let record = store.approved_intent(binding)?.ok_or_else(|| {
        ArchonError::NotFound(format!(
            "approved intent {} v{}",
            binding.id, binding.version
        ))
    })?;
    record.selected_requirements(binding)?;
    assert_eq!(record.id, binding.id);
    assert_eq!(record.version, binding.version);
    Ok(Some(record))
}

pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    records_build: bool,
    outcome: StageOutcome,
) -> Result<Option<StageOutcome>, ArchonError> {
    let StageOutcome::Completed { .. } = &outcome else {
        return Ok(Some(outcome));
    };
    let item = ctx.store.lock().await.item(&run.item)?;
    if item.intent.is_none() {
        return Ok(Some(outcome));
    }
    let acceptance = stage.stage_id == STAGE;
    if !records_build && !acceptance {
        return Ok(Some(outcome));
    }
    let record = intent(&*ctx.store.lock().await, &item)?.expect("binding checked above");
    if records_build || acceptance {
        records::invalidate(&mut *ctx.store.lock().await, &item.id)?;
    }
    let checked = if acceptance {
        verify_acceptance(ctx, run, stage, &item, &record, outcome).await
    } else {
        verify_build(ctx, run, stage, &item, &record, outcome).await
    };
    match checked {
        Err(ArchonError::Invalid(error) | ArchonError::NotFound(error)) => {
            if acceptance && records::load(&*ctx.store.lock().await, &item.id)?.is_none() {
                let (head, tree) = records::head(&run.workspace).await?;
                records::failed(
                    &mut *ctx.store.lock().await,
                    &item,
                    stage,
                    &head,
                    &tree,
                    "",
                    &error,
                )?;
            }
            Ok(Some(StageOutcome::Failed {
                error,
                infra: false,
            }))
        }
        other => other,
    }
}

async fn verify_build(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    item: &QueueItem,
    record: &ApprovedIntent,
    outcome: StageOutcome,
) -> Result<Option<StageOutcome>, ArchonError> {
    let StageOutcome::Completed { structured, .. } = &outcome else {
        unreachable!()
    };
    let binding = item.intent.as_ref().expect("intent required");
    let results = archon_pipeline::acceptance::build_results(record, binding, structured.as_ref())?;
    let (head, tree) = records::head(&run.workspace).await?;
    let evidence = BuildEvidence {
        head,
        tree,
        intent: binding.clone(),
        images: images::prove(record, &results, &run.workspace)?,
        requirements: results,
    };
    let key = format!("intent_build:{}", run.id);
    ctx.store
        .lock()
        .await
        .kv_set(&key, &serde_json::to_string(&evidence)?)?;
    if !deviations::authorize(ctx, run, stage, record, &evidence, &outcome).await? {
        return Ok(None);
    }
    assert!(!evidence.head.is_empty());
    assert_eq!(&evidence.intent, binding);
    Ok(Some(outcome))
}

async fn verify_acceptance(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    item: &QueueItem,
    record: &ApprovedIntent,
    mut outcome: StageOutcome,
) -> Result<Option<StageOutcome>, ArchonError> {
    let binding = item.intent.as_ref().expect("intent required");
    let evidence = build_evidence(ctx, run, record, binding, &outcome).await?;
    let (head, tree) = records::head(&run.workspace).await?;
    if evidence.head != head || evidence.tree != tree || evidence.intent != *binding {
        return Err(ArchonError::Invalid(
            "build evidence is stale; rebuild evidence for the current head and intent".into(),
        ));
    }
    images::verify(&evidence.images)?;
    spawn::check_independent(ctx, run, stage).await?;
    deviations::require_authorized(&*ctx.store.lock().await, run, &evidence)?;
    let StageOutcome::Completed { structured, .. } = &mut outcome else {
        unreachable!()
    };
    let judged =
        archon_pipeline::acceptance::judge(record, binding, structured.as_ref(), &head, &tree);
    let display = match &judged {
        Ok(_) => structured.clone(),
        Err(error) => Some(serde_json::json!({"acceptance":{"requirements":
            record.selected_requirements(binding)?.iter().map(|r|
                serde_json::json!({"id":r.id,"verdict":"fail","reason":error.to_string()})
            ).collect::<Vec<_>>()}})),
    };
    let page = pages::write(
        run,
        record,
        &evidence.requirements,
        display.as_ref(),
        "acceptance",
    )?;
    let (published, warning) = crate::reports::publish_page(
        ctx,
        "Approved-intent acceptance",
        std::path::Path::new(&page),
        Some(&item.id),
    )
    .await?;
    if let Some(warning) = warning {
        crate::feed::system(ctx, warning).await?;
    }
    record_judgment(ctx, item, stage, (&head, &tree), published.location, judged).await?;
    Ok(Some(outcome))
}

async fn record_judgment(
    ctx: &DaemonContext,
    item: &QueueItem,
    stage: &StageRun,
    (head, tree): (&str, &str),
    page: String,
    judged: Result<AcceptanceResult, ArchonError>,
) -> Result<(), ArchonError> {
    let result = match judged {
        Ok(result) => result,
        Err(error) => {
            records::failed(
                &mut *ctx.store.lock().await,
                item,
                stage,
                head,
                tree,
                &page,
                &error.to_string(),
            )?;
            return Err(error);
        }
    };
    let accepted = AcceptanceRecord {
        result,
        reviewer: stage
            .aether_agent
            .clone()
            .ok_or_else(|| ArchonError::Invalid("acceptance has no independent agent".into()))?,
        model: stage.choice.model.clone(),
        checked_at: archon_core::now(),
        page,
    };
    records::save(&mut *ctx.store.lock().await, &item.id, &accepted)?;
    assert_eq!(stage.kind, Some(StageKind::Test));
    assert_eq!(Some(&accepted.result.intent), item.intent.as_ref());
    Ok(())
}

async fn build_evidence(
    ctx: &DaemonContext,
    run: &PipelineRun,
    record: &ApprovedIntent,
    binding: &IntentBinding,
    outcome: &StageOutcome,
) -> Result<BuildEvidence, ArchonError> {
    let key = format!("intent_build:{}", run.id);
    if let Some(raw) = ctx.store.lock().await.kv_get(&key)? {
        return serde_json::from_str(&raw).map_err(|e| ArchonError::Corrupt(format!("{key}: {e}")));
    }
    // A validation-only pipeline supplies its observations directly.
    let StageOutcome::Completed { structured, .. } = outcome else {
        unreachable!()
    };
    let requirements =
        archon_pipeline::acceptance::build_results(record, binding, structured.as_ref())?;
    let (head, tree) = records::head(&run.workspace).await?;
    let images = images::prove(record, &requirements, &run.workspace)?;
    assert!(!requirements.is_empty());
    assert!(!head.is_empty());
    Ok(BuildEvidence {
        head,
        tree,
        intent: binding.clone(),
        requirements,
        images,
    })
}

pub(super) async fn publish_deviations(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<(), ArchonError> {
    let (item, record, evidence, assumptions) = {
        let store = ctx.store.lock().await;
        let item = store.item(&run.item)?;
        let record = intent(&store, &item)?;
        let evidence = store.kv_get(&format!("intent_build:{}", run.id))?;
        let assumptions = crate::clarifications::review_section(&store, &run.item)?;
        (item, record, evidence, assumptions)
    };
    let evidence = evidence
        .map(|raw| serde_json::from_str::<BuildEvidence>(&raw))
        .transpose()
        .map_err(|e| ArchonError::Corrupt(format!("build evidence: {e}")))?;
    let has_deviations = evidence.as_ref().is_some_and(|e| {
        e.requirements
            .iter()
            .any(|r| r.status == archon_core::acceptance::RequirementStatus::Deviation)
    });
    if !has_deviations && assumptions.is_empty() {
        return Ok(());
    }
    let page = match (record, evidence) {
        (Some(record), Some(evidence)) => {
            pages::write(run, &record, &evidence.requirements, None, "deviations")?
        }
        _ => {
            let path = run.workspace.join(".archon/reports/deviations/index.html");
            pages::directory_in_workspace(&run.workspace, path.parent().expect("page has parent"))?;
            std::fs::write(&path, "<!doctype html><meta charset=utf-8><title>Deviations to review</title><h1>Deviations to review</h1>").map_err(|e| ArchonError::io("write deviations report", e))?;
            path.display().to_string()
        }
    };
    pages::append_decisions(
        &page,
        &assumptions,
        &deviations::summary(&*ctx.store.lock().await, run)?,
    )?;
    let (_, warning) = crate::reports::publish_page(
        ctx,
        "Deviations to review",
        std::path::Path::new(&page),
        Some(&item.id),
    )
    .await?;
    if let Some(warning) = warning {
        crate::feed::system(ctx, warning).await?;
    }
    assert!(std::path::Path::new(&page).is_file());
    assert!(has_deviations || !assumptions.is_empty());
    Ok(())
}
