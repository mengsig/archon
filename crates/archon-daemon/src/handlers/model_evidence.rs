//! Read-only stage evidence. Classify from each run's frozen spec, then the
//! current catalog; unavailable definitions retain observations without verdicts.

use std::collections::{HashMap, hash_map::Entry};

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model_evidence::ModelEvidenceQuery;
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
        ctx.config.limits.count_estimated_cost,
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
    let mut report = snapshot.report(|_, stage| {
        Ok(runs
            .get(&stage.run)
            .and_then(Option::as_ref)
            .and_then(|spec| spec.stage(&stage.stage_id))
            .is_some_and(|s| matches!(s.kind, StageKind::Review | StageKind::Test)))
    })?;
    if ctx.config.learning.enabled {
        let store = ctx.store.lock().await;
        let repo = query.repo.as_ref().map(|r| store.repo(r)).transpose()?;
        report.pipelines = store
            .learning_stats(repo.as_ref().map(|r| &r.id))?
            .pipelines;
    }
    Ok(Reply::ModelEvidence(Box::new(report)))
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
