//! Worktree gc (ARCHITECTURE §13.2) over real git worktrees: build caches
//! go while source stays, settled worktrees go but branches stay, live and
//! retried runs are never touched, an unregistered repository's worktrees
//! are still collected, offline gc, and archond's own passes.

use crate::common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compose_teardown::{FakeDocker, container};
use archon_aether::client::AetherClient;
use archon_aether::fake::{FakeAether, Scripted};
use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::notify::FeedEntry;
use archon_core::protocol::{AetherGc, GcReport, Reply, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use archon_daemon::gc;
use common::{Harness, REPO_NAME, completed, git, verdict};

const CACHE_BYTES: usize = 256 << 10;

/// Caches become due at once; removal waits for the default week.
fn caches_due_at_once() -> Config {
    let mut config = Config::default();
    config.worktrees.cache_after = Age::from_secs(1);
    config
}

/// The report without the live repository cache pass notes it skipped.
fn untouched(mut report: GcReport) -> GcReport {
    report.skipped.clear();
    report
}

/// A harness whose repository ignores `target/` and `*.log`.
async fn harness(config: Config) -> Harness {
    let h = Harness::with_config(config).await;
    std::fs::write(h.repo.join(".gitignore"), "target/\n*.log\n").expect("gitignore");
    git(&h.repo, &["add", ".gitignore"]);
    git(&h.repo, &["commit", "--quiet", "-m", "ignore build output"]);
    h
}

/// Build output in the worktree `tree`: an ignored `target/` and log.
fn build_in(tree: &Path) {
    let target = tree.join("target/debug");
    std::fs::create_dir_all(&target).expect("target dir");
    std::fs::write(target.join("app"), vec![7u8; CACHE_BYTES]).expect("binary");
    std::fs::write(tree.join("build.log"), "compiled\n").expect("log");
}

/// An item whose run started and was cancelled, keeping its worktree, now
/// holding build output.
async fn cancelled_run(h: &Harness, title: &str) -> (QueueItem, PathBuf) {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert!(tree.join("README.md").exists(), "a cancelled run keeps it");
    build_in(&tree);
    age_finished_runs(h, &item).await;
    (item, tree)
}

async fn age_finished_runs(h: &Harness, item: &QueueItem) {
    let mut store = h.ctx.store.lock().await;
    let mut settled = store.item(&item.id).unwrap();
    settled.updated_at -= chrono::Duration::seconds(2);
    store.update_item(&settled).unwrap();
    for mut run in store.runs_for_item(&item.id).unwrap() {
        if let Some(at) = &mut run.finished_at {
            *at -= chrono::Duration::seconds(2);
            store.update_run(&run).unwrap();
        }
    }
}

async fn gc_request(h: &Harness, older_than_secs: Option<u64>, force: bool) -> GcReport {
    let request = Request::Gc {
        older_than_secs,
        force,
        preview: false,
    };
    match h.cli(request).await {
        Ok(Reply::Gc(report)) => *report,
        other => panic!("gc: {other:?}"),
    }
}

fn branch_exists(h: &Harness, branch: &str) -> bool {
    let listed = git(&h.repo, &["branch", "--list", branch]);
    !listed.is_empty()
}

#[tokio::test]
async fn a_settled_worktree_loses_its_caches_then_goes_but_keeps_its_branch() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "leaves build output").await;
    let branch = h.run(&item.id).await.branch;
    std::fs::write(tree.join("notes.md"), "unsaved work\n").expect("notes");

    let report = gc_request(&h, None, false).await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.reclaimed.len(), 1, "{report:?}");
    assert_eq!(report.reclaimed[0].path, tree);
    assert!(
        report.reclaimed[0].bytes >= CACHE_BYTES as u64,
        "{report:?}"
    );
    assert_eq!(report.bytes_freed, report.reclaimed[0].bytes);
    assert!(report.removed.is_empty(), "removal waits for gc_after");
    assert!(!tree.join("target").exists(), "known build output goes");
    assert!(tree.join("build.log").exists(), "other ignored files stay");
    assert!(tree.join("notes.md").exists(), "untracked source stays");
    assert!(tree.join("README.md").exists());

    let report = gc_request(&h, Some(0), false).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].1.contains("uncommitted"), "{report:?}");
    assert!(tree.join("notes.md").exists(), "dirty work stays");

    std::fs::remove_file(tree.join("notes.md")).expect("clean up");
    let report = gc_request(&h, Some(0), false).await;
    assert_eq!(report.removed, vec![tree.clone()]);
    assert!(!tree.exists());
    assert!(branch_exists(&h, &branch), "branches are kept");
}

