//! A promotion whose merge was sent but whose post-merge work has not all
//! run, kept in `kv` by repository id: a later `promote` finishes it once
//! the release branch holds the head, as after a merge queue let it in.

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_queue::store::Store;
use serde::{Deserialize, Serialize};

use crate::worktree::stack;

const KEY_PREFIX: &str = "promoting_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pending {
    /// The integration branch and the release branch it was merged into.
    pub branch: String,
    pub release: String,
    /// The verified integration head the merge was sent for.
    pub head: String,
    pub pull_request: String,
}

fn key(repo: &RepoId) -> String {
    format!("{KEY_PREFIX}{repo}")
}

pub(super) fn load(store: &Store, repo: &RepoId) -> Result<Option<Pending>, ArchonError> {
    let key = key(repo);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let pending: Pending = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))?;
    assert!(stack::is_commit_id(&pending.head), "a saved verified head");
    assert!(!pending.pull_request.is_empty(), "a saved pull request");
    assert_ne!(pending.branch, pending.release, "a saved pair of branches");
    Ok(Some(pending))
}

pub(super) fn save(store: &mut Store, repo: &RepoId, pending: &Pending) -> Result<(), ArchonError> {
    assert!(stack::is_commit_id(&pending.head), "a verified head");
    assert!(!pending.pull_request.is_empty(), "the pull request merged");
    assert_ne!(pending.branch, pending.release, "two branches");
    store.kv_set(&key(repo), &serde_json::to_string(pending)?)
}

pub(super) fn clear(store: &mut Store, repo: &RepoId) -> Result<(), ArchonError> {
    store.kv_delete(&key(repo))
}

/// Drop what is kept for repositories no longer registered.
pub(crate) fn forget_unregistered(store: &mut Store, kept: &[RepoId]) -> Result<(), ArchonError> {
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let id = key.strip_prefix(KEY_PREFIX).expect("ranged by prefix");
        let id = RepoId::parse(id).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
        if !kept.contains(&id) {
            store.kv_delete(&key)?;
        }
    }
    Ok(())
}
