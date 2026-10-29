//! Ending the processes a settled stage left under its worktree, which
//! escaped the engine's process group (double-forked daemons, `nohup`).
//! The services a stage declared for a later one are kept until the run ends.

use std::collections::BTreeSet;
use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::FeedEntry;

use super::FollowUp;
use crate::context::DaemonContext;

/// Reap the worktree of the run `stage` belongs to, once no other stage of
/// the run is live, and record what was ended or survived in the stage run
/// and the feed. A replacement not yet sent to Aether runs nothing yet. The
/// services the run declared are kept while it goes on.
pub(super) async fn stage_end(
    ctx: &DaemonContext,
    stage: &StageRunId,
) -> Result<FollowUp, ArchonError> {
    let (workspace, keep) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(stage)?;
        assert!(stage.state.is_terminal(), "only a settled stage is reaped");
        let run = store.run(&stage.run)?;
        let mut siblings_live = false;
        for s in store.stage_runs(&run.id)? {
            let live = s.id != stage.id && !s.state.is_terminal();
            siblings_live |= live && !super::spawn::never_sent(&store, &s.id)?;
        }
        let keep = match (run.state.is_terminal(), run.team_root.is_some()) {
            (true, _) => Keep::Nothing,
            (false, false) => Keep::Worktree,
            (false, true) => Keep::TeamWorktrees,
        };
        let workspace = run.team_root.unwrap_or(run.workspace);
        // Only the trees Archon made are reaped, never a checkout elsewhere.
        if siblings_live || !workspace.starts_with(&ctx.paths.worktrees) {
            return Ok(FollowUp::default());
        }
        (workspace, keep)
    };
    let report = reap(ctx, &workspace, keep).await?;
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

/// Reap `tree`, a team run's whole directory or else its worktree, once its
/// run ended: the services it declared go too. Told in the feed only.
pub(super) async fn run_end(ctx: &DaemonContext, tree: &Path) -> Result<FollowUp, ArchonError> {
    assert!(
        tree.starts_with(&ctx.paths.worktrees),
        "only Archon's trees are reaped"
    );
    assert!(
        tree != ctx.paths.worktrees,
        "a run's tree lies below the root"
    );
    let report = reap(ctx, tree, Keep::Nothing).await?;
    if report.is_empty() {
        return Ok(FollowUp::default());
    }
    let text = format!("{} Its run ended.", report.describe(tree));
    tracing::warn!("{text}");
    crate::feed::append(ctx, FeedEntry::System { text }).await?;
    Ok(FollowUp::default())
}

/// Which services a reap of a run's tree keeps.
enum Keep {
    /// None: the run ended.
    Nothing,
    /// Those declared in the tree, a run worktree.
    Worktree,
    /// Those declared in any worktree under the tree, a team run's root.
    TeamWorktrees,
}

/// Reap `tree` off the async runtime, keeping the services `keep` names.
async fn reap(
    ctx: &DaemonContext,
    tree: &Path,
    keep: Keep,
) -> Result<crate::reaper::Report, ArchonError> {
    assert!(tree.is_absolute(), "worktrees are absolute");
    assert!(
        tree.starts_with(&ctx.paths.worktrees),
        "only Archon's trees are reaped"
    );
    let (root, tree) = (ctx.paths.worktrees.clone(), tree.to_path_buf());
    let reaped = tokio::task::spawn_blocking(move || {
        let kept = match keep {
            Keep::Nothing => BTreeSet::new(),
            Keep::Worktree => crate::reaper::services::declared(&tree)?,
            Keep::TeamWorktrees => crate::reaper::services::declared_in_team(&tree)?,
        };
        crate::reaper::reap(&root, &tree, &kept)
    });
    reaped
        .await
        .map_err(|e| ArchonError::io("process reaping task", std::io::Error::other(e)))?
}
