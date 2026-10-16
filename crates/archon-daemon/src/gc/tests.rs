use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::Timestamp;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::PipelineRun;
use archon_queue::Store;

use super::plan::{self, Action, Candidate, Found, ItemPlan, Kind, Settled};
use super::sweep::{self, Verdict, is_inside};
use super::{Policy, Roots, cache, canonical_root, team};
use crate::worktree::git;

mod snapshot;

fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository at `repo` whose one commit ignores `target/` and `*.log`.
fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git_ok(repo, &["init", "-q"]);
    std::fs::write(repo.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(repo.join("README.md"), "demo\n").unwrap();
    git_ok(repo, &["add", "."]);
    git_ok(repo, &["commit", "-q", "-m", "init"]);
}

fn add_worktree(repo: &Path, tree: &Path, branch: &str) {
    let tree = tree.to_str().unwrap();
    git_ok(repo, &["worktree", "add", "-q", "-b", branch, tree]);
}

fn ago(secs: i64) -> Timestamp {
    archon_core::now() - chrono::Duration::seconds(secs)
}

/// Removal and reclaiming both due for anything that finished already.
fn policy(force: bool) -> Policy {
    let now = archon_core::now();
    Policy {
        remove_before: now,
        reclaim_before: now,
        force,
        cache_dirs: Vec::new(),
        owed_cleanup: false,
    }
}

fn roots_in(base: &Path) -> Roots {
    Roots {
        worktrees: base.join("trees"),
        scratch: base.join("scratch"),
        data_dir: base.join("data"),
    }
}

fn candidate(workspace: &Path, repo: Option<&Path>, action: Action) -> Candidate {
    Candidate {
        workspace: workspace.to_path_buf(),
        repo: repo.map(Path::to_path_buf),
        kind: Kind::Worktree,
        action,
    }
}

/// The report of collecting `candidates` alone.
fn sweep_of(
    candidates: Vec<Candidate>,
    roots: &Roots,
    policy: &Policy,
) -> archon_core::protocol::GcReport {
    let found = Found {
        candidates,
        ..Found::default()
    };
    sweep::sweep(found, roots, policy)
}

/// Build output under `tree`: an ignored `target/` and log, `bytes` big.
fn build_in(tree: &Path, bytes: usize) {
    std::fs::create_dir_all(tree.join("target/debug")).unwrap();
    std::fs::write(tree.join("target/debug/app"), vec![1u8; bytes]).unwrap();
    std::fs::write(tree.join("build.log"), "built\n").unwrap();
}

#[test]
fn only_listed_worktrees_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let tree = base.join("trees").join("repo").join("qi_1");
    add_worktree(&repo, &tree, "gc-test");
    let stray = base.join("stray");
    std::fs::create_dir(&stray).unwrap();
    let outside = base.join("elsewhere");
    add_worktree(&repo, &outside, "operator-own");

    let candidates = [&tree, &stray, &outside]
        .map(|w| candidate(w, Some(&repo), Action::Remove))
        .into();
    let report = sweep_of(candidates, &roots_in(&base), &policy(false));
    assert_eq!(report.removed, vec![tree.clone()]);
    assert!(!tree.exists());
    assert_eq!(report.skipped.len(), 2);
    assert!(
        stray.exists(),
        "a directory git does not list is left alone"
    );
    assert!(
        outside.exists(),
        "a worktree outside Archon's root is left alone"
    );
}

#[test]
fn a_worktree_reached_through_a_symlinked_root_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    std::fs::create_dir(base.join("real")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
    // As archond stores it: under the root as configured, symlink and all.
    let tree = base.join("link/trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-link");

    let roots = Roots {
        worktrees: canonical_root(&base.join("link/trees")).unwrap(),
        scratch: canonical_root(&base.join("link/scratch")).unwrap(),
        data_dir: base.join("data"),
    };
    assert_eq!(roots.worktrees, base.join("real/trees"));
    let found = vec![candidate(&tree, Some(&repo), Action::Remove)];
    let report = sweep_of(found, &roots, &policy(false));
    assert_eq!(report.removed, vec![base.join("real/trees/repo/qi_1")]);
    assert!(report.skipped.is_empty() && report.failed.is_empty());
    assert!(!tree.exists());
}

