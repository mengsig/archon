use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
struct Proposal {
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
