//! `repo-readonly` runs: the agent works in a detached snapshot of
//! the repository's `HEAD` commit under the worktree root, never in the
//! operator's checkout, and the snapshot goes when the run ends, however
//! it ends; gc removes one left behind. Repo-less runs still work in a
//! scratch directory.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::age::Age;
use archon_core::catalog::SNAPSHOT_DIR;
use archon_core::config::Config;
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{EnqueueRequest, Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::StageOutcome;
use archon_daemon::worktree::WorktreeManager;
use common::{Harness, MODEL, REPO_NAME, choice, completed, git};

const READONLY: &str = "ro-ask";

const READONLY_TOML: &str = r#"[pipeline]
name = "ro-ask"
description = "a read-only question"
version = 1
workspace = "repo-readonly"
requires_repo = false

[[stage]]
id = "answer"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30
"#;

fn answer_plan() -> ModelPlan {
    let mut stages = BTreeMap::new();
    stages.insert("answer".to_owned(), vec![choice(MODEL, Effort::Medium)]);
    ModelPlan(stages)
}

/// A harness whose repository carries the read-only pipeline, committed,
/// and an operator checkout with uncommitted and untracked work.
async fn harness() -> Harness {
    harness_with(Config::default()).await
}

async fn harness_with(config: Config) -> Harness {
    let h = Harness::with_config(config).await;
    h.add_pipeline(READONLY, READONLY_TOML);
    git(&h.repo, &["add", "."]);
    git(
        &h.repo,
        &["commit", "--quiet", "-m", "add the read-only pipeline"],
    );
    std::fs::write(h.repo.join("README.md"), "operator's edit\n").expect("edit");
    std::fs::write(h.repo.join("notes.txt"), "operator's notes\n").expect("notes");
    h
}

async fn ask(h: &Harness, title: &str) -> QueueItem {
    h.enqueue_in(READONLY, title, answer_plan())
        .await
        .expect("enqueue")
}

fn snapshot_of(h: &Harness, item: &QueueItem) -> PathBuf {
    h.ctx
        .paths
        .worktrees
        .join(REPO_NAME)
        .join(item.id.as_str())
        .join(SNAPSHOT_DIR)
}

/// What the operator's checkout looks like: its commit, its branch, its
/// status and the content of its edited file.
fn checkout_state(repo: &Path) -> [String; 4] {
    [
        git(repo, &["rev-parse", "HEAD"]),
        git(repo, &["symbolic-ref", "HEAD"]),
        git(repo, &["status", "--porcelain"]),
        std::fs::read_to_string(repo.join("README.md")).expect("readme"),
    ]
}

/// `HEAD` is detached in `tree`: `symbolic-ref --quiet` exits 1.
fn is_detached(tree: &Path) -> bool {
    let status = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .status()
        .expect("git runs");
    status.code() == Some(1)
}

/// The snapshot and its item directory are gone, and git lists neither.
fn assert_snapshot_removed(h: &Harness, snapshot: &Path) {
    assert!(!snapshot.exists(), "{} is left behind", snapshot.display());
    let item_dir = snapshot.parent().expect("item directory");
    assert!(!item_dir.exists(), "{} is left behind", item_dir.display());
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains(SNAPSHOT_DIR),
        "git still lists it: {listed}"
    );
}

#[tokio::test]
async fn an_ask_run_works_in_a_detached_snapshot_of_head() {
    let h = harness().await;
    let before = checkout_state(&h.repo);
    let item = ask(&h, "where is the greeting?").await;
    h.dispatch().await;

    let snapshot = snapshot_of(&h, &item);
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "one answer agent");
    assert_eq!(requests[0].workspace, snapshot, "the agent works in it");
    assert_ne!(requests[0].workspace, h.repo);
    assert!(requests[0].instructions.contains("snapshot"));
    assert_eq!(h.run(&item.id).await.workspace, snapshot);
    assert!(is_detached(&snapshot), "the snapshot is on no branch");
    assert_eq!(git(&snapshot, &["rev-parse", "HEAD"]), before[0]);
    let committed = std::fs::read_to_string(snapshot.join("README.md")).expect("readme");
    assert_eq!(committed, "demo\n", "the snapshot holds the committed tree");
    assert!(!snapshot.join("notes.txt").exists(), "nor untracked work");

    // What a build in the snapshot writes stays out of the operator's tree.
    std::fs::write(snapshot.join("build.log"), "built\n").expect("build");
    std::fs::write(snapshot.join("README.md"), "formatted\n").expect("format");
    h.settle_running(|_| completed("the greeting is in README.md"))
        .await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_snapshot_removed(&h, &snapshot);
    assert_eq!(checkout_state(&h.repo), before, "the checkout is untouched");
    assert!(!h.repo.join("build.log").exists());
}

#[tokio::test]
async fn the_snapshot_is_removed_when_the_run_fails() {
    let h = harness().await;
    let before = checkout_state(&h.repo);
    let item = ask(&h, "fails").await;
    h.dispatch().await;
    let snapshot = snapshot_of(&h, &item);
    assert!(
        snapshot.join("README.md").exists(),
        "the run has its snapshot"
    );
    std::fs::write(snapshot.join("partial.txt"), "left\n").expect("write");
    let failed = StageOutcome::Failed {
        error: "cannot tell".into(),
        infra: false,
    };
    h.settle_running(|_| failed.clone()).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_snapshot_removed(&h, &snapshot);
    assert_eq!(checkout_state(&h.repo), before);
}