#[test]
fn a_dirty_worktree_loses_only_its_caches_unless_forced() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let tree = base.join("trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-dirty");
    std::fs::write(tree.join("notes.txt"), "work in progress").unwrap();
    build_in(&tree, 64 << 10);
    let roots = roots_in(&base);
    let found = || vec![candidate(&tree, Some(&repo), Action::Remove)];

    let kept = sweep_of(found(), &roots, &policy(false));
    assert!(kept.failed.is_empty(), "a kept worktree is no failure");
    assert_eq!(kept.skipped.len(), 1);
    assert_eq!(kept.skipped[0].0, tree);
    assert!(kept.skipped[0].1.contains("--force"), "{:?}", kept.skipped);
    assert_eq!(kept.reclaimed.len(), 1, "its caches still went");
    assert!(tree.join("notes.txt").exists() && !tree.join("target").exists());

    let forced = sweep_of(found(), &roots, &policy(true));
    assert_eq!(forced.removed, vec![tree.clone()]);
    assert!(!tree.exists());
}

#[test]
fn reclaiming_deletes_ignored_build_output_and_keeps_source() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let tree = base.join("trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-cache");
    build_in(&tree, 256 << 10);
    std::fs::create_dir(tree.join("src")).unwrap();
    std::fs::write(tree.join("src/new.rs"), "fn new() {}\n").unwrap();
    std::fs::write(tree.join("README.md"), "changed\n").unwrap();

    let only_target = cache::reclaim_worktree(&tree, &[PathBuf::from("target")]).unwrap();
    assert!(
        only_target.is_some_and(|b| b >= 256 << 10),
        "{only_target:?}"
    );
    assert!(!tree.join("target").exists());
    assert!(
        tree.join("build.log").exists(),
        "outside the configured dirs"
    );

    let report = sweep_of(
        vec![candidate(&tree, Some(&repo), Action::Reclaim)],
        &roots_in(&base),
        &policy(false),
    );
    assert_eq!(report.reclaimed.len(), 1, "{report:?}");
    assert!(report.removed.is_empty() && report.skipped.is_empty());
    assert!(!tree.join("build.log").exists());
    assert!(tree.join("src/new.rs").exists(), "untracked source stays");
    let readme = std::fs::read_to_string(tree.join("README.md")).unwrap();
    assert_eq!(readme, "changed\n", "changes stay");
    assert_eq!(cache::reclaim_worktree(&tree, &[]).unwrap(), None);
}

#[test]
fn reclaiming_refuses_a_directory_inside_a_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    build_in(&repo, 1);
    std::fs::create_dir(repo.join("sub")).unwrap();
    let refused = cache::reclaim_worktree(&repo.join("sub"), &[]);
    assert!(refused.is_err(), "git would clean the enclosing repository");
    assert!(repo.join("target").exists() && repo.join("build.log").exists());
}

#[test]
fn a_set_aside_worktree_loses_only_its_real_cache_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let kept = base.join("kept");
    build_in(&kept, 4096);
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, kept.join("out")).unwrap();

    assert_eq!(cache::reclaim_kept(&kept, &[]).unwrap(), None);
    let dirs = [PathBuf::from("target"), PathBuf::from("out")];
    let freed = cache::reclaim_kept(&kept, &dirs).unwrap();
    assert!(freed.is_some_and(|b| b >= 4096), "{freed:?}");
    assert!(!kept.join("target").exists());
    assert!(
        kept.join("build.log").exists(),
        "no git tells what it ignores"
    );
    assert!(elsewhere.exists(), "a symlinked cache dir is not followed");
}

