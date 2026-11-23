use crate::context::DaemonContext;
use archon_core::acceptance::{AcceptanceReadiness, AcceptanceRecord, AcceptanceStatus};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::QueueItem;
use archon_core::run::PipelineRun;
use archon_queue::Store;
use std::path::Path;

fn key(item: &QueueItemId) -> String {
    format!("intent_acceptance:{item}")
}

pub(super) fn save(
    store: &mut Store,
    item: &QueueItemId,
    record: &AcceptanceRecord,
) -> Result<(), ArchonError> {
    if record.result.head.is_empty() || record.result.tree.is_empty() {
        return Err(ArchonError::Invalid(
            "acceptance must identify head and tree".into(),
        ));
    }
    store.kv_set(&key(item), &serde_json::to_string(record)?)
}

pub(super) fn load(
    store: &Store,
    item: &QueueItemId,
) -> Result<Option<AcceptanceRecord>, ArchonError> {
    store
        .kv_get(&key(item))?
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|e| ArchonError::Corrupt(format!("acceptance {item}: {e}")))
        })
        .transpose()
}

pub(super) async fn head(workspace: &Path) -> Result<(String, String), ArchonError> {
    let workspace = workspace.to_owned();
    tokio::task::spawn_blocking(move || {
        let head = crate::worktree::git(&workspace, &["rev-parse", "HEAD"])?;
        let tree = crate::worktree::git(&workspace, &["rev-parse", "HEAD^{tree}"])?;
        assert!(!head.is_empty());
        assert!(!tree.is_empty());
        Ok((head, tree))
    })
    .await
    .map_err(|e| ArchonError::Unavailable(format!("reading acceptance head: {e}")))?
}

pub(crate) async fn readiness(
    ctx: &DaemonContext,
    item: &QueueItem,
) -> Result<AcceptanceReadiness, ArchonError> {
    if item.intent.is_none() {
        return Ok(AcceptanceReadiness::default());
    }
    let (record, latest) = {
        let store = ctx.store.lock().await;
        (
            load(&store, &item.id)?,
            store.runs_for_item(&item.id)?.pop(),
        )
    };
    let current = match latest {
        Some(run) if run.workspace.exists() => Some(head(&run.workspace).await?),
        Some(run) => branch_head(ctx, item, &run.branch).await?,
        None => None,
    };
    let (head, tree) = current.unwrap_or_default();
    Ok(AcceptanceReadiness::at_head(
        item.intent.as_deref(),
        record,
        &head,
        &tree,
    ))
}

