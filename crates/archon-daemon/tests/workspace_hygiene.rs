//! Workspace hygiene (ARCHITECTURE §13.5): the shared build cache agents are
//! pointed at, gc of idle caches and expired artifacts that never touches a
//! live repository, and the durable per-item artifact store.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::FeedEntry;
use archon_core::protocol::{ArtifactListing, EnqueueRequest, GcReport, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::StageRun;
use common::{Harness, PIPELINE, REPO_NAME, completed, git, plan, verdict};

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
    let h = Harness::new().await;
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
async fn the_shared_cache_can_be_turned_off_for_everyone_or_for_one_repository() {
    let mut config = Config::default();
    config.worktrees.shared_cache = false;
    let h = Harness::with_config(config).await;
    start(&h, "no shared cache").await;
    assert!(!instructions(&h).contains("CARGO_TARGET_DIR"));
    assert!(!cache_dir(&h).exists());

    let h = Harness::new().await;
    h.repo_config("[cache]\nshared = false\n");
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

#[tokio::test]
async fn gc_never_touches_the_cache_of_a_repository_with_a_live_run() {
    let h = Harness::new().await;
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
    let h = Harness::new().await;
    let (item, _) = start(&h, "finished").await;
    cancel(&h, &item).await;
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
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
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
    let h = Harness::new().await;
    let (item, _) = start(&h, "finished").await;
    cancel(&h, &item).await;
    age_cache(&h, Duration::from_secs(2 * 86_400));

    let report = gc(&h, None).await;
    assert!(report.caches.is_empty() && !report.pressure, "{report:?}");

    // The dispatch held for low disk runs the pass.
    h.set_free_bytes(1 << 30);
    *h.ctx.gc.lock().await = None;
    h.dispatch().await;
    assert!(!cache_dir(&h).exists());
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
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
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: vec![after.id.clone()],
        tags: Vec::new(),
        model_plan: plan(),
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
    config.artifacts.keep_after = Age::from_secs(0);
    let h = Harness::with_config(config).await;
    let (live, live_stage) = start(&h, "still running").await;
    let workspace = h.run(&live.id).await.workspace;
    write(&workspace, "notes.txt", "keep while running");
    publish(&h, &live_stage, "notes.txt", None).await.unwrap();

    let done = h.enqueue("settled").await;
    cancel(&h, &done).await;
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