/// A finished team run of `qi_1`: its run worktree, a role's clean
/// worktree, a dirty one, a worktree set aside and its git directory,
/// beside the set-aside git directory of a live item `qi_2`.
#[test]
fn a_team_run_is_collected_whole_but_for_dirty_worktrees() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let root = base.join("trees/repo/qi_1");
    add_worktree(&repo, &root.join("main"), "run");
    add_worktree(&repo, &root.join("build-api"), "run--api-12345678-a1");
    add_worktree(&repo, &root.join("build-ui"), "run--ui-12345678-a1");
    std::fs::write(root.join("build-ui/wip.txt"), "unsaved").unwrap();
    let kept = root.join(".set-aside/build-ui-1");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join("left.txt"), "left").unwrap();
    let git_aside = repo.join(".git/archon-set-aside");
    let set_aside_for = |name: &str, tree: &Path| {
        let dir = git_aside.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gitdir"),
            format!("{}\n", tree.join(".git").display()),
        )
        .unwrap();
        dir
    };
    let ours = set_aside_for("build-ui-1", &root.join("build-ui"));
    let theirs = set_aside_for("build-ui1-1", &base.join("trees/repo/qi_2/build-ui"));

    let found = team_run(&root, Some(&repo), Action::Remove);
    assert!(found.unreadable.is_empty(), "{:?}", found.unreadable);
    let paths: Vec<&Path> = found
        .candidates
        .iter()
        .map(|c| c.workspace.as_path())
        .collect();
    assert!(
        !paths.contains(&theirs.as_path()),
        "another item's is not ours"
    );
    assert_eq!(paths.len(), 5, "{paths:?}");
    let roots = roots_in(&base);
    let report = sweep::sweep(found, &roots, &policy(false));
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    for gone in [root.join("main"), root.join("build-api"), kept, ours] {
        assert!(!gone.exists(), "{} is left", gone.display());
    }
    assert_eq!(report.removed.len(), 4, "{:?}", report.removed);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, root.join("build-ui"));
    assert!(root.join("build-ui/wip.txt").exists(), "dirty work stays");
    assert!(theirs.exists(), "a live item's git directory stays");
    let branches = git(&repo, &["branch", "--list", "run*"]).unwrap();
    assert_eq!(branches.lines().count(), 3, "branches are kept: {branches}");

    let forced = team_run(&root, Some(&repo), Action::Remove);
    let forced = sweep::sweep(forced, &roots, &policy(true));
    assert_eq!(forced.removed, vec![root.join("build-ui"), root.clone()]);
    assert!(!root.exists(), "the emptied item directory goes too");
}

/// What a plan finds for a team run rooted at `root`.
fn team_run(root: &Path, repo: Option<&Path>, action: Action) -> Found {
    let mut found = Found::default();
    team::candidates(root, repo, action, &mut found);
    found.deduplicated()
}

/// A shipped team run already lost `main`: gc does not report it again,
/// and removes the item directory once the roles' worktrees are gone.
#[test]
fn a_shipped_team_run_leaves_no_item_directory_behind() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let root = base.join("trees/repo/qi_1");
    add_worktree(&repo, &root.join("main"), "run");
    git_ok(
        &repo,
        &["worktree", "remove", root.join("main").to_str().unwrap()],
    );
    add_worktree(&repo, &root.join("build-api"), "run--api-12345678-a1");
    std::fs::create_dir(root.join(".set-aside")).unwrap();

    let found = team_run(&root, Some(&repo), Action::Remove);
    assert_eq!(found.candidates.len(), 1, "{:?}", found.candidates);
    assert_eq!(found.team_roots, vec![root.clone()]);
    let report = sweep::sweep(found, &roots_in(&base), &policy(false));
    assert_eq!(report.removed, vec![root.join("build-api"), root.clone()]);
    assert!(
        report.skipped.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert!(!root.exists());
    let branches = git(&repo, &["branch", "--list", "run*"]).unwrap();
    assert_eq!(branches.lines().count(), 2, "branches are kept: {branches}");
}