pub(crate) async fn require_current(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<String, ArchonError> {
    let item = ctx.store.lock().await.item(&run.item)?;
    let view = readiness(ctx, &item).await?;
    if !matches!(
        view.status,
        AcceptanceStatus::Passed | AcceptanceStatus::NotRequired
    ) {
        return Err(ArchonError::Invalid(format!(
            "approved-intent acceptance is {:?}; current head, tree and intent version must pass before delivery",
            view.status
        )));
    }
    assert!(view.status != AcceptanceStatus::Passed || view.record.is_some());
    assert!(view.status != AcceptanceStatus::NotRequired || item.intent.is_none());
    Ok(readiness_text(&view))
}

pub(crate) fn readiness_text(view: &AcceptanceReadiness) -> String {
    let pin = view
        .record
        .as_ref()
        .map(|r| {
            format!(
                "; head `{}`, tree `{}`, intent `{}` v{}",
                r.result.head, r.result.tree, r.result.intent.id, r.result.intent.version
            )
        })
        .unwrap_or_default();
    let honesty = match &view.record {
        Some(r) if !r.independent => format!(
            "; acceptance: not independent (judged by `{}`, which shares a model or vendor with a \
             builder)",
            r.model
        ),
        _ => String::new(),
    };
    format!(
        "Approved-intent acceptance: {:?}{pin}{honesty}. Code review and CI are separate checks.",
        view.status
    )
}

pub(super) fn failed(
    store: &mut Store,
    item: &QueueItem,
    stage: &archon_core::run::StageRun,
    head: &str,
    tree: &str,
    page: &str,
    reason: &str,
) -> Result<(), ArchonError> {
    let binding = item.intent.as_deref().expect("acceptance requires intent");
    let intent = super::intent(store, item)?.expect("acceptance requires intent");
    let requirements = intent
        .selected_requirements(binding)?
        .into_iter()
        .map(|r| archon_core::acceptance::RequirementJudgment {
            id: r.id.clone(),
            verdict: archon_core::acceptance::AcceptanceVerdict::Fail,
            reason: reason.into(),
        })
        .collect();
    let result = archon_core::acceptance::AcceptanceResult {
        head: head.into(),
        tree: tree.into(),
        intent: binding.clone(),
        requirements,
    };
    let reviewer = stage
        .aether_agent
        .clone()
        .ok_or_else(|| ArchonError::Invalid("acceptance has no agent".into()))?;
    let builders = super::spawn::builder_models(&store.stage_runs(&stage.run)?);
    let record = AcceptanceRecord {
        result,
        reviewer,
        model: stage.choice.model.clone(),
        checked_at: archon_core::now(),
        page: page.into(),
        independent: !super::spawn::shares_builder(&stage.choice.model, &builders),
    };
    assert!(!record.result.requirements.is_empty());
    assert!(!reason.is_empty());
    save(store, &item.id, &record)
}

pub(super) fn invalidate(store: &mut Store, item: &QueueItemId) -> Result<(), ArchonError> {
    store.kv_delete(&key(item))
}

async fn branch_head(
    ctx: &DaemonContext,
    item: &QueueItem,
    branch: &str,
) -> Result<Option<(String, String)>, ArchonError> {
    let Some(id) = &item.repo else {
        return Ok(None);
    };
    let repo = ctx.store.lock().await.repo(&crate::resolve::repo_ref(id))?;
    let branch = branch.to_owned();
    tokio::task::spawn_blocking(move || {
        if !crate::worktree::branch_exists(&repo.path, &branch)? {
            return Ok(None);
        }
        let reference = format!("refs/heads/{branch}");
        let head = crate::worktree::git(&repo.path, &["rev-parse", "--verify", &reference])?;
        let tree = crate::worktree::git(
            &repo.path,
            &["rev-parse", "--verify", &format!("{reference}^{{tree}}")],
        )?;
        assert!(!head.is_empty());
        assert!(!tree.is_empty());
        Ok(Some((head, tree)))
    })
    .await
    .map_err(|e| ArchonError::Unavailable(format!("reading accepted branch: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::acceptance::{AcceptanceResult, AcceptanceVerdict, RequirementJudgment};
    use archon_core::intent::IntentBinding;

    fn passed(independent: bool) -> AcceptanceReadiness {
        let record = AcceptanceRecord {
            result: AcceptanceResult {
                head: "a".repeat(40),
                tree: "b".repeat(40),
                intent: IntentBinding {
                    id: "i".into(),
                    version: 1,
                    requirements: vec!["R1".into()],
                },
                requirements: vec![RequirementJudgment {
                    id: "R1".into(),
                    verdict: AcceptanceVerdict::Pass,
                    reason: "compared".into(),
                }],
            },
            reviewer: archon_core::aether_ids::AetherAgentId::parse("agt_judge").unwrap(),
            model: "claude-code/builder".parse().unwrap(),
            checked_at: archon_core::now(),
            page: String::new(),
            independent,
        };
        AcceptanceReadiness {
            status: AcceptanceStatus::Passed,
            record: Some(record),
        }
    }

    #[test]
    fn a_fallback_judgment_is_reported_not_independent_and_an_independent_one_is_not() {
        let fallback = readiness_text(&passed(false));
        assert!(
            fallback.contains("acceptance: not independent"),
            "{fallback}"
        );
        assert!(fallback.contains("`claude-code/builder`"), "{fallback}");
        assert!(!readiness_text(&passed(true)).contains("not independent"));
    }
}