/// A Compose stack a run left is stopped when the run ends, its volumes
/// kept for a retry, and they go when gc removes the worktree.
#[tokio::test]
async fn a_runs_compose_stack_stops_at_its_end_and_its_volumes_go_with_the_worktree() {
    let docker = FakeDocker::new();
    let mut config = Config::default();
    config.worktrees.docker = docker.binary();
    let h = harness(config).await;
    let item = h.enqueue("starts a stack").await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    let project = format!("{}-review", item.id.as_str().to_lowercase());
    docker.lists("ps", &[container(&project, &tree)]);
    docker.lists("volumes", &[format!("{project}_db")]);
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    h.dispatch().await;
    let down = format!("compose --project-name {project} down --remove-orphans");
    assert_eq!(
        docker.downs(),
        std::slice::from_ref(&down),
        "{:?}",
        docker.calls()
    );

    age_finished_runs(&h, &item).await;
    let report = gc_request(&h, Some(0), false).await;
    assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
    let calls = docker.calls();
    let rm = format!("volume rm {project}_db");
    assert!(calls.contains(&rm), "{calls:?}");
    assert_eq!(docker.downs(), [down], "nothing else was torn down");
}

async fn retry(h: &Harness, item: &QueueItem) {
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
}

async fn runs_of(h: &Harness, item: &QueueItem) -> Vec<archon_core::run::PipelineRun> {
    let store = h.ctx.store.lock().await;
    store.runs_for_item(&item.id).expect("runs")
}

/// A retry between planning and collecting: the pass planned the cancelled
/// item, the retry re-queued it, so the pass leaves its worktree alone.
#[tokio::test]
async fn a_retry_after_a_pass_planned_keeps_the_worktree_it_reuses() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "retried meanwhile").await;
    let pass = gc::plan(&h.ctx, Some(Age::from_secs(0)), true)
        .await
        .expect("plan");
    assert_eq!(pass.items(), 1, "{pass:?}");
    retry(&h, &item).await;

    let report = gc::collect_planned(&h.ctx, pass).await.expect("pass");
    assert_eq!(untouched(report), GcReport::default());
    assert!(
        tree.join("target/debug/app").exists(),
        "nothing was deleted"
    );
    h.dispatch().await;
    let runs = runs_of(&h, &item).await;
    assert_eq!(runs.len(), 2, "the retry started");
    assert_eq!(runs[1].workspace, tree, "in the same worktree");
}

/// A retry while a pass collects the item: the item is queued again, but
/// no run starts until the pass lets go of it, and then one does.
#[tokio::test]
async fn a_retry_while_a_pass_collects_waits_for_it() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "retried during a pass").await;
    let mark = h.ctx.collecting.mark(item.id.clone()).expect("unmarked");
    retry(&h, &item).await;
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert_eq!(runs_of(&h, &item).await.len(), 1, "no run started");

    drop(mark);
    assert!(!h.ctx.collecting.contains(&item.id));
    h.dispatch().await;
    let runs = runs_of(&h, &item).await;
    assert_eq!(runs.len(), 2, "the retry started once the pass let go");
    assert_eq!(runs[1].workspace, tree);
    let report = gc_request(&h, Some(0), true).await;
    assert_eq!(
        untouched(report),
        GcReport::default(),
        "the live run is left alone"
    );
}

/// A pass aborted mid-sweep (supervise stops the workers on a full disk,
/// or the held-dispatch pass ran out of time) leaves its sweep running with
/// the item's mark. A later `archon gc`, served in standby, plans the same
/// item and reports it left to that sweep. The sweep pokes dispatch as it
/// lets go, and a pass after it collects the item.
#[tokio::test]
async fn a_pass_leaves_an_item_an_aborted_sweep_still_holds() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "swept by an aborted pass").await;
    let orphaned = h.ctx.collecting.mark(item.id.clone()).expect("unmarked");
    assert!(h.ctx.collecting.mark(item.id.clone()).is_none());

    let report = gc_request(&h, Some(0), true).await;
    let [(skipped, why)] = &report.skipped[..] else {
        panic!("the held item is reported skipped: {report:?}");
    };
    assert_eq!(skipped, &tree);
    assert!(
        why.contains("stopped gc pass") && why.contains(item.id.as_str()),
        "{why}"
    );
    assert!(
        report.removed.is_empty() && report.reclaimed.is_empty(),
        "{report:?}"
    );
    assert!(tree.join("target/debug/app").exists());
    assert!(h.ctx.collecting.contains(&item.id), "its mark is kept");

    // What enqueue and cancel poked is spent first.
    let poked = || tokio::time::timeout(Duration::ZERO, h.ctx.dispatch.notified());
    while poked().await.is_ok() {}
    drop(orphaned);
    assert!(poked().await.is_ok(), "letting go pokes dispatch");
    let report = gc_request(&h, Some(0), true).await;
    assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
    assert!(!tree.exists() && !h.ctx.collecting.contains(&item.id));
}

