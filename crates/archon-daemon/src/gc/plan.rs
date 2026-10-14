//! What a gc pass will consider: settled items read from the store, and
//! for each, the workspaces its finished runs left, what to do with each,
//! and the repository git removes a worktree through.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use archon_core::Timestamp;
use archon_core::catalog::is_snapshot_of;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::protocol::ItemQuery;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::Store;

use super::sweep::is_inside;
use super::{Policy, Roots, team};
use crate::worktree::git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    pub workspace: PathBuf,
    /// The repository, or its common git directory, git lists the worktree
    /// in; `None` for scratch runs and set-aside directories.
    pub repo: Option<PathBuf>,
    pub kind: Kind,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    /// A run workspace: a worktree git lists, or a scratch directory.
    Worktree,
    /// A role's worktree a team run set aside, directly in the real
    /// directory `within`. No git runs in it.
    KeptWorktree { within: PathBuf },
    /// The git directory of a worktree set aside, directly in `within`.
    KeptGitDir { within: PathBuf },
    /// The snapshot a `repo-readonly` run of `item` left behind, which
    /// nothing is kept in: it goes whole, dirty or locked.
    Snapshot { item: QueueItemId },
}

/// Ordered so that, of two for the same workspace, removal wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Action {
    Remove,
    /// Delete the build caches only.
    Reclaim,
}

/// What gc will consider, and the places it could not look into; one of
/// those fails gc without stopping it from collecting the rest.
#[derive(Debug, Default)]
pub(super) struct Found {
    pub candidates: Vec<Candidate>,
    pub unreadable: Vec<(PathBuf, String)>,
    /// Team roots, removed once gc has emptied them.
    pub team_roots: Vec<PathBuf>,
}

impl Found {
    /// `result`'s value, or `None` with its error noted against `path`.
    pub fn or_note<T>(&mut self, path: &Path, result: Result<T, ArchonError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                self.unreadable.push((path.to_path_buf(), e.to_string()));
                None
            }
        }
    }

    /// One candidate per workspace, removal first, and each team root once,
    /// in path order.
    pub fn deduplicated(mut self) -> Self {
        self.candidates
            .sort_by(|a, b| (&a.workspace, a.action).cmp(&(&b.workspace, b.action)));
        self.candidates
            .dedup_by(|later, first| later.workspace == first.workspace);
        self.team_roots.sort();
        self.team_roots.dedup();
        debug_assert!(
            self.candidates
                .is_sorted_by(|a, b| a.workspace < b.workspace)
        );
        self
    }

    /// Every place it names: candidates, team roots and unreadable places.
    pub fn paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        let candidates = self.candidates.iter().map(|c| &c.workspace);
        let unreadable = self.unreadable.iter().map(|(path, _)| path);
        candidates
            .chain(&self.team_roots)
            .chain(unreadable)
            .cloned()
    }

    fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.unreadable.is_empty() && self.team_roots.is_empty()
    }
}

/// A settled item with its runs and registered repository, as the store
/// had them when the pass began.
#[derive(Debug, Clone)]
pub(super) struct Settled {
    pub item: QueueItem,
    pub runs: Vec<PipelineRun>,
    pub repo: Option<Repo>,
}

/// One item's share of a pass, collected only while the item is still as
/// settled as it was planned.
#[derive(Debug)]
pub(super) struct ItemPlan {
    pub item: QueueItemId,
    pub updated_at: Timestamp,
    /// Runs are never deleted, so a retry since shows as another run even
    /// when the clock stepped back to the planned `updated_at`.
    pub runs: usize,
    pub found: Found,
}