/// `qi_1`'s team root is a symlink to live `qi_2`'s: nothing under it is
/// collected, and `qi_2` is not removed even once it looks emptied.
#[test]
fn a_symlinked_team_root_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let live = base.join("trees/repo/qi_2");
    add_worktree(&repo, &live.join("main"), "run");
    std::fs::create_dir(live.join(".set-aside")).unwrap();
    let root = base.join("trees/repo/qi_1");
    std::os::unix::fs::symlink(&live, &root).unwrap();

    let found = team_run(&root, Some(&repo), Action::Remove);
    assert!(found.candidates.is_empty(), "{:?}", found.candidates);
    assert!(found.unreadable.is_empty(), "{:?}", found.unreadable);
    assert!(team::worktrees(&root).is_empty());
    let roots = roots_in(&base);
    let report = sweep::sweep(found, &roots, &policy(true));
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert_eq!(report.skipped[0].0, root);
    assert!(live.join("main").exists() && live.join(".set-aside").exists());

    git_ok(
        &repo,
        &["worktree", "remove", live.join("main").to_str().unwrap()],
    );
    let verdict = team::remove_if_emptied(&root, &roots.worktrees);
    assert!(
        matches!(verdict, Some((ref path, Verdict::Skipped(_))) if *path == root),
        "{verdict:?}"
    );
    assert!(live.join(".set-aside").exists(), "the live item's stays");
}

/// A team root recorded under a symlinked worktree root is reported
/// removed by its real path, like the worktrees removed from it.
#[test]
fn a_removed_team_root_is_reported_by_its_real_path() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    std::fs::create_dir(base.join("real")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
    let root = base.join("link/trees/repo/qi_1");
    add_worktree(&repo, &root.join("build-api"), "run--api-12345678-a1");
    std::fs::create_dir(root.join(".set-aside")).unwrap();

    let roots = Roots {
        worktrees: canonical_root(&base.join("link/trees")).unwrap(),
        scratch: canonical_root(&base.join("link/scratch")).unwrap(),
        data_dir: base.join("data"),
    };
    let found = team_run(&root, Some(&repo), Action::Remove);
    let report = sweep::sweep(found, &roots, &policy(false));
    let real = base.join("real/trees/repo/qi_1");
    assert_eq!(report.removed, vec![real.join("build-api"), real.clone()]);
    assert!(report.skipped.is_empty() && report.failed.is_empty());
    assert!(!real.exists());
}

#[test]
fn a_set_aside_directory_reached_through_a_symlink_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let elsewhere = base.join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("precious")).unwrap();
    let root = base.join("trees/repo/qi_1");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(".set-aside")).unwrap();
    let found = team_run(&root, None, Action::Remove);
    assert!(found.candidates.is_empty(), "{:?}", found.candidates);
    assert!(found.unreadable.is_empty(), "{:?}", found.unreadable);
    let within = root.join(".set-aside");
    let candidates = [Action::Remove, Action::Reclaim].map(|action| Candidate {
        workspace: elsewhere.join("precious"),
        repo: None,
        kind: Kind::KeptWorktree {
            within: within.clone(),
        },
        action,
    });
    let roots = roots_in(&base);
    for candidate in candidates {
        let report = sweep_of(vec![candidate], &roots, &policy(true));
        assert!(
            report.removed.is_empty() && report.failed.is_empty(),
            "{report:?}"
        );
    }
    assert!(elsewhere.join("precious").exists());
    assert_eq!(team::remove_if_emptied(&root, &roots.worktrees), None);
    assert!(within.exists(), "the symlink stays");
}