#[tokio::test]
async fn a_live_run_is_never_touched() {
    let h = harness(caches_due_at_once()).await;
    let item = h.enqueue("still running").await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    build_in(&tree);
    std::fs::write(tree.join("wip.rs"), "fn main() {}\n").expect("source");

    let report = gc_request(&h, Some(0), true).await;
    assert_eq!(untouched(report), GcReport::default());
    assert!(tree.join("target/debug/app").exists() && tree.join("wip.rs").exists());
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn a_worktree_of_an_unregistered_repository_is_still_collected() {
    let h = harness(Config::default()).await;
    let (item, tree) = cancelled_run(&h, "outlives its repo").await;
    let branch = h.run(&item.id).await.branch;
    let remove = Request::RepoRemove {
        repo: archon_core::protocol::RepoRef(REPO_NAME.into()),
    };
    h.cli(remove).await.expect("unregister");

    let report = gc_request(&h, Some(0), false).await;
    assert!(
        report.failed.is_empty() && report.skipped.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.removed, vec![tree.clone()]);
    assert!(!tree.exists());
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains(tree.to_str().expect("utf-8")), "{listed}");
    assert!(branch_exists(&h, &branch), "branches are kept");
}

#[tokio::test]
async fn offline_gc_collects_only_while_no_archond_holds_the_lock() {
    let h = harness(Config::default()).await;
    let (item, tree) = cancelled_run(&h, "collected offline").await;
    let (config, paths) = (&h.ctx.config, &h.ctx.paths);
    let older_than = Some(Age::from_secs(0));

    let report = gc::collect_offline(config, paths, older_than, false).expect("offline gc");
    assert_eq!(report.removed, vec![tree.clone()]);
    assert!(report.bytes_freed >= CACHE_BYTES as u64, "{report:?}");
    assert!(!tree.exists());

    let _daemon = lock_when_free(&paths.socket).await;
    let refused = gc::collect_offline(config, paths, older_than, false);
    assert!(
        matches!(refused, Err(ArchonError::Unavailable(_))),
        "{refused:?}"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

/// The daemon lock, once free: a git another test forks while the pass
/// holds it keeps it until that git starts.
async fn lock_when_free(socket: &Path) -> archon_daemon::server::DaemonLock {
    for _ in 0..500 {
        match archon_daemon::server::lock_daemon(socket) {
            Ok(lock) => return lock,
            Err(ArchonError::Unavailable(_)) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("locking: {e}"),
        }
    }
    panic!("the daemon lock stayed held");
}

#[tokio::test]
async fn archond_passes_at_startup_and_before_a_dispatch_held_for_disk() {
    let h = harness(caches_due_at_once()).await;
    let (_, tree) = cancelled_run(&h, "reclaimed at startup").await;
    let pass = tokio::spawn(gc::run(h.ctx.clone()));
    let gone = tokio::time::timeout(Duration::from_secs(20), async {
        while tree.join("target").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    gone.await.expect("the startup pass deletes the caches");
    pass.abort();
    assert!(
        tree.join("README.md").exists(),
        "the worktree waits for gc_after"
    );

    let h = harness(caches_due_at_once()).await;
    let (_, tree) = cancelled_run(&h, "reclaimed for disk").await;
    h.dispatch().await;
    assert!(
        tree.join("target").exists(),
        "no pass while the disk has room"
    );
    h.set_free_bytes(1 << 30);
    h.dispatch().await;
    assert!(
        !tree.join("target").exists(),
        "the held dispatch ran a pass"
    );
    assert!(h.ctx.gc.lock().await.is_some(), "the pass was recorded");
}

/// What a well-behaved agent of each stage reports.
fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        _ => completed("done"),
    }
}

/// Run the item's pipeline to success; `in_tree` runs before it ships.
async fn ship(h: &Harness, title: &str, in_tree: impl FnOnce(&Path)) -> (QueueItem, PathBuf) {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    in_tree(&tree);
    for _ in ["implement", "review", "ship"] {
        h.settle_running(passing).await;
        h.dispatch().await;
    }
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    age_finished_runs(h, &item).await;
    (item, tree)
}

#[tokio::test]
async fn shipping_deletes_the_caches_even_of_a_worktree_it_cannot_remove() {
    let h = harness(Config::default()).await;
    let (_, tree) = ship(&h, "leaves a stray file", |tree| {
        build_in(tree);
        std::fs::write(tree.join("stray.txt"), "untracked\n").expect("stray");
    })
    .await;
    assert!(
        tree.join("stray.txt").exists(),
        "an unforced remove keeps it"
    );
    assert!(!tree.join("target").exists(), "known build output goes");
    assert!(tree.join("build.log").exists(), "other ignored files stay");
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    let noted = feed.iter().any(|item| {
        matches!(&item.entry, FeedEntry::System { text }
            if text.contains("Could not remove") && text.contains("build caches were deleted"))
    });
    assert!(noted, "the operator learns what was freed");
}

/// A crash between finishing a shipped run and removing its worktree: the
/// pass removes it once its caches are due, not a week later.
#[tokio::test]
async fn a_cleanup_lost_after_shipping_is_caught_by_the_pass() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = ship(&h, "shipped", |_| {}).await;
    assert!(!tree.exists(), "archond removed it after shipping");
    let branch = h.run(&item.id).await.branch;
    git(
        &h.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            tree.to_str().expect("utf-8"),
            &branch,
        ],
    );
    build_in(&tree);

    let report = gc_request(&h, None, false).await;
    assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
    assert!(!tree.exists());
    assert!(branch_exists(&h, &branch));
}

