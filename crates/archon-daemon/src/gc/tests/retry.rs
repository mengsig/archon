//! A failed item can be retried in its worktree, so gc reclaims nothing of
//! it; once it succeeded only build caches go, never `.archon/reports/` or
//! other ignored data (the 2026-11-27 wipe of a failed item's reports).

use std::path::{Path, PathBuf};

use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::RunState;
use archon_queue::Store;

use super::{
    Policy, add_worktree, ago, cache, git_ok, init_repo, item_in, plan, policy, repo_at, roots_in,
    run_in, sweep,
};

const IGNORED: &str = "/.archon/reports/\n__pycache__/\n/bench/results/\n";

/// Ignored measurement output a retry needs, beside real build caches.
const KEPT: [&str; 5] = [
    ".archon/reports/run/data.json",
    ".archon/reports/run/bin/tool",
    ".archon/reports/run/py/__pycache__/m.pyc",
    "bench/results/r1/__pycache__/m.pyc",
    ".env",
];
const CACHES: [&str; 3] = [
    "target/debug/app",
    "pkg/__pycache__/a.pyc",
    "web/node_modules/i.js",
];

/// A worktree of a repository that also ignores reports and bench results,
/// holding each of [`KEPT`] and [`CACHES`].
fn worktree_with_reports(base: &Path) -> (PathBuf, PathBuf) {
    let repo = base.join("repo");
    init_repo(&repo);
    let gitignore = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
    std::fs::write(repo.join(".gitignore"), format!("{gitignore}{IGNORED}")).unwrap();
    git_ok(&repo, &["commit", "-q", "-am", "ignore reports"]);
    let tree = base.join("trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-retry");
    for file in KEPT.iter().chain(&CACHES) {
        let path = tree.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![1u8; 4096]).unwrap();
    }
    (repo, tree)
}

/// Run `item` once more in `tree`, failing or succeeding two hours ago:
/// past `cache_after`, well before `gc_after`.
fn run_once(store: &mut Store, item: &QueueItem, tree: &Path, succeed: bool) {
    let at = ago(7200);
    store
        .transition_item(&item.id, QueueState::Dispatching, at)
        .unwrap();
    let mut run = run_in(tree, None);
    run.item = item.id.clone();
    run.state = RunState::Running {
        stage_id: "implement".into(),
        attempt: 1,
    };
    store.insert_run(&run).unwrap();
    let running = QueueState::Running {
        run: run.id.clone(),
    };
    store.transition_item(&item.id, running, at).unwrap();
    let error = "login failed".to_owned();
    let (run_end, end) = match succeed {
        true => (
            RunState::Succeeded {
                summary: "shipped".into(),
            },
            QueueState::Succeeded {
                run: run.id.clone(),
                summary: "shipped".into(),
            },
        ),
        false => (
            RunState::Failed {
                stage_id: "implement".into(),
                error: error.clone(),
            },
            QueueState::Failed {
                run: Some(run.id.clone()),
                error,
            },
        ),
    };
    run.finish(run_end, at).unwrap();
    store.finish_run(&run, end, at).unwrap();
}

/// One gc pass over `store`, as archond's own: plan, then sweep each plan.
fn pass(store: &Store, base: &Path, policy: &Policy) -> usize {
    let settled = plan::settled(store, policy.settled_before()).unwrap();
    let plans = plan::plan_all(&settled, &roots_in(base), policy);
    let mut reclaimed = 0;
    for item in plans {
        assert!(plan::still_settled(store, &item).unwrap());
        let report = sweep::sweep(item.found, &roots_in(base), policy).report;
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.removed.is_empty(), "not old enough: {report:?}");
        reclaimed += report.reclaimed.len();
    }
    reclaimed
}

fn exists(tree: &Path, files: &[&str]) -> Vec<bool> {
    files.iter().map(|file| tree.join(file).exists()).collect()
}

#[test]
fn a_failed_items_retry_finds_its_reports_and_success_loses_only_caches() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (repo, tree) = worktree_with_reports(&base);
    let mut store = Store::open_in_memory().unwrap();
    let registered = repo_at(&repo);
    store.insert_repo(&registered).unwrap();
    let mut queued = item_in(QueueState::Queued, 9000);
    queued.repo = Some(registered.id.clone());
    let item = store.insert_item(&queued, None).unwrap();
    let policy = Policy {
        remove_before: ago(7 * 86_400),
        reclaim_before: ago(3600),
        ..policy(false)
    };

    run_once(&mut store, &item, &tree, false);
    assert_eq!(
        pass(&store, &base, &policy),
        0,
        "a failed item keeps its caches"
    );
    assert_eq!(exists(&tree, &KEPT), [true; 5]);
    assert_eq!(exists(&tree, &CACHES), [true; 3]);

    store
        .transition_item(&item.id, QueueState::Queued, ago(7200))
        .unwrap();
    assert_eq!(
        exists(&tree, &KEPT),
        [true; 5],
        "the retry finds its reports"
    );
    run_once(&mut store, &item, &tree, true);
    assert_eq!(
        pass(&store, &base, &policy),
        1,
        "a succeeded item's caches go"
    );
    assert_eq!(exists(&tree, &CACHES), [false; 3]);
    assert_eq!(exists(&tree, &KEPT), [true; 5], "only build caches went");
}

/// The default reclaim never used a `:(glob)**/name` pathspec's matching:
/// git then lists every wholly ignored directory, reports included.
#[test]
fn the_default_reclaim_lists_only_build_caches() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (_repo, tree) = worktree_with_reports(&base);

    let listed = cache::preview_worktree(&tree, &[]).unwrap();
    assert_eq!(listed, ["pkg/__pycache__/", "target/", "web/node_modules/"]);
    let freed = cache::reclaim_worktree(&tree, &[]).unwrap();
    assert!(freed.is_some_and(|bytes| bytes >= 3 * 4096), "{freed:?}");
    assert_eq!(exists(&tree, &CACHES), [false; 3]);
    assert_eq!(exists(&tree, &KEPT), [true; 5]);
}