/// Terminal items last changed before `before`, with their runs. A later
/// change (a retry, a late run) may mean a recent run, so such an item
/// waits for a later pass.
pub(super) fn settled(store: &Store, before: Timestamp) -> Result<Vec<Settled>, ArchonError> {
    let query = ItemQuery {
        states: vec!["succeeded".into(), "failed".into(), "cancelled".into()],
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    let repos: HashMap<RepoId, Repo> = store
        .repos()?
        .into_iter()
        .map(|repo| (repo.id.clone(), repo))
        .collect();
    let mut settled = Vec::new();
    for item in store.items(&query)? {
        if !item.state.is_terminal() || item.updated_at >= before {
            continue;
        }
        let runs = store.runs_for_item(&item.id)?;
        let repo = item.repo.as_ref().and_then(|id| repos.get(id)).cloned();
        settled.push(Settled { item, runs, repo });
    }
    debug_assert!(settled.iter().all(|s| s.item.state.is_terminal()));
    Ok(settled)
}

/// Whether the item is still settled as it was when `plan` was made; an
/// item retried since may be using its worktree again.
pub(super) fn still_settled(store: &Store, plan: &ItemPlan) -> Result<bool, ArchonError> {
    if store.has_pending_stack_fix(&plan.item)? {
        return Ok(false);
    }
    let item = match store.item(&plan.item) {
        Ok(item) => item,
        Err(ArchonError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(error),
    };
    assert_eq!(item.id, plan.item, "the store returns the item asked for");
    if !item.state.is_terminal() || item.updated_at != plan.updated_at {
        return Ok(false);
    }
    let runs = store.runs_for_item(&plan.item)?.len();
    assert!(runs >= plan.runs, "runs are never deleted");
    Ok(runs == plan.runs)
}

/// The plans of the items that hold something to collect.
pub(super) fn plan_all(settled: &[Settled], roots: &Roots, policy: &Policy) -> Vec<ItemPlan> {
    let plans: Vec<ItemPlan> = settled
        .iter()
        .map(|s| plan_item(s, roots, policy))
        .filter(|plan| !plan.found.is_empty())
        .collect();
    debug_assert!(plans.len() <= settled.len());
    plans
}

/// The workspaces of one item whose runs all finished long enough ago,
/// still on disk, and not the repository itself (`repo-readonly` stages).
/// A team run's workspace brings everything under its team root, on the
/// normal cutoffs: only the owed run workspace itself goes early.
fn plan_item(settled: &Settled, roots: &Roots, policy: &Policy) -> ItemPlan {
    let registered = settled.repo.as_ref().map(|r| r.path.as_path());
    let owed = owed_workspace(settled, policy);
    let item = &settled.item.id;
    let mut found = Found::default();
    for (workspace, finished) in last_use(&settled.runs) {
        if registered == Some(workspace.as_path()) {
            continue;
        }
        let action = action_for(finished, policy);
        let early = owed == Some(workspace.as_path())
            && finished.is_some_and(|at| at < policy.reclaim_before)
            && workspace.exists();
        assert!(
            !early || action.is_some(),
            "an owed workspace's caches are due too"
        );
        // What becomes of the workspace itself; a team root's rest follows `action`.
        let Some(own) = (if early { Some(Action::Remove) } else { action }) else {
            continue;
        };
        let root = settled
            .runs
            .iter()
            .find(|run| run.workspace == workspace)
            .and_then(|run| run.team_root.as_deref());
        match root {
            Some(root) if root.exists() && registered != Some(root) => {
                let repo = repo_of(settled, &team::worktrees(root), roots, &mut found);
                if let Some(action) = action {
                    team::candidates(root, repo.as_deref(), action, &mut found);
                }
                if early {
                    let candidate = run_workspace(workspace, repo, own, item);
                    found.candidates.push(candidate);
                }
            }
            Some(_) => {}
            None => flat_candidates(settled, workspace, own, action, roots, &mut found),
        }
    }
    ItemPlan {
        item: settled.item.id.clone(),
        updated_at: settled.item.updated_at,
        runs: settled.runs.len(),
        found: found.deduplicated(),
    }
}

/// A flat run's workspace, while it exists, with `own`, and what fresh
/// retries of its item set aside of it with `action`, the normal cutoff;
/// those outlive the workspace when a shipped run removed it.
fn flat_candidates(
    settled: &Settled,
    workspace: PathBuf,
    own: Action,
    action: Option<Action>,
    roots: &Roots,
    found: &mut Found,
) {
    let repo = repo_of(settled, std::slice::from_ref(&workspace), roots, found);
    if let (Some(action), Some(repo)) = (action, repo.as_deref()) {
        team::flat_set_asides(&workspace, repo, action, found);
    }
    if workspace.exists() {
        found
            .candidates
            .push(run_workspace(workspace, repo, own, &settled.item.id));
    }
}

/// A run workspace; `item`'s snapshot goes whole as soon as its caches or
/// the workspace itself would, since nothing in it is kept.
fn run_workspace(
    workspace: PathBuf,
    repo: Option<PathBuf>,
    action: Action,
    item: &QueueItemId,
) -> Candidate {
    let (kind, action) = match is_snapshot_of(&workspace, item) {
        true => (Kind::Snapshot { item: item.clone() }, Action::Remove),
        false => (Kind::Worktree, action),
    };
    Candidate {
        workspace,
        repo,
        kind,
        action,
    }
}

/// Removal once the runs finished before `remove_before`; else, once they
/// finished before `reclaim_before`, only the caches go.
fn action_for(finished: Option<Timestamp>, policy: &Policy) -> Option<Action> {
    let at = finished?;
    if at < policy.remove_before {
        Some(Action::Remove)
    } else if at < policy.reclaim_before {
        Some(Action::Reclaim)
    } else {
        None
    }
}

/// The workspace of a succeeded run that archond removes right after it
/// ships (`executor::finish`); still here, its cleanup was lost.
fn owed_workspace<'a>(settled: &'a Settled, policy: &Policy) -> Option<&'a Path> {
    let QueueState::Succeeded { run, .. } = &settled.item.state else {
        return None;
    };
    let ship = settled.repo.as_ref()?.ship;
    if !policy.owed_cleanup || ship != ShipMode::Pr {
        return None;
    }
    let run = settled.runs.iter().find(|r| &r.id == run)?;
    run.shipped.then_some(run.workspace.as_path())
}

/// The registered repository; for an item whose repository was
/// unregistered since, the common git directory a worktree among `dirs`
/// names in its `.git` file. `None` for scratch runs.
fn repo_of(
    settled: &Settled,
    dirs: &[PathBuf],
    roots: &Roots,
    found: &mut Found,
) -> Option<PathBuf> {
    if let Some(repo) = &settled.repo {
        return Some(repo.path.clone());
    }
    settled.item.repo.as_ref()?;
    let inside = |dir: &&PathBuf| {
        dir.join(".git").is_file()
            && dir
                .canonicalize()
                .is_ok_and(|real| is_inside(&real, &roots.worktrees))
    };
    let dir = dirs.iter().find(inside)?;
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = found.or_note(dir, git(dir, &args))?;
    let common = PathBuf::from(common);
    assert!(common.is_absolute(), "git was asked for an absolute path");
    Some(common)
}

/// When each workspace was last used: the latest finish of the runs in it,
/// or `None` while any of them has not finished.
pub(super) fn last_use(runs: &[PipelineRun]) -> BTreeMap<PathBuf, Option<Timestamp>> {
    let mut last: BTreeMap<PathBuf, Option<Timestamp>> = BTreeMap::new();
    for run in runs {
        let finished = run.finished_at;
        last.entry(run.workspace.clone())
            .and_modify(|at| *at = at.zip(finished).map(|(a, b)| a.max(b)))
            .or_insert(finished);
    }
    debug_assert!(last.len() <= runs.len());
    last
}