/// A registered repository that is no longer a git repository, and a
/// set-aside git directory whose `gitdir` cannot be read: each is noted
/// as unreadable, everything else of the team run is still found and
/// removed, and gc fails with the unreadable path reported.
#[test]
fn what_cannot_be_read_is_noted_and_the_rest_still_collected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("trees/repo/qi_1");
    for sub in ["main", "build-api", ".set-aside/build-api-1"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    let plain = base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    // Broken on its own, so git fails even when the temp dir is in a repo.
    std::fs::write(plain.join(".git"), "not a gitfile\n").unwrap();
    let found = team_run(&root, Some(&plain), Action::Remove);
    assert_eq!(found.unreadable.len(), 1, "{:?}", found.unreadable);
    assert_eq!(found.unreadable[0].0, plain);
    assert_eq!(found.candidates.len(), 3, "{:?}", found.candidates);

    let repo = base.join("repo");
    init_repo(&repo);
    let broken = repo.join(".git/archon-set-aside/build-api-1");
    std::fs::create_dir_all(broken.join("gitdir")).unwrap();
    let found = team_run(&root, Some(&repo), Action::Remove);
    assert_eq!(found.unreadable.len(), 1, "{:?}", found.unreadable);
    assert_eq!(found.unreadable[0].0, broken);
    assert_eq!(found.candidates.len(), 3, "{:?}", found.candidates);

    let report = sweep::sweep(found, &roots_in(&base), &policy(false));
    assert_eq!(report.removed, vec![root.join(".set-aside/build-api-1")]);
    let failed: Vec<&Path> = report.failed.iter().map(|(p, _)| p.as_path()).collect();
    assert_eq!(failed, vec![broken.as_path()]);
    assert_eq!(
        report.skipped.len(),
        2,
        "not worktrees: {:?}",
        report.skipped
    );
}

fn item_in(state: QueueState, updated_secs_ago: i64) -> QueueItem {
    let at = ago(updated_secs_ago);
    QueueItem {
        onto: None,
        id: QueueItemId::new(),
        repo: None,
        pipeline: "direct".into(),
        title: "t".into(),
        brief: String::new(),
        priority: archon_core::queue::Priority::Normal,
        dependencies: vec![],
        tags: vec![],
        model_plan: archon_core::model::ModelPlan::default(),
        model_reasons: None,
        budget_usd: None,
        autonomy: archon_core::autonomy::Autonomy::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        state,
        created_at: at,
        updated_at: at,
    }
}

fn run_in(workspace: &Path, finished_secs_ago: Option<i64>) -> PipelineRun {
    PipelineRun {
        id: archon_core::id::PipelineRunId::new(),
        item: QueueItemId::new(),
        pipeline: "direct".into(),
        shipped: false,
        aether_run: None,
        workspace: workspace.into(),
        team_root: None,
        branch: String::new(),
        state: archon_core::run::RunState::Cancelled { reason: None },
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: finished_secs_ago.map(ago),
    }
}

/// Cancelled items in an in-memory store: one settled long ago, one just
/// now, and a queued one.
#[test]
fn only_items_settled_before_the_cutoff_are_planned_and_a_retry_unsettles() {
    let mut store = Store::open_in_memory().unwrap();
    let mut settle = |secs_ago| {
        let item = store
            .insert_item(&item_in(QueueState::Queued, 900), None)
            .unwrap();
        let cancelled = QueueState::Cancelled { reason: None };
        store
            .transition_item(&item.id, cancelled, ago(secs_ago))
            .unwrap()
    };
    let (old, recent) = (settle(900), settle(0));
    store
        .insert_item(&item_in(QueueState::Queued, 900), None)
        .unwrap();

    let settled = plan::settled(&store, ago(60)).unwrap();
    let ids: Vec<&QueueItemId> = settled.iter().map(|s| &s.item.id).collect();
    assert_eq!(ids, vec![&old.id]);
    assert!(!ids.contains(&&recent.id));

    let item_plan = ItemPlan {
        item: old.id.clone(),
        updated_at: old.updated_at,
        runs: 0,
        found: Found::default(),
    };
    assert!(plan::still_settled(&store, &item_plan).unwrap());
    store
        .transition_item(&old.id, QueueState::Queued, archon_core::now())
        .unwrap();
    assert!(
        !plan::still_settled(&store, &item_plan).unwrap(),
        "a retry unsettles it"
    );
    let gone = ItemPlan {
        item: QueueItemId::new(),
        ..item_plan
    };
    assert!(!plan::still_settled(&store, &gone).unwrap());
}