#[tokio::test]
async fn a_kept_shipped_worktree_only_loses_its_caches() {
    let mut config = caches_due_at_once();
    config.worktrees.keep_on_success = true;
    let h = harness(config).await;
    let (_, tree) = ship(&h, "kept on success", build_in).await;
    assert!(tree.join("target").exists(), "kept whole on success");

    let report = gc_request(&h, None, false).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.reclaimed.len(), 1, "{report:?}");
    assert!(tree.join("README.md").exists() && !tree.join("target").exists());
}

fn fresh_retry(item: &QueueItem) -> Request {
    Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: true,
        brief_append: None,
        stage_timeouts: Default::default(),
    }
}

/// The real directories directly in `dir`, sorted.
fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| entry.expect("entry").path().canonicalize().expect("real"))
        .collect();
    dirs.sort();
    dirs
}

/// A fresh retry would move the worktree a pass is removing: it is refused
/// until the pass lets go of the item, and nothing moves meanwhile.
#[tokio::test]
async fn a_fresh_retry_is_refused_while_a_pass_collects_the_item() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "fresh during a pass").await;
    let mark = h.ctx.collecting.mark(item.id.clone());
    let refused = h.cli(fresh_retry(&item)).await.expect_err("collecting");
    assert!(
        matches!(&refused, ArchonError::Unavailable(why) if why.contains("gc")),
        "{refused:?}"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert!(tree.join(".git").exists(), "nothing was moved");

    drop(mark);
    h.cli(fresh_retry(&item)).await.expect("fresh retry");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert!(!tree.exists(), "the old worktree was set aside");
}

/// What a fresh retry set aside of a flat item's worktree goes with the
/// item's settled worktree, and every branch stays.
#[tokio::test]
async fn a_pass_collects_what_a_fresh_retry_set_aside() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "retried fresh").await;
    let old_branch = h.run(&item.id).await.branch;
    h.cli(fresh_retry(&item)).await.expect("fresh retry");
    h.dispatch().await;
    let fresh = runs_of(&h, &item).await.pop().expect("the fresh run");
    assert_eq!(
        fresh.workspace, tree,
        "the fresh run reuses the item's path"
    );
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let kept = dirs_in(&h.ctx.paths.worktrees.join(REPO_NAME).join(".set-aside"));
    let git_kept = dirs_in(&h.repo.join(".git/archon-set-aside"));
    assert_eq!(
        (kept.len(), git_kept.len()),
        (1, 1),
        "{kept:?} {git_kept:?}"
    );

    let mut expected = vec![tree.canonicalize().expect("real"), kept[0].clone()];
    expected.push(git_kept[0].clone());

    let report = gc_request(&h, Some(0), false).await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    expected.sort();
    let mut removed = report.removed.clone();
    removed.sort();
    assert_eq!(removed, expected, "{report:?}");
    assert!(!kept[0].exists() && !git_kept[0].exists());
    assert!(branch_exists(&h, &old_branch), "the old branch stays");
    assert!(branch_exists(&h, &fresh.branch), "the fresh branch stays");
}

