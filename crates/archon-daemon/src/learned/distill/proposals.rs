use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
pub(super) struct Proposal {
    name: String,
    files: BTreeMap<String, String>,
}

pub(super) async fn stage(
    ctx: &DaemonContext,
    value: serde_json::Value,
) -> Result<(), ArchonError> {
    let proposal: Proposal = serde_json::from_value(value)
        .map_err(|e| ArchonError::Invalid(format!("pipeline proposal: {e}")))?;
    let root = archon_core::paths::config_dir().join("pipelines");
    let path = archon_pipeline::proposals::stage(&root, &proposal.name, &proposal.files)?;
    assert!(path.starts_with(&root));
    assert!(path.ends_with(format!("{}.proposed", proposal.name)));
    crate::executor::notify(ctx,archon_core::notify::NotificationKind::Outcome,
        &format!("Proposed pipeline {} validated. Inspect {} then activate with archon pipelines accept {}.",
            proposal.name,path.display(),proposal.name),None,None).await
}

pub(super) fn validate(value: &serde_json::Value) -> Result<(), ArchonError> {
    let proposal: Proposal = serde_json::from_value(value.clone())
        .map_err(|e| ArchonError::Invalid(format!("pipeline proposal: {e}")))?;
    let spec = archon_pipeline::proposals::validate(&proposal.name, &proposal.files)?;
    assert_eq!(spec.name(), proposal.name);
    assert!(!proposal.files.is_empty());
    Ok(())
}

pub(super) async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let pending = ctx
        .store
        .lock()
        .await
        .kv_prefix("learning/proposal-pending/")?;
    for (key, raw) in pending {
        let value = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("proposal outbox: {e}")))?;
        match stage(ctx, value).await {
            Ok(()) => ctx.store.lock().await.kv_delete(&key)?,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::warn!(%error, %key, "proposal publication deferred");
                super::report_error(ctx, &error).await?;
            }
        }
    }
    Ok(())
}