/// A retry that fails again after the clock stepped back: the store keeps
/// `updated_at` from going backwards, so only the new run shows the change.
#[test]
fn a_retry_that_failed_again_across_a_clock_step_unsettles() {
    let mut store = Store::open_in_memory().unwrap();
    let item = store
        .insert_item(&item_in(QueueState::Queued, 900), None)
        .unwrap();
    let cancelled = QueueState::Cancelled { reason: None };
    let item = store
        .transition_item(&item.id, cancelled, ago(600))
        .unwrap();
    let item_plan = ItemPlan {
        item: item.id.clone(),
        updated_at: item.updated_at,
        runs: 0,
        found: Found::default(),
    };
    assert!(plan::still_settled(&store, &item_plan).unwrap());

    let stepped_back = ago(3600);
    for next in [QueueState::Queued, QueueState::Dispatching] {
        store.transition_item(&item.id, next, stepped_back).unwrap();
    }
    let mut run = run_in(Path::new("/tmp/retried"), None);
    run.item = item.id.clone();
    run.state = archon_core::run::RunState::Running {
        stage_id: "implement".into(),
        attempt: 1,
    };
    store.insert_run(&run).unwrap();
    let running = QueueState::Running {
        run: run.id.clone(),
    };
    store
        .transition_item(&item.id, running, stepped_back)
        .unwrap();
    let run_failed = archon_core::run::RunState::Failed {
        stage_id: "implement".into(),
        error: "failed again".into(),
    };
    run.finish(run_failed, stepped_back).unwrap();
    let failed = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "failed again".into(),
    };
    store.finish_run(&run, failed, stepped_back).unwrap();

    let now = store.item(&item.id).unwrap();
    assert!(now.state.is_terminal());
    assert_eq!(
        now.updated_at, item_plan.updated_at,
        "the clock stepped back"
    );
    assert!(
        !plan::still_settled(&store, &item_plan).unwrap(),
        "the new run unsettles it"
    );
}

fn settled(item: QueueItem, runs: Vec<PipelineRun>, repo: Option<&Path>) -> Settled {
    let repo = repo.map(|path| archon_core::repo::Repo {
        id: RepoId::new(),
        name: "repo".into(),
        path: path.to_path_buf(),
        ship: archon_core::repo::ShipMode::Pr,
        base_branch: None,
        default_pipeline: None,
        autonomy_max: Default::default(),
        grant_merge: false,
        attribution: Default::default(),
        gh_user: None,
        tiers: Default::default(),
        confirmed_fast_check: None,
        confirmed_full_check: None,
        confirmed_release_check: None,
        added_at: archon_core::now(),
    });
    Settled { item, runs, repo }
}

#[test]
fn plans_remove_old_workspaces_reclaim_recent_ones_and_skip_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let [repo, old, recent, fresh] = ["repo", "old", "recent", "fresh"].map(|n| base.join(n));
    for path in [&repo, &old, &recent, &fresh] {
        std::fs::create_dir(path).unwrap();
    }
    let runs = vec![
        run_in(&old, Some(900)),
        run_in(&recent, Some(120)),
        run_in(&fresh, Some(1)),
        run_in(&base.join("gone"), Some(900)),
        run_in(&repo, Some(900)),
    ];
    let failed = QueueState::Failed {
        run: None,
        error: "e".into(),
    };
    let policy = Policy {
        remove_before: ago(600),
        reclaim_before: ago(60),
        ..policy(false)
    };
    let item = settled(item_in(failed, 900), runs, Some(&repo));
    let plans = plan::plan_all(&[item], &roots_in(&base), &policy);
    assert_eq!(plans.len(), 1);
    let expected = vec![
        candidate(&old, Some(&repo), Action::Remove),
        candidate(&recent, Some(&repo), Action::Reclaim),
    ];
    assert_eq!(plans[0].found.candidates, expected);
}