/// A report-only success retains even a clean worktree; a repository's
/// ship mode cannot prove that this particular pipeline shipped.
#[tokio::test]
async fn report_only_success_keeps_scratch_and_clean_worktrees_until_gc_after() {
    for workspace in ["scratch", "worktree"] {
        let h = harness(caches_due_at_once()).await;
        h.add_pipeline(
            "report-only",
            &format!(
                r#"
[pipeline]
name = "report-only"
description = "report"
version = 1
workspace = "{workspace}"
requires_repo = true
[[stage]]
id = "ship"
kind = "report"
prompt = "implement.md"
"#
            ),
        );
        let item = h
            .enqueue_in(
                "report-only",
                "report",
                archon_core::model::ModelPlan(std::collections::BTreeMap::from([(
                    "ship".into(),
                    common::plan().0["ship"].clone(),
                )])),
            )
            .await
            .unwrap();
        h.dispatch().await;
        let tree = h.run(&item.id).await.workspace;
        h.settle_running(|_| completed("report complete")).await;
        let run = h.run(&item.id).await;
        assert!(matches!(
            h.state(&item.id).await,
            QueueState::Succeeded { .. }
        ));
        assert!(!run.shipped);
        assert!(tree.exists(), "finish keeps {workspace}");
        if workspace == "worktree" {
            assert!(
                git(&tree, &["status", "--porcelain"]).is_empty(),
                "clean worktree"
            );
        }
        for force in [false, true] {
            for _ in 0..3 {
                let pass = gc_request(&h, None, force).await;
                assert!(pass.failed.is_empty(), "{pass:?}");
                assert!(pass.removed.is_empty(), "{workspace}: {pass:?}");
                assert!(tree.exists());
            }
        }
        let pass = gc_request(&h, Some(0), false).await;
        assert_eq!(pass.removed, vec![tree.clone()], "cutoff still applies");
        assert!(!tree.exists());
    }
}

#[tokio::test]
async fn a_custom_named_ship_stage_still_gets_early_cleanup() {
    let h = harness(Config::default()).await;
    h.add_pipeline(
        "custom-ship",
        r#"
[pipeline]
name = "custom-ship"
description = "ship"
version = 1
workspace = "worktree"
requires_repo = true
[[stage]]
id = "publish-change"
kind = "ship"
prompt = "implement.md"
"#,
    );
    let item = h
        .enqueue_in(
            "custom-ship",
            "ships",
            archon_core::model::ModelPlan(std::collections::BTreeMap::from([(
                "publish-change".into(),
                common::plan().0["ship"].clone(),
            )])),
        )
        .await
        .unwrap();
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    h.settle_running(|_| completed("shipped")).await;
    let run = h.run(&item.id).await;
    assert!(run.shipped);
    assert!(!tree.exists(), "finish removes a shipped worktree");
}

/// An item that never built frees nothing, so no "Freed 0.0 GiB" notice.
#[tokio::test]
async fn releasing_zero_bytes_posts_no_notice_but_real_bytes_do() {
    let h = harness(Config::default()).await;
    let item = archon_core::id::QueueItemId::new();
    let nothing = archon_core::protocol::Reclaimed {
        path: h.repo.join("target"),
        bytes: 0,
    };
    gc::announce_item_release(&h.ctx, &item, &nothing)
        .await
        .expect("zero bytes");
    let notices = |feed: Vec<archon_core::notify::FeedItem>| {
        feed.iter()
            .filter(|f| matches!(&f.entry, FeedEntry::System { text } if text.contains("of build output")))
            .count()
    };
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    assert_eq!(notices(feed), 0);
    let some = archon_core::protocol::Reclaimed {
        bytes: 5 << 30,
        ..nothing
    };
    gc::announce_item_release(&h.ctx, &item, &some)
        .await
        .expect("real bytes");
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    assert_eq!(notices(feed), 1);
}

/// Queue a refresh of the finished `owner`, which works in its worktree.
async fn queue_refresh(h: &Harness, owner: &QueueItem) -> QueueItem {
    let mut store = h.ctx.store.lock().await;
    let owner = store.item(&owner.id).expect("owner");
    let run = store
        .runs_for_item(&owner.id)
        .expect("runs")
        .pop()
        .expect("a run");
    let sha = git(&h.repo, &["rev-parse", "main"]);
    let refresh = archon_queue::store::stacks::Refresh {
        base: "main".into(),
        sha,
    };
    store
        .enqueue_refresh(&owner, &run, &refresh)
        .expect("refresh queued")
}

