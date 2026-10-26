//! Read-only stage evidence. Classify from each run's frozen spec, then the
//! current catalog; unavailable definitions retain observations without verdicts.

use std::collections::{HashMap, hash_map::Entry};

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model_evidence::{ModelEvidenceQuery, ModelEvidenceReport};
use archon_core::protocol::Reply;
use archon_core::run::StageState;
use archon_pipeline::PipelineSpec;

use crate::context::DaemonContext;
use crate::executor::frozen;
use crate::resolve::{RepoEnv, repo_ref};

pub(super) async fn report(
    ctx: &DaemonContext,
    query: &ModelEvidenceQuery,
) -> Result<Reply, ArchonError> {
    let today = archon_core::now()
        .with_timezone(&chrono::Local)
        .date_naive();
    let range = query.range(today)?;
    let snapshot = ctx.store.lock().await.model_evidence_snapshot(
        query,
        &range,
        archon_core::cost::API_EQUIVALENT,
    )?;
    let mut runs = HashMap::new();
    let mut catalogs = HashMap::new();
    for (item, pipeline, stage) in snapshot.attempts() {
        if stage.state != StageState::Completed || stage.verdict.is_none() {
            continue;
        }
        let Entry::Vacant(entry) = runs.entry(stage.run.clone()) else {
            continue;
        };
        let spec = match frozen::thaw(ctx, &stage.run, pipeline).await {
            Ok(spec) => Some(spec),
            Err(error) => {
                tracing::debug!(run = %stage.run, %error, "evidence snapshot unavailable; using catalog");
                let key = (item.repo.clone(), pipeline.to_owned());
                let spec = match catalogs.entry(key) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let spec = current_spec(ctx, item.repo.as_ref(), pipeline).await?;
                        entry.insert(spec)
                    }
                };
                spec.clone()
            }
        };
        entry.insert(spec);
    }
    let catalog = ctx.store.lock().await.model_catalog();
    let mut report = snapshot.report(&catalog, |_, stage| {
        Ok(runs
            .get(&stage.run)
            .and_then(Option::as_ref)
            .and_then(|spec| spec.stage(&stage.stage_id))
            .is_some_and(|s| matches!(s.kind, StageKind::Review | StageKind::Test)))
    })?;
    if ctx.config.learning.enabled {
        add_pipelines(ctx, query, &mut report).await?;
    }
    Ok(Reply::ModelEvidence(Box::new(report)))
}

/// Learning's per-pipeline evidence, with the names of the repositories it
/// cites so the report names them as `cost` does.
async fn add_pipelines(
    ctx: &DaemonContext,
    query: &ModelEvidenceQuery,
    report: &mut ModelEvidenceReport,
) -> Result<(), ArchonError> {
    assert!(report.pipelines.is_empty(), "pipelines are added once");
    let store = ctx.store.lock().await;
    let repo = query.repo.as_ref().map(|r| store.repo(r)).transpose()?;
    let stats = store.learning_stats(repo.as_ref().map(|r| &r.id))?;
    let cited: Vec<&RepoId> = stats
        .pipelines
        .iter()
        .filter_map(|p| p.repo.as_ref())
        .collect();
    report.repo_names = stats
        .repo_names
        .into_iter()
        .filter(|(id, _)| cited.contains(&id))
        .collect();
    report.pipelines = stats.pipelines;
    debug_assert!(report.repo_names.len() <= report.pipelines.len());
    Ok(())
}

async fn current_spec(
    ctx: &DaemonContext,
    repo: Option<&RepoId>,
    pipeline: &str,
) -> Result<Option<PipelineSpec>, ArchonError> {
    let repo = match repo {
        Some(id) => match ctx.store.lock().await.repo(&repo_ref(id)) {
            Ok(repo) => Some(repo),
            Err(ArchonError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        },
        None => None,
    };
    // A historical definition may have been removed or broken. Unknown kinds
    // safely retain attempts/cost/time while excluding verdicts from pass shares.
    match RepoEnv::new(repo)
        .and_then(|env| env.catalog())
        .and_then(|catalog| catalog.get(pipeline).cloned())
    {
        Ok(spec) => Ok(Some(spec)),
        Err(error) => {
            tracing::debug!(pipeline, %error, "evidence stage kinds unavailable");
            Ok(None)
        }
    }
}