#[test]
fn a_lost_cleanup_is_owed_removal_once_its_caches_are_due() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let tree = base.join("trees/repo/qi_1");
    std::fs::create_dir_all(&tree).unwrap();
    let mut run = run_in(&tree, Some(120));
    run.shipped = true;
    let state = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "shipped".into(),
    };
    let item = settled(item_in(state, 120), vec![run], Some(&base.join("repo")));
    let policy = Policy {
        remove_before: ago(600),
        reclaim_before: ago(60),
        ..policy(false)
    };
    let owed = Policy {
        owed_cleanup: true,
        ..policy.clone()
    };
    let action = |policy: &Policy| {
        let plans = plan::plan_all(std::slice::from_ref(&item), &roots_in(&base), policy);
        plans[0].found.candidates[0].action
    };
    assert_eq!(action(&policy), Action::Reclaim, "kept on success");
    assert_eq!(action(&owed), Action::Remove);
    let mut unshipped = item.clone();
    unshipped.runs[0].shipped = false;
    let plans = plan::plan_all(&[unshipped], &roots_in(&base), &owed);
    assert_eq!(
        plans[0].found.candidates[0].action,
        Action::Reclaim,
        "success without shipping proof waits for normal retention"
    );
}

/// A shipped team run: what it set aside and its roles' worktrees wait for
/// `gc_after`, as any run's do; only a `main` whose cleanup was lost goes
/// once its caches are due.
#[test]
fn only_the_owed_run_worktree_of_a_team_run_goes_early() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let root = base.join("trees/repo/qi_1");
    add_worktree(&repo, &root.join("build-api"), "run--api-12345678-a1");
    let aside = root.join(".set-aside/build-api-1");
    std::fs::create_dir_all(&aside).unwrap();
    std::fs::write(aside.join("precious.rs"), "fn keep() {}\n").unwrap();
    let mut run = run_in(&root.join("main"), Some(7200));
    run.team_root = Some(root.clone());
    run.shipped = true;
    let state = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "shipped".into(),
    };
    let item = settled(item_in(state, 7200), vec![run], Some(&repo));
    let policy = Policy {
        remove_before: ago(7 * 86_400),
        reclaim_before: ago(3600),
        owed_cleanup: true,
        ..policy(false)
    };
    let (roots, items) = (roots_in(&base), std::slice::from_ref(&item));
    let actions = |plans: &[ItemPlan]| -> Vec<(PathBuf, Action)> {
        let found = &plans[0].found.candidates;
        found
            .iter()
            .map(|c| (c.workspace.clone(), c.action))
            .collect()
    };

    let plans = plan::plan_all(items, &roots, &policy);
    let expected = vec![
        (aside.clone(), Action::Reclaim),
        (root.join("build-api"), Action::Reclaim),
    ];
    assert_eq!(actions(&plans), expected, "main was already removed");
    let found = plans.into_iter().next().unwrap().found;
    let report = sweep::sweep(found, &roots, &policy);
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(aside.join("precious.rs").exists() && root.join("build-api").exists());

    add_worktree(&repo, &root.join("main"), "run");
    let plans = plan::plan_all(items, &roots, &policy);
    let mut owed = expected.clone();
    owed.push((root.join("main"), Action::Remove));
    assert_eq!(actions(&plans), owed, "a lost cleanup");
    let found = plans.into_iter().next().unwrap().found;
    let report = sweep::sweep(found, &roots, &policy);
    assert_eq!(report.removed, vec![root.join("main")], "{report:?}");
    assert!(aside.join("precious.rs").exists() && root.join("build-api").exists());
}