async fn cancel(h: &Harness, item: &QueueItem) {
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

/// A cancelled owner and a cancelled first refresh that ran in its worktree,
/// both settled long enough ago for gc to remove it.
async fn owner_and_finished_refresh(h: &Harness) -> (QueueItem, QueueItem, PathBuf) {
    let (owner, tree) = cancelled_run(h, "refreshed twice").await;
    let first = queue_refresh(h, &owner).await;
    h.dispatch().await;
    assert_eq!(
        h.run(&first.id).await.workspace,
        tree,
        "it borrows the worktree"
    );
    cancel(h, &first).await;
    age_finished_runs(h, &first).await;
    (owner, first, tree)
}

fn kept_for(report: &GcReport, tree: &Path, user: &QueueItem) -> bool {
    report
        .skipped
        .iter()
        .any(|(path, why)| path == tree && why.contains(user.id.as_str()))
}

/// The worktree of a finished item is kept while a refresh of it is queued
/// or running, even though an earlier refresh that ran there has settled,
/// and goes once all of them have finished.
#[tokio::test]
async fn a_worktree_stays_while_any_refresh_of_its_item_is_queued_or_running() {
    let h = harness(caches_due_at_once()).await;
    let (owner, _, tree) = Box::pin(owner_and_finished_refresh(&h)).await;
    let second = queue_refresh(&h, &owner).await;

    let report = gc_request(&h, Some(0), true).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(kept_for(&report, &tree, &second), "{report:?}");
    assert!(
        tree.join("target/debug/app").exists(),
        "nothing was deleted"
    );

    h.dispatch().await;
    assert!(matches!(
        h.state(&second.id).await,
        QueueState::Running { .. }
    ));
    assert_eq!(h.run(&second.id).await.workspace, tree);
    let report = gc_request(&h, Some(0), true).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(tree.join("target/debug/app").exists() && tree.join("README.md").exists());

    cancel(&h, &second).await;
    age_finished_runs(&h, &second).await;
    let report = gc_request(&h, Some(0), true).await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert!(!tree.exists(), "it goes once every user finished");
}

/// A refresh queued again for the same base commit takes the settled
/// first one's place in its owner's index. A retry of that first one works
/// in the worktree too, so its recent run keeps the worktree from a pass.
#[tokio::test]
async fn a_retried_replaced_refresh_keeps_the_worktree_it_just_used() {
    let h = harness(caches_due_at_once()).await;
    let (owner, first, tree) = Box::pin(owner_and_finished_refresh(&h)).await;
    let second = queue_refresh(&h, &owner).await;
    assert_ne!(second.id, first.id, "the settled refresh is not reused");
    h.dispatch().await;
    cancel(&h, &second).await;
    for item in [&owner, &first, &second, &owner, &first, &second] {
        age_finished_runs(&h, item).await;
    }
    let retry = Request::RetryItem {
        item: first.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("the first refresh retried");
    h.dispatch().await;
    let runs = h.ctx.store.lock().await.runs_for_item(&first.id).unwrap();
    assert_eq!(runs.len(), 2, "a new run");
    assert_eq!(runs[1].workspace, tree, "the retry works in the worktree");
    cancel(&h, &first).await;

    let report = gc_request(&h, Some(3), true).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(
        tree.join("README.md").exists(),
        "the worktree just used stays"
    );
}

/// A refresh queued after a pass planned the settled first refresh: the
/// pass checks again under the run table, keeps the worktree, and the new
/// refresh starts in it.
#[tokio::test]
async fn a_refresh_queued_after_a_pass_planned_keeps_the_worktree() {
    let h = harness(caches_due_at_once()).await;
    let (owner, first, tree) = Box::pin(owner_and_finished_refresh(&h)).await;
    let pass = gc::plan(&h.ctx, Some(Age::from_secs(0)), true)
        .await
        .expect("plan");
    assert!(pass.items() >= 1, "{pass:?}");
    let second = queue_refresh(&h, &owner).await;

    let report = gc::collect_planned(&h.ctx, pass).await.expect("pass");
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(kept_for(&report, &tree, &second), "{report:?}");
    assert!(
        tree.join("target/debug/app").exists(),
        "nothing was deleted"
    );
    assert!(!h.ctx.collecting.contains(&first.id) && !h.ctx.collecting.contains(&owner.id));
    h.dispatch().await;
    assert_eq!(h.run(&second.id).await.workspace, tree, "it starts in it");
}

/// A refresh queued again for the base commit an earlier one merged
/// replaces it in the store; one that ran and finished after a pass planned
/// the worktree's owner leaves that plan stale, so the worktree it just used
/// stays until it is as old as the retention asks.
#[tokio::test]
async fn a_refresh_that_ran_after_a_pass_planned_keeps_the_worktree_its_age() {
    let h = harness(caches_due_at_once()).await;
    let (owner, _, tree) = Box::pin(owner_and_finished_refresh(&h)).await;
    let pass = gc::plan(&h.ctx, Some(Age::from_secs(1)), true)
        .await
        .expect("plan");
    assert!(pass.items() >= 1, "{pass:?}");
    let second = queue_refresh(&h, &owner).await;
    h.dispatch().await;
    assert_eq!(h.run(&second.id).await.workspace, tree, "it ran there");
    cancel(&h, &second).await;

    let report = gc::collect_planned(&h.ctx, pass).await.expect("pass");
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(tree.join("README.md").exists(), "the worktree stays");
    let report = gc_request(&h, Some(3600), true).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(tree.join("README.md").exists(), "it was used just now");
}

/// While a pass collects any item's share of a worktree, it holds the
/// worktree's owner: a pass meets the owner held and leaves the settled
/// refresh alone, and dispatch holds a new refresh until the owner is free.
#[tokio::test]
async fn a_pass_holds_every_item_of_the_worktree_it_collects() {
    let h = harness(caches_due_at_once()).await;
    let (owner, first, tree) = Box::pin(owner_and_finished_refresh(&h)).await;
    let sweeping = h.ctx.collecting.mark(owner.id.clone()).expect("unmarked");

    let report = gc_request(&h, Some(0), true).await;
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(
        report
            .skipped
            .iter()
            .any(|(path, why)| path == &tree && why.contains("stopped gc pass")),
        "{report:?}"
    );
    assert!(
        tree.join("target/debug/app").exists(),
        "nothing was deleted"
    );
    assert!(
        !h.ctx.collecting.contains(&first.id),
        "the held pass let go"
    );

    let second = queue_refresh(&h, &owner).await;
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(h.state(&second.id).await, QueueState::Queued, "held back");
    drop(sweeping);
    h.dispatch().await;
    assert_eq!(
        h.run(&second.id).await.workspace,
        tree,
        "it starts once let go"
    );
}

/// A worktree deleted from under a settled item, by gc or by hand, is added
/// again from the item's kept branch when the item is retried.
#[tokio::test]
async fn a_retry_recreates_a_missing_worktree_from_the_item_s_branch() {
    let h = harness(caches_due_at_once()).await;
    let (item, tree) = cancelled_run(&h, "lost its worktree").await;
    let branch = h.run(&item.id).await.branch;
    std::fs::write(tree.join("work.md"), "kept on the branch\n").expect("work");
    git(&tree, &["add", "work.md"]);
    git(&tree, &["commit", "--quiet", "-m", "work"]);
    let head = git(&tree, &["rev-parse", "HEAD"]);
    std::fs::remove_dir_all(&tree).expect("removed by hand");

    retry(&h, &item).await;
    h.dispatch().await;
    let runs = runs_of(&h, &item).await;
    assert_eq!(runs.len(), 2, "the retry started");
    assert_eq!(runs[1].workspace, tree, "in the same place");
    assert_eq!(git(&tree, &["rev-parse", "HEAD"]), head, "from the branch");
    assert_eq!(git(&tree, &["branch", "--show-current"]), branch);
    assert!(tree.join("work.md").exists());
}

/// A fake Aether serving `storage_gc` that the harness's link is connected to.
async fn connected_aether(h: &Harness, features: &[&str]) -> FakeAether {
    let fake = FakeAether::start(&h.dir.path().join("aetherd.sock"))
        .await
        .expect("fake aetherd binds");
    fake.set_features(features.iter().map(|f| (*f).to_owned()).collect());
    let client = AetherClient::connect(&fake.endpoint())
        .await
        .expect("connects to the fake");
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(client)))
        .expect("installs the client");
    fake
}