#[tokio::test]
async fn the_snapshot_is_removed_when_the_item_is_cancelled() {
    let h = harness().await;
    let before = checkout_state(&h.repo);
    let item = ask(&h, "cancelled").await;
    h.dispatch().await;
    let snapshot = snapshot_of(&h, &item);
    assert!(
        snapshot.join("README.md").exists(),
        "the run has its snapshot"
    );
    // Its agent locked it and left work in it: neither keeps it.
    git(&h.repo, &["worktree", "lock", snapshot.to_str().unwrap()]);
    std::fs::write(snapshot.join("scratch.txt"), "wip\n").expect("write");
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("no longer needed".into()),
    };
    h.cli(cancel).await.expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert_snapshot_removed(&h, &snapshot);
    assert_eq!(checkout_state(&h.repo), before);
}

/// A retried item's new run replaces the snapshot an earlier run left
/// behind with a fresh one of the current `HEAD`.
#[tokio::test]
async fn a_leftover_snapshot_is_replaced_by_a_fresh_one() {
    let h = harness().await;
    let item = ask(&h, "asked again").await;
    let manager = WorktreeManager::new(h.ctx.paths.worktrees.clone(), "archon/".into());
    let earlier = manager
        .create_snapshot(&h.repo, REPO_NAME, &item.id)
        .expect("an earlier run's snapshot");
    assert_eq!(earlier.path, snapshot_of(&h, &item));
    std::fs::write(earlier.path.join("stale.txt"), "stale\n").expect("write");
    git(
        &h.repo,
        &["commit", "--quiet", "--allow-empty", "-m", "later"],
    );

    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(
        requests.len(),
        1,
        "the run started: {:?}",
        h.state(&item.id).await
    );
    assert_eq!(requests[0].workspace, earlier.path);
    assert!(!earlier.path.join("stale.txt").exists(), "a fresh snapshot");
    let head = git(&h.repo, &["rev-parse", "HEAD"]);
    assert_eq!(git(&earlier.path, &["rev-parse", "HEAD"]), head);
    assert_ne!(head, earlier.commit, "of the current HEAD");
}

