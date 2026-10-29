//! Who uses a run workspace. An item's worktree is also the workspace of
//! each restack fix and refresh of it (`Store::stack_fix`), and of every
//! retry of any of them, so whatever deletes a workspace asks here, by its
//! path, and not the state of the one item whose run left it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::protocol::ItemQuery;
use archon_queue::Store;

use super::WorktreeManager;

/// The item whose worktree `item` works in: the owner of a restack fix or
/// refresh, else the item itself.
pub(crate) fn owner(store: &Store, item: &QueueItemId) -> Result<QueueItemId, ArchonError> {
    let owner = store
        .stack_fix(item)?
        .map_or_else(|| item.clone(), |fix| fix.owner);
    Ok(owner)
}

/// The items of `owner`'s worktree: the owner and each restack fix and
/// refresh of it.
pub(crate) fn sharers(store: &Store, owner: &QueueItemId) -> Result<Vec<QueueItemId>, ArchonError> {
    let mut sharers = store.stack_fixes_of(owner)?;
    assert!(!sharers.contains(owner), "a fix repairs another item");
    sharers.push(owner.clone());
    Ok(sharers)
}

/// The places live items work in, or will once dispatched, as the store had
/// them at one moment: read it holding the run table, which dispatch takes.
#[derive(Debug, Default)]
pub(crate) struct LiveWorkspaces {
    places: Vec<(PathBuf, QueueItemId)>,
}

impl LiveWorkspaces {
    /// Each non-terminal item's worktree under `worktrees`, and every place a
    /// non-terminal run works in: its workspace and its team root.
    pub(crate) fn read(store: &Store, worktrees: &Path) -> Result<Self, ArchonError> {
        assert!(worktrees.is_absolute(), "the worktree root is absolute");
        let names: HashMap<RepoId, String> = store
            .repos()?
            .into_iter()
            .map(|repo| (repo.id, repo.name))
            .collect();
        let manager = WorktreeManager::new(worktrees.to_path_buf(), String::new());
        let query = ItemQuery {
            states: ["queued", "dispatching", "running", "blocked"]
                .map(String::from)
                .into(),
            limit: Some(usize::MAX),
            ..ItemQuery::default()
        };
        let mut places = Vec::new();
        for item in store.items(&query)? {
            assert!(!item.state.is_terminal(), "only live states were asked for");
            let Some(name) = item.repo.as_ref().and_then(|id| names.get(id)) else {
                continue;
            };
            places.push((manager.path_for(name, &owner(store, &item.id)?), item.id));
        }
        for run in store.active_runs()? {
            assert!(!run.state.is_terminal(), "the store lists live runs only");
            if let Some(root) = &run.team_root {
                places.push((root.clone(), run.item.clone()));
            }
            places.push((run.workspace, run.item));
        }
        // An empty path would hold every other.
        places.retain(|(path, _)| path.is_absolute());
        Ok(Self { places })
    }

    /// A live item that uses `path`: it works there, inside it, or in a
    /// place that holds it.
    pub(crate) fn user_of(&self, path: &Path) -> Option<&QueueItemId> {
        self.user_besides(path, None)
    }

    /// A live item but `except` that uses `path`, as [`Self::user_of`].
    pub(crate) fn user_besides(
        &self,
        path: &Path,
        except: Option<&QueueItemId>,
    ) -> Option<&QueueItemId> {
        assert!(path.is_absolute(), "workspaces are absolute");
        self.places
            .iter()
            .filter(|(_, item)| Some(item) != except)
            .find(|(place, _)| place.starts_with(path) || path.starts_with(place))
            .map(|(_, item)| item)
    }
}