/// A worktree of the harness repository outside Archon's worktree root, the
/// way Aether delegates a workspace to a child agent.
fn delegated_workspace(h: &Harness) -> PathBuf {
    let path = h.repo.parent().expect("repo parent").join("delegated");
    let path_text = path.to_str().expect("utf-8");
    git(&h.repo, &["worktree", "add", "--detach", path_text]);
    path
}

fn lists_worktree(h: &Harness, path: &Path) -> bool {
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    listed.contains(path.to_str().expect("utf-8"))
}

#[tokio::test]
async fn a_pass_that_removed_a_worktree_has_aether_delete_its_workspaces_then_prunes_git() {
    let h = harness(Config::default()).await;
    let fake = connected_aether(&h, &["storage"]).await;
    let (_, tree) = cancelled_run(&h, "removed by the pass").await;
    let delegated = delegated_workspace(&h);
    let aether_workspace = delegated.clone();
    fake.set_responder(
        "storage_gc",
        std::sync::Arc::new(move |_| {
            std::fs::remove_dir_all(&aether_workspace).expect("aether deletes its workspace");
            Scripted::reply("storage_report", serde_json::json!({"reclaimed": 1}))
        }),
    );
    assert!(lists_worktree(&h, &delegated));

    let report = gc_request(&h, Some(0), false).await;
    assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.aether_gc, Some(AetherGc::Ran));
    let asked = fake.received_named("storage_gc");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0]["dry_run"], false);
    assert!(asked[0].get("prune_build_output").is_none(), "{asked:?}");
    assert!(!delegated.exists());
    assert!(
        !lists_worktree(&h, &delegated),
        "git forgot the worktree Aether deleted"
    );
}

