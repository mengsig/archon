//! Workspace hygiene (ARCHITECTURE §13.5): the shared build cache agents are
//! pointed at, gc of idle caches and expired artifacts that never touches a
//! live repository, and the durable per-item artifact store.

use crate::common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{Effort, ModelPlan};
use archon_core::notify::FeedEntry;
use archon_core::ports::DescendantState;
use archon_core::protocol::{ArtifactListing, EnqueueRequest, GcReport, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use common::{Harness, MODEL, PIPELINE, REPO_NAME, choice, completed, git, plan, verdict};

fn cache_dir(h: &Harness) -> PathBuf {
    h.ctx.paths.cache().join(REPO_NAME)
}

fn artifacts_dir(h: &Harness, item: &QueueItemId) -> PathBuf {
    h.ctx.paths.artifacts().join(item.as_str())
}

/// The instructions the item's first agent was started with.
fn instructions(h: &Harness) -> String {
    h.backend.requests()[0].instructions.clone()
}

async fn start(h: &Harness, title: &str) -> (QueueItem, StageRun) {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    (item, stage)
}

/// A harness whose repositories share one cargo target directory.
async fn shared() -> Harness {
    let mut config = Config::default();
    config.worktrees.shared_cache = true;
    Harness::with_config(config).await
}

async fn cancel(h: &Harness, item: &QueueItem) {
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

async fn gc(h: &Harness, older_than_secs: Option<u64>) -> GcReport {
    let request = Request::Gc {
        older_than_secs,
        force: false,
        preview: false,
    };
    match h.cli(request).await {
        Ok(Reply::Gc(report)) => *report,
        other => panic!("gc: {other:?}"),
    }
}

/// Make the cache look last used `ago` ago.
fn age_cache(h: &Harness, ago: Duration) {
    let marker = cache_dir(h).join(".used");
    let file = std::fs::File::options().write(true).open(&marker).unwrap();
    file.set_modified(SystemTime::now() - ago).unwrap();
}

async fn publish(
    h: &Harness,
    stage: &StageRun,
    path: &str,
    name: Option<&str>,
) -> Result<ArtifactListing, ArchonError> {
    let request = Request::PublishArtifact {
        path: path.into(),
        name: name.map(str::to_owned),
        caller: stage.aether_agent.clone(),
    };
    match h.cli(request).await? {
        Reply::Artifacts(listing) => Ok(*listing),
        other => panic!("publish: {other:?}"),
    }
}

fn write(workspace: &Path, name: &str, text: &str) {
    let path = workspace.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn every_stage_agent_is_pointed_at_one_shared_cache_per_repository() {
    let h = shared().await;
    let (item, stage) = start(&h, "builds").await;
    let target = cache_dir(&h).join("cargo-target");
    let told = instructions(&h);
    assert!(
        told.contains(&format!("CARGO_TARGET_DIR={}", target.display())),
        "{told}"
    );
    assert!(target.is_dir() && cache_dir(&h).join(".used").is_file());
    assert!(!target.starts_with(h.run(&item.id).await.workspace));

    let config = h
        .ctx
        .paths
        .worktrees
        .join(REPO_NAME)
        .join(".cargo/config.toml");
    let text = std::fs::read_to_string(config).unwrap();
    assert!(text.contains(&format!("target-dir = {:?}", target.to_str().unwrap())));
    assert_eq!(stage.stage_id, "implement");

    let second = h.enqueue("builds too").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "{second:?} started beside it");
    let all = h.backend.requests();
    assert!(
        all.iter()
            .all(|r| r.instructions.contains(&target.display().to_string())),
        "every agent of the repository shares the one directory"
    );
}

#[tokio::test]
async fn the_build_cache_can_be_turned_off_for_everyone_or_for_one_repository() {
    let mut config = Config::default();
    config.worktrees.shared_cache = false;
    config.worktrees.item_cache = false;
    config.worktrees.lean_builds = false;
    let h = Harness::with_config(config).await;
    start(&h, "no shared cache").await;
    assert!(!instructions(&h).contains("CARGO_TARGET_DIR"));
    assert!(!cache_dir(&h).exists());

    let h = shared().await;
    h.repo_config("[cache]\nshared = false\nitem = false\nlean = false\n");
    start(&h, "repository opted out").await;
    assert!(!instructions(&h).contains("CARGO_TARGET_DIR"));
    assert!(!cache_dir(&h).exists());
    let config = h
        .ctx
        .paths
        .worktrees
        .join(REPO_NAME)
        .join(".cargo/config.toml");
    assert!(!config.exists());
}

fn item_dir(h: &Harness, item: &QueueItemId) -> PathBuf {
    cache_dir(h).join("items").join(item.as_str())
}

/// Leave `dir` the way a test suite can: a subdirectory nobody can enter.
fn lock_a_subdirectory(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let locked = dir.join("debug/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("f"), vec![1u8; 100_000]).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
}

async fn system_notes(h: &Harness) -> Vec<String> {
    h.ctx
        .store
        .lock()
        .await
        .feed_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|f| match f.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn each_item_builds_lean_in_its_own_directory_that_goes_when_it_ends() {
    let h = Harness::new().await;
    let (first, stage) = start(&h, "builds").await;
    let target = item_dir(&h, &first.id).join("cargo-target");
    assert!(
        instructions(&h).contains(&format!("CARGO_TARGET_DIR={}", target.display())),
        "{}",
        instructions(&h)
    );
    assert!(target.is_dir());
    let config = h
        .ctx
        .paths
        .worktrees
        .join(REPO_NAME)
        .join(".cargo/config.toml");
    let text = std::fs::read_to_string(config).unwrap();
    assert!(text.contains("incremental = false"), "{text}");
    assert!(text.contains("line-tables-only"), "{text}");
    assert!(!text.contains("target-dir"), "{text}");

    let second = h.enqueue("builds too").await;
    h.dispatch().await;
    let other = item_dir(&h, &second.id);
    assert!(other.is_dir() && other != item_dir(&h, &first.id));

    // Kept while the item lives: the next stage builds on it.
    std::fs::write(target.join("warm.o"), b"object").unwrap();
    let settled = h
        .settle_running(|_| completed("implemented"))
        .await
        .into_iter()
        .any(|s| s.run == stage.run);
    assert!(settled);
    assert!(target.join("warm.o").exists());

    lock_a_subdirectory(&target);
    cancel(&h, &first).await;
    // The run node's retirement ends the kept agents, then the release runs.
    h.dispatch().await;
    assert!(!item_dir(&h, &first.id).exists(), "deleted at its end");
    assert!(other.is_dir(), "the other item's build is untouched");
    let notes = system_notes(&h).await;
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Freed") && n.contains(first.id.as_str())),
        "{notes:?}"
    );
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
        panic!("status");
    };
    let last = status.disk.unwrap().gc.expect("the release is shown");
    assert!(last.bytes_freed >= 100_000, "{last:?}");
}

#[tokio::test]
async fn gc_sweeps_item_build_directories_whose_item_is_final_or_gone() {
    let h = Harness::new().await;
    let (live, _) = start(&h, "still running").await;
    let (gone, ended) = (QueueItemId::new(), h.enqueue("ended").await);
    cancel(&h, &ended).await;
    for orphan in [item_dir(&h, &gone), item_dir(&h, &ended.id)] {
        std::fs::create_dir_all(orphan.join("cargo-target")).unwrap();
        lock_a_subdirectory(&orphan);
    }
    let stray = cache_dir(&h).join("items/not-an-item");
    std::fs::create_dir_all(&stray).unwrap();

    let report = gc(&h, None).await;
    assert_eq!(report.item_caches.len(), 2, "{report:?}");
    assert!(report.bytes_freed >= 200_000, "{report:?}");
    assert!(!item_dir(&h, &gone).exists() && !item_dir(&h, &ended.id).exists());
    assert!(item_dir(&h, &live.id).is_dir(), "a live item's stays");
    assert!(stray.is_dir(), "what is not an item's is the operator's");
    let notes = system_notes(&h).await;
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Gc freed") && n.contains("2 item(s)")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_build_directory_stays_while_a_cancelled_item_s_agent_may_still_run() {
    let h = Harness::new().await;
    let (item, _) = start(&h, "unreachable agent").await;
    h.backend
        .fail_next_cancel(ArchonError::Unavailable("aether is away".into()));
    cancel(&h, &item).await;
    let dir = item_dir(&h, &item.id);
    assert!(dir.is_dir(), "its agent was not confirmed stopped");
    let report = gc(&h, None).await;
    assert!(report.item_caches.is_empty(), "{report:?}");
    assert!(dir.is_dir());

    age_cache(&h, Duration::from_secs(3600));
    let report = gc(&h, Some(60)).await;
    assert!(report.caches.is_empty(), "{report:?}");
    assert!(
        dir.is_dir(),
        "an idle repository cache keeps a live agent's"
    );
}

/// What a failed lead delegated may still build: its item's directory waits
/// for Aether to confirm the stop, then goes.
#[tokio::test]
async fn a_build_directory_stays_until_what_a_failed_lead_delegated_has_ended() {
    let h = Harness::new().await;
    let build = ("build".to_owned(), vec![choice(MODEL, Effort::Medium)]);
    let item = h
        .enqueue_in("adaptive", "a led change", ModelPlan([build].into()))
        .await
        .unwrap();
    h.dispatch().await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    let helper = h.backend.delegate(&lead, MODEL);
    h.settle_running(|_| StageOutcome::Failed {
        error: "the build broke".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let dir = item_dir(&h, &item.id);
    assert!(dir.is_dir(), "its helper still runs");
    let report = gc(&h, None).await;
    assert!(report.item_caches.is_empty(), "{report:?}");
    assert!(dir.is_dir());

    h.backend.end_delegated(&helper, DescendantState::Stopped);
    h.dispatch().await;
    h.dispatch().await;
    assert!(!dir.exists(), "released once the stop is confirmed");
}

/// A cancel Aether accepts is cooperative: the agent may build on for a
/// while, so neither the item's end nor gc deletes its directory until Aether
/// says the agent ended.
#[tokio::test]
async fn a_build_directory_stays_until_aether_confirms_a_cancelled_agent_ended() {
    let h = Harness::new().await;
    let (item, stage) = start(&h, "slow to stop").await;
    let agent = stage.aether_agent.unwrap();
    h.backend.cancel_slowly(&agent);
    cancel(&h, &item).await;
    h.dispatch().await;
    let dir = item_dir(&h, &item.id);
    assert!(dir.is_dir(), "its agent still runs");
    let report = gc(&h, None).await;
    assert!(report.item_caches.is_empty(), "{report:?}");
    assert!(dir.is_dir());

    h.backend.kill(&agent);
    h.dispatch().await;
    assert!(!dir.exists(), "released once Aether says it ended");
}

#[tokio::test]
async fn a_build_directory_stays_past_the_grace_until_a_slow_node_and_its_builders_end() {
    let h = Harness::new().await;
    let (item, stage) = start(&h, "slow subtree").await;
    let agent = stage.aether_agent.unwrap();
    let node = h.backend.node_ids().remove(0);
    h.backend.cancel_slowly(&agent);
    h.backend.cancel_slowly(&node);
    cancel(&h, &item).await;
    common::age_settlements(&h, &stage.run, 2).await;
    h.dispatch().await;
    let dir = item_dir(&h, &item.id);
    assert!(dir.is_dir(), "grace expiry does not prove termination");
    assert!(gc(&h, Some(0)).await.caches.is_empty());
    assert!(dir.is_dir());

    h.backend.kill(&node);
    h.dispatch().await;
    assert!(
        dir.is_dir(),
        "the builder still runs after its parent ended"
    );
    h.backend.kill(&agent);
    h.dispatch().await;
    assert!(!dir.exists(), "released after the entire subtree stopped");
}

/// A symlinked `items` directory leads outside the cache: gc and an item's
/// end leave both it and what it points at alone.
#[tokio::test]
async fn item_build_directories_behind_a_symlink_are_never_deleted() {
    let h = Harness::new().await;
    let (_live, _) = start(&h, "keeps the repository live").await;
    let outside = h.ctx.paths.data_dir.join("outside");
    let gone = QueueItemId::new();
    std::fs::create_dir_all(outside.join(gone.as_str())).unwrap();
    std::fs::write(outside.join(gone.as_str()).join("valuable.txt"), b"keep").unwrap();
    let items = cache_dir(&h).join("items");
    std::fs::rename(&items, h.ctx.paths.data_dir.join("real-items")).unwrap();
    std::os::unix::fs::symlink(&outside, &items).unwrap();

    let report = gc(&h, None).await;
    assert!(report.item_caches.is_empty(), "{report:?}");
    assert!(
        report.failed.iter().any(|(path, _)| path == &items),
        "{report:?}"
    );
    assert!(outside.join(gone.as_str()).join("valuable.txt").exists());

    let ended = h.enqueue("ends behind the symlink").await;
    std::fs::create_dir_all(outside.join(ended.id.as_str())).unwrap();
    std::fs::write(
        outside.join(ended.id.as_str()).join("valuable.txt"),
        b"keep",
    )
    .unwrap();
    cancel(&h, &ended).await;
    h.dispatch().await;
    assert!(
        outside
            .join(ended.id.as_str())
            .join("valuable.txt")
            .exists()
    );
}

#[tokio::test]
async fn a_repository_can_keep_the_default_profile_by_turning_lean_off() {
    let h = Harness::new().await;
    h.repo_config("[cache]\nlean = false\n");
    let (item, _) = start(&h, "full debuginfo").await;
    assert!(item_dir(&h, &item.id).is_dir(), "still per item");
    let config = h
        .ctx
        .paths
        .worktrees
        .join(REPO_NAME)
        .join(".cargo/config.toml");
    assert!(!config.exists());
}

#[tokio::test]
async fn gc_never_touches_the_cache_of_a_repository_with_a_live_run() {
    let h = shared().await;
    let (_item, _stage) = start(&h, "still running").await;
    age_cache(&h, Duration::from_secs(90 * 86_400));
    let target = cache_dir(&h).join("cargo-target");
    std::fs::write(target.join("artifact.o"), b"object").unwrap();

    let report = gc(&h, Some(0)).await;
    assert!(report.caches.is_empty(), "{report:?}");
    assert!(
        report
            .skipped
            .iter()
            .any(|(path, why)| { path == &cache_dir(&h) && why.contains("live") }),
        "{report:?}"
    );
    assert!(target.join("artifact.o").exists());
}

#[tokio::test]
async fn gc_deletes_an_idle_cache_and_says_what_it_freed() {
    let h = shared().await;
    let (item, _) = start(&h, "finished").await;
    cancel(&h, &item).await;
    h.dispatch().await;
    let target = cache_dir(&h).join("cargo-target");
    std::fs::write(target.join("artifact.o"), vec![1u8; 100_000]).unwrap();

    age_cache(&h, Duration::from_secs(3600));
    let report = gc(&h, None).await;
    assert!(report.caches.is_empty(), "an hour is not idle for 14 days");
    assert!(target.exists());

    let report = gc(&h, Some(60)).await;
    assert_eq!(report.caches.len(), 1, "{report:?}");
    assert_eq!(report.caches[0].path, cache_dir(&h));
    assert!(report.caches[0].bytes >= 100_000);
    assert!(!cache_dir(&h).exists());

    let notes: Vec<String> = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|f| match f.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Gc freed") && n.contains("1 shared build cache")),
        "{notes:?}"
    );
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
        panic!("status");
    };
    let last = status.disk.unwrap().gc.expect("the last pass is shown");
    assert!(last.bytes_freed >= 100_000 && !last.pressure, "{last:?}");

    let next = h.enqueue("builds again").await;
    h.dispatch().await;
    assert!(
        cache_dir(&h).join("cargo-target").is_dir(),
        "{next:?} rebuilt it"
    );
}

#[tokio::test]
async fn disk_pressure_shortens_how_long_an_idle_cache_is_kept() {
    let h = shared().await;
    let (item, _) = start(&h, "finished").await;
    cancel(&h, &item).await;
    h.dispatch().await;
    age_cache(&h, Duration::from_secs(2 * 86_400));

    let report = gc(&h, None).await;
    assert!(report.caches.is_empty() && !report.pressure, "{report:?}");

    // The dispatch held for low disk runs the pass.
    h.set_free_bytes(1 << 30);
    *h.ctx.gc.lock().await = None;
    h.dispatch().await;
    assert!(!cache_dir(&h).exists());
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
        panic!("status");
    };
    let last = status.disk.unwrap().gc.expect("the pass is shown");
    assert!(last.pressure, "{last:?}");
}

#[tokio::test]
async fn a_cache_a_pass_set_aside_but_did_not_delete_is_deleted_by_the_next() {
    let h = Harness::new().await;
    let left = h.ctx.paths.cache().join(".gc-1-demo-0");
    std::fs::create_dir_all(left.join("cargo-target")).unwrap();
    std::fs::write(left.join("cargo-target/x"), b"x").unwrap();
    let report = gc(&h, None).await;
    assert_eq!(report.caches.len(), 1, "{report:?}");
    assert!(!left.exists());
}

#[tokio::test]
async fn a_stage_publishes_files_that_outlive_its_worktree_and_reach_dependents() {
    let h = Harness::new().await;
    let first = h.enqueue("produces evidence").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let workspace = h.run(&first.id).await.workspace;
    write(&workspace, "bench/result.json", "{\"ms\": 41}\n");
    write(&workspace, "logs/a.log", "line a\n");

    let published = publish(&h, &stage, "bench/result.json", None)
        .await
        .unwrap();
    assert_eq!(published.published[0].name, "result.json");
    assert_eq!(published.dir, artifacts_dir(&h, &first.id));
    publish(&h, &stage, "logs", Some("evidence/logs"))
        .await
        .unwrap();

    for _ in 0..3 {
        h.settle_running(|s| match s.stage_id.as_str() {
            "review" => verdict(true),
            _ => completed("done"),
        })
        .await;
        h.dispatch().await;
    }
    assert!(matches!(
        h.state(&first.id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(
        !item_dir(&h, &first.id).exists(),
        "success releases build output"
    );
    // Unpublished files keep archond from removing the worktree itself.
    git(
        &h.repo,
        &["worktree", "remove", "--force", workspace.to_str().unwrap()],
    );
    assert!(!workspace.exists(), "the worktree is gone");

    let Ok(Reply::Artifacts(listing)) = h
        .cli(Request::ListArtifacts {
            item: first.id.clone(),
        })
        .await
    else {
        panic!("list");
    };
    let names: Vec<_> = listing.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["evidence/logs/a.log", "result.json"]);
    let Ok(Reply::ArtifactPage(page)) = h
        .cli(Request::ReadArtifact {
            item: first.id.clone(),
            name: "result.json".into(),
            offset: 0,
            limit: None,
        })
        .await
    else {
        panic!("read");
    };
    assert_eq!(page.text, "{\"ms\": 41}\n");

    let second = depends_on(&h, "uses the evidence", &first).await;
    h.dispatch().await;
    let told = h
        .backend
        .requests()
        .into_iter()
        .find(|r| r.labels["archon.item"] == second.id.as_str())
        .expect("the dependent started")
        .instructions;
    assert!(
        told.contains(&format!("\"artifacts\":{:?}", artifacts_dir(&h, &first.id))),
        "{told}"
    );
    assert!(told.contains("publish_artifact"), "{told}");
}

async fn depends_on(h: &Harness, title: &str, after: &QueueItem) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: vec![after.id.clone()],
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

#[tokio::test]
async fn only_a_running_stage_agent_may_publish_and_only_from_its_workspace() {
    let h = Harness::new().await;
    let (item, stage) = start(&h, "publisher").await;
    let workspace = h.run(&item.id).await.workspace;
    write(&workspace, "ok.txt", "ok");

    let anonymous = Request::PublishArtifact {
        path: "ok.txt".into(),
        name: None,
        caller: None,
    };
    assert!(matches!(
        h.cli(anonymous).await,
        Err(ArchonError::Forbidden(_))
    ));
    let stranger = Request::PublishArtifact {
        path: "ok.txt".into(),
        name: None,
        caller: Some(archon_core::aether_ids::AetherAgentId::parse("agt_stranger").unwrap()),
    };
    assert!(matches!(
        h.cli(stranger).await,
        Err(ArchonError::Forbidden(_))
    ));
    let outside = publish(&h, &stage, "../../../etc/hostname", None).await;
    assert!(
        matches!(
            outside,
            Err(ArchonError::Forbidden(_) | ArchonError::Invalid(_))
        ),
        "{outside:?}"
    );
    assert!(!artifacts_dir(&h, &item.id).exists());

    publish(&h, &stage, "ok.txt", None).await.unwrap();
    cancel(&h, &item).await;
    let late = publish(&h, &stage, "ok.txt", None).await;
    assert!(matches!(late, Err(ArchonError::Forbidden(_))), "{late:?}");
}

#[tokio::test]
async fn artifacts_obey_the_configured_size_limits() {
    let mut config = Config::default();
    config.artifacts.max_file_mb = 1;
    config.artifacts.max_item_mb = 1;
    let h = Harness::with_config(config).await;
    let (item, stage) = start(&h, "too much").await;
    let workspace = h.run(&item.id).await.workspace;
    std::fs::write(workspace.join("huge.bin"), vec![0u8; (1 << 20) + 1]).unwrap();
    std::fs::write(workspace.join("a.bin"), vec![0u8; 700_000]).unwrap();
    std::fs::write(workspace.join("b.bin"), vec![0u8; 700_000]).unwrap();

    let huge = publish(&h, &stage, "huge.bin", None).await.unwrap_err();
    assert!(huge.to_string().contains("max_file_mb"), "{huge}");
    publish(&h, &stage, "a.bin", None).await.unwrap();
    let full = publish(&h, &stage, "b.bin", None).await.unwrap_err();
    assert!(full.to_string().contains("max_item_mb"), "{full}");
}

#[tokio::test]
async fn gc_deletes_artifacts_only_of_settled_items_past_keep_after() {
    let mut config = Config::default();
    config.artifacts.keep_after = Age::from_secs(1);
    let h = Harness::with_config(config).await;
    let (live, live_stage) = start(&h, "still running").await;
    let workspace = h.run(&live.id).await.workspace;
    write(&workspace, "notes.txt", "keep while running");
    publish(&h, &live_stage, "notes.txt", None).await.unwrap();

    let done = h.enqueue("settled").await;
    cancel(&h, &done).await;
    {
        let mut store = h.ctx.store.lock().await;
        let mut item = store.item(&done.id).unwrap();
        item.updated_at -= chrono::Duration::seconds(2);
        store.update_item(&item).unwrap();
    }
    let kept = h.ctx.paths.artifacts();
    std::fs::create_dir_all(kept.join(done.id.as_str())).unwrap();
    std::fs::write(kept.join(done.id.as_str()).join("old.txt"), b"old").unwrap();
    std::fs::create_dir_all(kept.join("not-an-item")).unwrap();
    let stranger = QueueItemId::new();
    std::fs::create_dir_all(kept.join(stranger.as_str())).unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;
    let report = gc(&h, None).await;
    assert_eq!(report.artifacts.len(), 1, "{report:?}");
    assert_eq!(report.artifacts[0].path, kept.join(done.id.as_str()));
    assert!(artifacts_dir(&h, &live.id).join("notes.txt").exists());
    assert!(kept.join("not-an-item").exists(), "not Archon's to judge");
    assert!(
        kept.join(stranger.as_str()).exists(),
        "an unknown item is left"
    );
}
