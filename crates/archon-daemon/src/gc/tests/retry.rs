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

fn write_all(dir: &Path, files: &[&str]) {
    for file in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![1u8; 4096]).unwrap();
    }
}

/// Git deletes an ignored directory whole, so a cache that holds an
/// `.archon` directory stays whole: known, configured or set aside.
#[test]
fn a_cache_holding_an_archon_directory_is_kept_whole() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (_repo, tree) = worktree_with_reports(&base);
    let nested = ["target/.archon/reports/a", "web/node_modules/.archon/b"];
    write_all(&tree, &nested);

    let listed = cache::preview_worktree(&tree, &[]).unwrap();
    assert_eq!(listed, ["pkg/__pycache__/"]);
    assert!(cache::reclaim_worktree(&tree, &[]).unwrap().is_some());
    let target = [PathBuf::from("target")];
    assert_eq!(cache::reclaim_worktree(&tree, &target).unwrap(), None);
    assert_eq!(exists(&tree, &nested), [true; 2]);
    assert_eq!(exists(&tree, &CACHES), [true, false, true]);

    let kept = base.join("kept");
    write_all(&kept, &["target/.archon/reports/a", "build/x"]);
    let dirs = [PathBuf::from("target"), PathBuf::from("build")];
    assert!(cache::reclaim_kept(&kept, &dirs).unwrap().is_some());
    assert!(kept.join("target/.archon/reports/a").exists());
    assert!(!kept.join("build").exists(), "a cache without one goes");
}

/// With `target/.gitkeep` tracked, git lists what is ignored inside
/// `target/` rather than `target/` itself: that goes, the tracked file stays.
#[test]
fn a_partly_tracked_cache_loses_its_ignored_contents() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let ignore = "/target/*\n!/target/.gitkeep\n/.archon/reports/\n";
    std::fs::write(repo.join(".gitignore"), ignore).unwrap();
    write_all(&repo, &["target/.gitkeep"]);
    git_ok(&repo, &["add", ".gitignore", "target/.gitkeep"]);
    git_ok(&repo, &["commit", "-q", "-m", "keep target"]);
    let tree = base.join("trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-partly");
    let built = ["target/debug/app", "target/.rustc_info.json"];
    write_all(&tree, &built);
    write_all(&tree, &[".archon/reports/r"]);

    let listed = cache::preview_worktree(&tree, &[]).unwrap();
    assert_eq!(listed, ["target/.rustc_info.json", "target/debug/"]);
    assert!(cache::reclaim_worktree(&tree, &[]).unwrap().is_some());
    assert_eq!(exists(&tree, &built), [false; 2]);
    assert!(tree.join("target/.gitkeep").exists(), "tracked files stay");
    assert!(tree.join(".archon/reports/r").exists());
}

/// Only the removal of a worktree, or of one set aside, is logged as a loss
/// for a failed item's retry: not a snapshot, a git directory or caches.
#[test]
fn only_a_removed_worktree_is_a_retry_s_loss() {
    use super::plan::{Action, Candidate, Kind};
    use super::sweep::{Verdict, loses_a_retry_worktree};
    let item = archon_core::id::QueueItemId::new();
    let within = PathBuf::from("/w");
    let of = |kind: Kind, action| Candidate {
        workspace: PathBuf::from("/w/qi_1"),
        repo: None,
        kind,
        action,
    };
    let removed = Verdict::Removed { bytes: 1 };
    let losses = [
        (Kind::Worktree, Action::Remove, true),
        (
            Kind::KeptWorktree {
                within: within.clone(),
            },
            Action::Remove,
            true,
        ),
        (Kind::Snapshot { item }, Action::Remove, false),
        (Kind::KeptGitDir { within }, Action::Remove, false),
    ];
    for (kind, action, lost) in losses {
        let candidate = of(kind, action);
        assert_eq!(
            loses_a_retry_worktree(&candidate, &removed),
            lost,
            "{candidate:?}"
        );
    }
    let reclaimed = Verdict::Reclaimed { bytes: 1 };
    let candidate = of(Kind::Worktree, Action::Reclaim);
    assert!(!loses_a_retry_worktree(&candidate, &reclaimed));
}

/// A cancelled owner and a refresh of it onto `sha` that failed at dispatch.
fn owner_with_failed_refresh(store: &mut Store) -> (QueueItem, QueueItem) {
    let owner = store
        .insert_item(&item_in(QueueState::Queued, 900), None)
        .unwrap();
    let fix = refresh_of(store, &owner, 'a');
    fail_at_dispatch(store, &fix);
    let cancelled = QueueState::Cancelled { reason: None };
    store
        .transition_item(&owner.id, cancelled, ago(900))
        .unwrap();
    (store.item(&owner.id).unwrap(), fix)
}

fn refresh_of(store: &mut Store, owner: &QueueItem, sha: char) -> QueueItem {
    let mut run = run_in(Path::new("/w/owner"), Some(900));
    run.item = owner.id.clone();
    run.branch = "archon/owner".into();
    let refresh = archon_queue::store::stacks::Refresh {
        base: "main".into(),
        sha: sha.to_string().repeat(40),
        from: None,
    };
    store.enqueue_refresh(owner, &run, &refresh).unwrap()
}

/// Dispatch the queued `item`, which fails before it starts a run.
fn fail_at_dispatch(store: &mut Store, item: &QueueItem) {
    let at = ago(900);
    store
        .transition_item(&item.id, QueueState::Dispatching, at)
        .unwrap();
    let failed = QueueState::Failed {
        run: None,
        error: "e".into(),
    };
    store.transition_item(&item.id, failed, at).unwrap();
}

