//! After a pass removed run worktrees: Aether deletes the workspaces it
//! delegated to the agents that used them (`storage_gc`; Archon never deletes
//! those), then git forgets every worktree entry whose directory is gone.
//! Neither failure fails the pass; both are reported.

use std::collections::BTreeSet;
use std::path::PathBuf;

use archon_core::protocol::{AetherGc, GcReport};

use crate::context::DaemonContext;
use crate::worktree::git;

/// Ask Aether to delete the workspaces of its finished agents. The call ends
/// at the Aether client's request timeout (60s): its sweep reads and removes
/// directories, so it may take a while, but the gc lock is held meanwhile.
pub(super) async fn storage_gc(ctx: &DaemonContext) -> AetherGc {
    let client = match ctx.aether.client() {
        Ok(client) => client,
        Err(error) => return failed(format!("Aether is not reachable: {error}")),
    };
    match client.storage_gc().await {
        Ok(report) => {
            tracing::debug!(%report, "aether storage gc ran");
            AetherGc::Ran
        }
        Err(error) => failed(error.to_string()),
    }
}

fn failed(why: String) -> AetherGc {
    tracing::warn!(%why, "aether storage gc did not run");
    AetherGc::Failed(why)
}

/// `git worktree prune` in each of `repos`; a failure is reported against the
/// repository and the rest are still pruned.
pub(super) fn prune(repos: &BTreeSet<PathBuf>) -> GcReport {
    assert!(
        repos.iter().all(|repo| repo.is_absolute()),
        "repositories are absolute paths"
    );
    let mut report = GcReport::default();
    for repo in repos {
        if let Err(error) = git(repo, &["worktree", "prune"]) {
            report
                .failed
                .push((repo.clone(), format!("git worktree prune: {error}")));
        }
    }
    assert!(
        report.failed.len() <= repos.len(),
        "at most one failure per repository"
    );
    report
}