#[tokio::test]
async fn a_storage_gc_aether_refuses_is_reported_and_the_pass_still_succeeds() {
    let h = harness(Config::default()).await;
    let fake = connected_aether(&h, &["storage"]).await;
    fake.set_default(
        "storage_gc",
        Scripted::refuse("sweep_busy", "a sweep is running"),
    );
    let (_, tree) = cancelled_run(&h, "removed despite Aether").await;

    let report = gc_request(&h, Some(0), false).await;
    assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(!tree.exists());
    match report.aether_gc {
        Some(AetherGc::Failed(why)) => assert!(why.contains("sweep_busy"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.received_named("storage_gc").len(), 1);
}

#[tokio::test]
async fn an_unreachable_or_older_aether_is_reported_and_git_is_still_pruned() {
    for features in [None, Some(&[][..])] {
        let h = harness(Config::default()).await;
        let fake = match features {
            Some(features) => Some(connected_aether(&h, features).await),
            None => None,
        };
        let (_, tree) = cancelled_run(&h, "removed without Aether").await;
        let delegated = delegated_workspace(&h);
        std::fs::remove_dir_all(&delegated).expect("a deleted workspace");
        assert!(lists_worktree(&h, &delegated));

        let report = gc_request(&h, Some(0), false).await;
        assert_eq!(report.removed, vec![tree.clone()], "{report:?}");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(
            matches!(&report.aether_gc, Some(AetherGc::Failed(_))),
            "{:?}",
            report.aether_gc
        );
        assert!(!lists_worktree(&h, &delegated), "pruned without Aether");
        if let Some(fake) = fake {
            assert!(
                fake.received_named("storage_gc").is_empty(),
                "no storage feature"
            );
        }
    }
}

#[tokio::test]
async fn a_pass_that_removed_nothing_does_not_ask_aether_to_clean_up() {
    let h = harness(caches_due_at_once()).await;
    let fake = connected_aether(&h, &["storage"]).await;
    let (_, tree) = cancelled_run(&h, "only reclaimed").await;

    let report = gc_request(&h, None, false).await;
    assert_eq!(report.reclaimed.len(), 1, "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(tree.exists());
    assert_eq!(report.aether_gc, None);
    assert!(fake.received_named("storage_gc").is_empty());
}

#[tokio::test]
async fn offline_gc_prunes_git_but_leaves_aether_alone() {
    let h = harness(Config::default()).await;
    let (_, tree) = cancelled_run(&h, "removed offline").await;
    let delegated = delegated_workspace(&h);
    std::fs::remove_dir_all(&delegated).expect("a deleted workspace");
    let (config, paths) = (&h.ctx.config, &h.ctx.paths);

    let report =
        gc::collect_offline(config, paths, Some(Age::from_secs(0)), false).expect("offline gc");
    assert_eq!(report.removed, vec![tree], "{report:?}");
    assert_eq!(report.aether_gc, None);
    assert!(!lists_worktree(&h, &delegated), "pruned offline");
}