/// The item's repository was unregistered: git still names it in the
/// worktree's `.git` file, so the worktree is removed through it.
#[test]
fn an_unregistered_repository_is_found_through_the_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let repo = base.join("repo");
    init_repo(&repo);
    let tree = base.join("trees/repo/qi_1");
    add_worktree(&repo, &tree, "gc-orphan");
    let mut item = item_in(QueueState::Cancelled { reason: None }, 900);
    item.repo = Some(RepoId::new());
    let orphan = settled(item, vec![run_in(&tree, Some(900))], None);

    let roots = roots_in(&base);
    let plans = plan::plan_all(&[orphan], &roots, &policy(false));
    let found = &plans[0].found;
    assert!(found.unreadable.is_empty(), "{:?}", found.unreadable);
    assert_eq!(found.candidates[0].repo, Some(repo.join(".git")));
    let plan = plans.into_iter().next().unwrap();
    let report = sweep::sweep(plan.found, &roots, &policy(false));
    assert_eq!(report.removed, vec![tree.clone()]);
    assert!(!tree.exists());
    assert!(
        !git(&repo, &["branch", "--list", "gc-orphan"])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_paths_strictly_inside_a_root_count() {
    let root = Path::new("/data/archon/scratch");
    assert!(is_inside(&root.join("qi_1"), root));
    assert!(!is_inside(root, root));
    assert!(!is_inside(
        Path::new("/data/archon/scratch/../../../home"),
        root
    ));
    assert!(!is_inside(Path::new("/data/archon/scratch2/qi_1"), root));
}

#[test]
fn the_policy_follows_the_config_unless_overridden() {
    let config = archon_core::config::WorktreeConfig::default();
    let now = archon_core::now();
    let policy = Policy::new(&config, now, None, false).unwrap();
    assert_eq!(policy.remove_before, now - chrono::Duration::days(7));
    assert_eq!(policy.reclaim_before, now - chrono::Duration::hours(1));
    assert_eq!(policy.settled_before(), policy.reclaim_before);
    let hour = archon_core::age::Age::from_secs(3600);
    let policy = Policy::new(&config, now, Some(hour), true).unwrap();
    assert_eq!(policy.remove_before, now - chrono::Duration::hours(1));
    assert!(policy.force && policy.owed_cleanup);
}

#[test]
fn a_workspace_is_as_recent_as_its_latest_run() {
    let [a, b] = [Path::new("/w/a"), Path::new("/w/b")];
    let runs = [
        run_in(a, Some(900)),
        run_in(a, Some(10)),
        run_in(b, Some(900)),
        run_in(b, None),
    ];
    let last = plan::last_use(&runs);
    let old = |path: &Path| last[path].is_some_and(|at| at < ago(60));
    assert!(!old(a), "a recent retry keeps the worktree");
    assert!(!old(b), "an unfinished run keeps the worktree");
    assert_eq!(last.len(), 2);
}

#[test]
fn configured_zero_disables_each_gc_age_even_under_pressure() {
    use archon_core::age::Age;
    use archon_core::config::Config;
    let mut config = Config::default();
    config.worktrees.gc_after = Age::from_secs(0);
    config.worktrees.cache_after = Age::from_secs(0);
    config.worktrees.shared_cache_after = Age::from_secs(0);
    config.artifacts.keep_after = Age::from_secs(0);
    let now = archon_core::now();
    let policy = Policy::new(&config.worktrees, now, None, false)
        .unwrap()
        .under_pressure(&config.worktrees, now)
        .unwrap();
    assert_eq!(policy.remove_before, Timestamp::MIN_UTC);
    assert_eq!(policy.reclaim_before, Timestamp::MIN_UTC);
    assert!(!policy.owed_cleanup);
    let extras = super::Extras::new(&config, now, None, true).unwrap();
    assert_eq!(extras.cache_before, Timestamp::MIN_UTC);
    assert_eq!(extras.artifacts_before, Timestamp::MIN_UTC);
    let manual = Policy::new(&config.worktrees, now, Some(Age::from_secs(0)), false).unwrap();
    assert_eq!(
        manual.remove_before, now,
        "an explicit --older-than 0s still collects now"
    );
}