/// A cancelled owner whose refresh failed names the refresh, not itself, as
/// the failed item a retry of which needs the worktree.
#[test]
fn a_failed_refresh_is_named_for_its_cancelled_owners_worktree() {
    let mut store = Store::open_in_memory().unwrap();
    let (owner, fix) = owner_with_failed_refresh(&mut store);

    let settled = plan::settled(&store, ago(-60)).unwrap();
    let of = |id| settled.iter().find(|s| &s.item.id == id).unwrap();
    assert_eq!(of(&owner.id).failed, [fix.id.clone()]);
    assert_eq!(of(&fix.id).failed, [fix.id.clone()]);
}

/// The owner's plan as a pass would make it now, for the recheck.
fn owner_plan(store: &Store, owner: &QueueItem) -> plan::ItemPlan {
    let settled = plan::settled(store, ago(-60)).unwrap();
    let found = settled.iter().find(|s| s.item.id == owner.id).unwrap();
    assert_eq!(found.item.updated_at, owner.updated_at);
    plan::ItemPlan {
        item: owner.id.clone(),
        updated_at: found.item.updated_at,
        runs: store.runs_for_item(&owner.id).unwrap().len(),
        borrowed: Default::default(),
        found: plan::Found {
            failed: found.failed.clone(),
            ..plan::Found::default()
        },
    }
}

/// A second sharer that fails at dispatch after planning, or a failed one
/// cancelled since, adds no run but unsettles the plan: it named others.
#[test]
fn a_change_in_which_sharers_failed_unsettles_the_plan() {
    let mut store = Store::open_in_memory().unwrap();
    let (owner, first) = owner_with_failed_refresh(&mut store);
    let second = refresh_of(&mut store, &owner, 'b');
    let cancelled = QueueState::Cancelled { reason: None };
    store
        .transition_item(&second.id, cancelled.clone(), ago(900))
        .unwrap();

    let planned = owner_plan(&store, &owner);
    assert_eq!(planned.found.failed, [first.id.clone()]);
    assert!(plan::still_settled(&store, &planned).unwrap());
    store
        .transition_item(&second.id, QueueState::Queued, ago(900))
        .unwrap();
    fail_at_dispatch(&mut store, &second);
    assert!(
        !plan::still_settled(&store, &planned).unwrap(),
        "another sharer failed"
    );

    let planned = owner_plan(&store, &owner);
    assert_eq!(planned.found.failed.len(), 2);
    for next in [QueueState::Queued, cancelled] {
        store.transition_item(&first.id, next, ago(900)).unwrap();
    }
    assert!(
        !plan::still_settled(&store, &planned).unwrap(),
        "a planned failed sharer was cancelled"
    );
}

#[derive(Clone)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Plan and sweep `store` once, with the worktree past `gc_after`, and
/// return what was logged at info and above.
fn logged_pass(store: &Store, base: &Path) -> (usize, String) {
    let policy = Policy {
        remove_before: ago(3600),
        reclaim_before: ago(3600),
        ..policy(false)
    };
    let settled = plan::settled(store, policy.settled_before()).unwrap();
    let plans = plan::plan_all(&settled, &roots_in(base), &policy);
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Sink(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || sink.clone())
        .finish();
    let removed = tracing::subscriber::with_default(subscriber, || {
        let mut removed = 0;
        for item in plans {
            assert!(plan::still_settled(store, &item).unwrap());
            let report = sweep::sweep(item.found, &roots_in(base), &policy).report;
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            removed += report.removed.len();
        }
        removed
    });
    let written = bytes.lock().unwrap().clone();
    (removed, String::from_utf8(written).unwrap())
}

/// The worktree of an item that ran once, failing or succeeding, in a repo.
fn finished_in_worktree(succeed: bool) -> (tempfile::TempDir, Store, QueueItem, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (repo, tree) = worktree_with_reports(&base);
    let mut store = Store::open_in_memory().unwrap();
    let registered = repo_at(&repo);
    store.insert_repo(&registered).unwrap();
    let mut queued = item_in(QueueState::Queued, 9000);
    queued.repo = Some(registered.id.clone());
    let item = store.insert_item(&queued, None).unwrap();
    run_once(&mut store, &item, &tree, succeed);
    (dir, store, item, tree)
}

const RETRY_LOSS: &str = "gc removed a worktree a failed item may retry in";

/// Removing a failed item's worktree at `gc_after` logs the item and the
/// path at info, so its retry's loss is traceable; any other removal not.
#[test]
fn removing_a_failed_items_worktree_is_logged_with_the_item_and_path() {
    let (dir, store, item, tree) = finished_in_worktree(false);
    let (removed, logged) = logged_pass(&store, &dir.path().canonicalize().unwrap());
    assert_eq!(removed, 1, "{logged}");
    let line = logged.lines().find(|l| l.contains(RETRY_LOSS));
    let line = line.unwrap_or_else(|| panic!("no retry loss in {logged:?}"));
    assert!(line.contains(" INFO "), "{line}");
    assert!(line.contains(item.id.as_str()), "{line}");
    assert!(line.contains(&tree.display().to_string()), "{line}");

    let (dir, store, _, _) = finished_in_worktree(true);
    let (removed, logged) = logged_pass(&store, &dir.path().canonicalize().unwrap());
    assert_eq!(removed, 1, "{logged}");
    assert!(!logged.contains(RETRY_LOSS), "a succeeded item: {logged}");
}
