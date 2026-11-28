//! A read-only run's snapshot left behind: planned for removal as soon
//! as anything of it is due, and removed dirty or locked without `--force`,
//! but only when it is really that item's snapshot.

use std::path::{Path, PathBuf};

use archon_core::catalog::SNAPSHOT_DIR;
use archon_core::id::QueueItemId;
use archon_core::queue::QueueState;

use super::{
    Action, Candidate, Kind, Policy, ago, git_ok, init_repo, item_in, plan, policy, roots_in,
    run_in, settled, sweep_of,
};
use crate::worktree::git;

/// A snapshot of `repo`'s HEAD at `<item_dir>/snapshot`, as a failed ask
/// run left it: detached, dirty and locked by its agent.
fn leftover_snapshot(repo: &Path, item_dir: &Path) -> PathBuf {
    let snapshot = item_dir.join(SNAPSHOT_DIR);
    let path = snapshot.to_str().unwrap();
    git_ok(repo, &["worktree", "add", "-q", "--detach", path, "HEAD"]);
    std::fs::write(snapshot.join("build.log"), "written by a build\n").unwrap();
    std::fs::write(snapshot.join("scratch.txt"), "untracked\n").unwrap();
    git_ok(repo, &["worktree", "lock", path]);
    snapshot
}

fn snapshot_candidate(workspace: &Path, repo: &Path, item: &QueueItemId) -> Candidate {
    Candidate {
        workspace: workspace.to_path_buf(),
        repo: Some(repo.to_path_buf()),
        kind: Kind::Snapshot { item: item.clone() },
        action: Action::Remove,
    }
}

fn listed(repo: &Path) -> String {
    git(repo, &["worktree", "list", "--porcelain"]).unwrap()
}

/// Runs finished two minutes ago: caches due, removal not yet. The failed
/// item's worktree keeps its caches for a retry; the snapshot goes whole.
#[test]
fn a_snapshot_is_planned_for_removal_once_its_caches_are_due() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let failed = QueueState::Failed {
        run: None,
        error: "e".into(),
    };
    let item = item_in(failed, 120);
    let item_dir = base.join("trees/repo").join(item.id.as_str());
    let snapshot = item_dir.join(SNAPSHOT_DIR);
    std::fs::create_dir_all(&snapshot).unwrap();
    let runs = vec![run_in(&snapshot, Some(120)), run_in(&item_dir, Some(120))];
    let repo = base.join("repo");
    let policy = Policy {
        remove_before: ago(600),
        reclaim_before: ago(60),
        ..policy(false)
    };
    let id = item.id.clone();
    let plans = plan::plan_all(
        &[settled(item, runs, Some(&repo))],
        &roots_in(&base),
        &policy,
    );
    assert_eq!(plans.len(), 1);
    let expected = vec![snapshot_candidate(&snapshot, &repo, &id)];
    assert_eq!(plans[0].found.candidates, expected);
    assert!(plans[0].found.keep_caches, "a failed item may be retried");
}

#[test]
fn a_dirty_locked_leftover_snapshot_goes_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let item = QueueItemId::new();
    let item_dir = base.join("trees/repo").join(item.as_str());
    let snapshot = leftover_snapshot(&repo, &item_dir);
    assert!(listed(&repo).contains("locked"), "{}", listed(&repo));

    let candidates = vec![snapshot_candidate(&snapshot, &repo, &item)];
    let report = sweep_of(candidates, &roots_in(&base), &policy(false));
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.removed, vec![snapshot.clone()]);
    assert!(report.bytes_freed > 0, "{report:?}");
    assert!(!item_dir.exists(), "its emptied item directory goes too");
    assert!(!listed(&repo).contains(SNAPSHOT_DIR), "{}", listed(&repo));
    assert!(repo.join("README.md").exists(), "the repository stays");
}

/// Neither a snapshot that is a symlink to another worktree, nor one whose
/// item directory is a symlink to another item's, is removed through it.
#[test]
fn a_snapshot_reached_through_a_symlink_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let trees = base.join("trees/repo");
    let other_item = QueueItemId::new();
    let other = leftover_snapshot(&repo, &trees.join(other_item.as_str()));

    let linked = QueueItemId::new();
    let linked_dir = trees.join(linked.as_str());
    std::fs::create_dir_all(&linked_dir).unwrap();
    std::os::unix::fs::symlink(&other, linked_dir.join(SNAPSHOT_DIR)).unwrap();
    let aliased = QueueItemId::new();
    let aliased_dir = trees.join(aliased.as_str());
    std::os::unix::fs::symlink(trees.join(other_item.as_str()), &aliased_dir).unwrap();

    let candidates = vec![
        snapshot_candidate(&linked_dir.join(SNAPSHOT_DIR), &repo, &linked),
        snapshot_candidate(&aliased_dir.join(SNAPSHOT_DIR), &repo, &aliased),
    ];
    let report = sweep_of(candidates, &roots_in(&base), &policy(true));
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.skipped.len(), 2, "{:?}", report.skipped);
    assert!(
        other.join("scratch.txt").exists(),
        "the other snapshot stays"
    );
}