/// A snapshot path that is a symbolic link to another worktree is never
/// removed through: the start fails and the other worktree is untouched.
#[tokio::test]
async fn a_symlinked_leftover_snapshot_is_refused() {
    let h = harness().await;
    let item = ask(&h, "symlinked").await;
    let other = h.ctx.paths.worktrees.join(REPO_NAME).join("qi_other");
    git(
        &h.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join("wip.txt"), "unsaved\n").expect("write");
    let snapshot = snapshot_of(&h, &item);
    std::fs::create_dir_all(snapshot.parent().unwrap()).expect("item directory");
    std::os::unix::fs::symlink(&other, &snapshot).expect("symlink");

    h.dispatch().await;
    let QueueState::Failed { run: None, error } = h.state(&item.id).await else {
        panic!("expected a refused start: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("symbolic link"), "{error}");
    assert!(other.join("wip.txt").exists(), "the other worktree stays");
    assert!(h.backend.requests().is_empty(), "no agent was started");
}

/// An item directory that is a symbolic link is refused too, whether a
/// snapshot would be removed or added through it: neither another item's
/// snapshot nor a directory elsewhere is touched.
#[tokio::test]
async fn a_symlinked_item_directory_is_refused() {
    let h = harness().await;
    let item = ask(&h, "symlinked item").await;
    let manager = WorktreeManager::new(h.ctx.paths.worktrees.clone(), "archon/".into());
    let other = manager
        .create_snapshot(&h.repo, REPO_NAME, &archon_core::id::QueueItemId::new())
        .expect("another item's snapshot");
    std::fs::write(other.path.join("wip.txt"), "in use\n").expect("write");
    let snapshot = snapshot_of(&h, &item);
    let item_dir = snapshot.parent().unwrap();
    std::os::unix::fs::symlink(other.path.parent().unwrap(), item_dir).expect("symlink");

    let refused = manager
        .remove_snapshot(&h.repo, &snapshot)
        .expect_err("not removed through a symlinked item directory");
    assert!(refused.to_string().contains("symbolic link"), "{refused}");
    assert!(
        other.path.join("wip.txt").exists(),
        "the other snapshot stays"
    );

    let elsewhere = h.ctx.paths.worktrees.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::remove_file(item_dir).expect("unlink");
    std::os::unix::fs::symlink(&elsewhere, item_dir).expect("symlink");
    h.dispatch().await;
    let QueueState::Failed { run: None, error } = h.state(&item.id).await else {
        panic!("expected a refused start: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("symbolic link"), "{error}");
    let added = std::fs::read_dir(&elsewhere).expect("read").count();
    assert_eq!(added, 0, "no snapshot was added where it leads");
    assert!(h.backend.requests().is_empty(), "no agent was started");
}

/// A pipeline changed from `repo-readonly` to `worktree`: the snapshot an
/// earlier run left is no team run, and goes for the item's worktree.
#[tokio::test]
async fn a_worktree_run_replaces_a_leftover_snapshot() {
    let h = harness().await;
    let item = ask(&h, "now edits").await;
    let manager = WorktreeManager::new(h.ctx.paths.worktrees.clone(), "archon/".into());
    let leftover = manager
        .create_snapshot(&h.repo, REPO_NAME, &item.id)
        .expect("an earlier run's snapshot");
    std::fs::write(leftover.path.join("stale.txt"), "stale\n").expect("write");

    let branch = manager.branch_name(&item.id, "now edits", 0);
    let tree = manager
        .create(&h.repo, REPO_NAME, &item.id, &branch, None)
        .expect("the leftover snapshot is not taken for a team run");
    assert_eq!(Some(tree.path.as_path()), leftover.path.parent());
    assert!(tree.added, "a new worktree");
    assert_eq!(git(&tree.path, &["branch", "--show-current"]), branch);
    assert!(!leftover.path.exists(), "the snapshot went");
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains(SNAPSHOT_DIR),
        "git lists it no more: {listed}"
    );
}

/// A start refused after its snapshot was added removes the snapshot.
#[tokio::test]
async fn an_abandoned_start_removes_its_snapshot() {
    let h = harness().await;
    let item = ask(&h, "drifted plan").await;
    let extra = READONLY_TOML.to_owned()
        + "\n[[stage]]\nid = \"more\"\nkind = \"work\"\nprompt = \"implement.md\"\ntimeout_minutes = 30\n";
    h.add_pipeline(READONLY, &extra);
    h.dispatch().await;
    let QueueState::Failed { run: None, error } = h.state(&item.id).await else {
        panic!("expected a refused start: {:?}", h.state(&item.id).await);
    };
    assert!(
        error.contains("more"),
        "refused after the snapshot: {error}"
    );
    assert_snapshot_removed(&h, &snapshot_of(&h, &item));
}

/// A snapshot whose directory vanished while its agent held a lock on it is
/// still deregistered, so the item's next run gets a fresh one.
#[tokio::test]
async fn a_vanished_locked_snapshot_does_not_block_the_next_run() {
    let h = harness().await;
    let item = ask(&h, "vanished").await;
    let manager = WorktreeManager::new(h.ctx.paths.worktrees.clone(), "archon/".into());
    let earlier = manager
        .create_snapshot(&h.repo, REPO_NAME, &item.id)
        .expect("an earlier run's snapshot");
    git(
        &h.repo,
        &["worktree", "lock", earlier.path.to_str().unwrap()],
    );
    std::fs::remove_dir_all(&earlier.path).expect("vanish");

    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(requests[0].workspace, earlier.path);
    assert!(earlier.path.join("README.md").exists(), "a fresh snapshot");
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("locked"), "the old lock went: {listed}");
}

/// A snapshot archond could not remove when its run ended (here, added
/// again by hand, dirty and locked) is collected by gc without `--force`.
#[tokio::test]
async fn gc_collects_a_leftover_snapshot() {
    let mut config = Config::default();
    config.worktrees.cache_after = Age::from_secs(0);
    let h = harness_with(config).await;
    let before = checkout_state(&h.repo);
    let item = ask(&h, "left behind").await;
    h.dispatch().await;
    h.settle_running(|_| completed("answered")).await;
    let snapshot = snapshot_of(&h, &item);
    assert_snapshot_removed(&h, &snapshot);
    let manager = WorktreeManager::new(h.ctx.paths.worktrees.clone(), "archon/".into());
    manager
        .create_snapshot(&h.repo, REPO_NAME, &item.id)
        .expect("a leftover snapshot");
    std::fs::write(snapshot.join("build.log"), "built\n").expect("build");
    git(&h.repo, &["worktree", "lock", snapshot.to_str().unwrap()]);

    let request = Request::Gc {
        older_than_secs: None,
        force: false,
    };
    let Ok(Reply::Gc(report)) = h.cli(request).await else {
        panic!("gc replies with its report");
    };
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.removed, vec![snapshot.clone()], "{report:?}");
    assert_snapshot_removed(&h, &snapshot);
    assert_eq!(checkout_state(&h.repo), before, "the checkout is untouched");
}

/// Without a repository, a read-only pipeline still works in the item's
/// scratch directory.
#[tokio::test]
async fn a_repo_less_ask_works_in_scratch() {
    let h = Harness::new().await;
    let request = EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: None,
        pipeline: Some("ask".into()),
        title: "what is a monad?".into(),
        brief: "in one line".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: answer_plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.expect("enqueue") else {
        panic!("enqueue replies with the item");
    };
    h.dispatch().await;
    let scratch = h.ctx.paths.scratch.join(item.id.as_str());
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(requests[0].workspace, scratch);
    h.settle_running(|_| completed("a monoid in the category of endofunctors"))
        .await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(scratch.is_dir(), "a scratch directory is kept for gc");
}
