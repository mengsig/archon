//! The hosts a preview may load besides loopback: `[design] allow_hosts` of
//! the repository's committed `.archon/repo.toml` at the run's base commit,
//! read through sealed git. The worktree's copy is the agent's and never read.

use std::path::Path;

use archon_core::repo::RepoTiers;
use archon_core::repo::tiers::valid_allow_host;

use super::WorktreeRef;
use crate::context::DaemonContext;
use crate::worktree::daemon_git::DaemonGit;

/// The allowed extra hosts; none when the file, its commit or its table is
/// missing or unreadable.
pub(crate) async fn allow_hosts(
    ctx: &DaemonContext,
    wt: &WorktreeRef,
    _data_dir: &Path,
) -> Vec<String> {
    let base_branch = {
        let store = ctx.store.lock().await;
        store
            .repos()
            .ok()
            .and_then(|repos| repos.into_iter().find(|r| r.path == wt.repo))
            .and_then(|r| r.base_branch)
    };
    let worktree = wt.worktree.clone();
    let repo = wt.repo.clone();
    let result = tokio::task::spawn_blocking(move || read(&repo, &worktree, base_branch)).await;
    match result {
        Ok(hosts) => hosts,
        Err(_) => Vec::new(),
    }
}

fn read(repo: &Path, worktree: &Path, base_branch: Option<String>) -> Vec<String> {
    let Some(head) = DaemonGit::sealed_in(worktree)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .text()
        .ok()
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = base_branch.into_iter().collect();
    names.extend(["main".to_owned(), "master".to_owned()]);
    let base = names.iter().find_map(|name| {
        crate::worktree::branch_fork(repo, name, &head)
            .ok()
            .flatten()
    });
    let Some(base) = base else {
        return Vec::new();
    };
    let spec = format!("{base}:.archon/repo.toml");
    let Ok(text) = DaemonGit::sealed_in(repo).args(["show", &spec]).text() else {
        return Vec::new();
    };
    match RepoTiers::parse(&text, ".archon/repo.toml at the run's base commit") {
        Ok(tiers) => tiers
            .design
            .allow_hosts
            .into_iter()
            .filter(|h| valid_allow_host(h))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "design: the committed repo.toml is unreadable; no extra hosts");
            Vec::new()
        }
    }
}
