//! Ending the processes a settled stage left under its worktree, which
//! escaped the engine's process group (double-forked daemons, `nohup`).

use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::FeedEntry;

use super::FollowUp;
use crate::context::DaemonContext;

/// Reap the worktree of the run `stage` belongs to, once no other stage of
/// the run is live, and record what was ended or survived in the stage run
/// and the feed.
pub(super) async fn stage_end(
    ctx: &DaemonContext,
    stage: &StageRunId,
) -> Result<FollowUp, ArchonError> {
    let workspace = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(stage)?;
        assert!(stage.state.is_terminal(), "only a settled stage is reaped");
        let run = store.run(&stage.run)?;
        let siblings_live = store
            .stage_runs(&run.id)?
            .iter()
            .any(|s| s.id != stage.id && !s.state.is_terminal());
        if siblings_live || !run.workspace.is_absolute() {
            return Ok(FollowUp::default());
        }
        run.team_root.unwrap_or(run.workspace)
    };
    let tree = workspace.clone();
    let report = tokio::task::spawn_blocking(move || crate::reaper::reap(&tree))
        .await
        .map_err(|e| ArchonError::io("process reaping task", std::io::Error::other(e)))??;
    if report.is_empty() {
        return Ok(FollowUp::default());
    }
    let text = report.describe(&workspace);
    tracing::warn!(%stage, "{text}");
    {
        // Read again: the reap ran unlocked and may have taken seconds.
        let mut store = ctx.store.lock().await;
        let mut record = store.stage_run(stage)?;
        assert!(record.state.is_terminal(), "a settled stage stays settled");
        record.summary = Some(match record.summary.take() {
            Some(summary) => format!("{summary}\n\n{text}"),
            None => text.clone(),
        });
        store.upsert_stage_run(&record)?;
    }
    crate::feed::append(ctx, FeedEntry::System { text }).await?;
    Ok(FollowUp::default())
}
