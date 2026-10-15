//! The operator's drained upgrade and restart against a fake installer and
//! restart helper: the drain holds dispatch and waits for running stage
//! agents, gives up or restarts when its wait runs out, an install failure
//! holds nothing, and only the operator may ask.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::ops::{UpgradeRequest, UpgradeTarget};
use archon_core::protocol::{ClientKind, DaemonStatus, Reply, RepoAdd, Request, WaitingReasonCode};
use archon_daemon::handlers;
use archon_daemon::ops::{self, Program, RestartPlan};
use common::{Harness, completed};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);

fn restart(drain_secs: u64, requeue: bool) -> UpgradeRequest {
    UpgradeRequest {
        archon: None,
        aether: None,
        drain_secs,
        requeue,
        keep_aether: false,
    }
}

fn worker(h: &Harness) -> JoinHandle<Result<(), ArchonError>> {
    tokio::spawn(ops::run(h.ctx.clone()))
}

async fn start(h: &Harness, request: UpgradeRequest) -> String {
    match h.cli(Request::Upgrade(request)).await {
        Ok(Reply::UpgradeStarted { text }) => text,
        other => panic!("upgrade replied {other:?}"),
    }
}

async fn status(h: &Harness) -> DaemonStatus {
    match h.cli(Request::Status).await {
        Ok(Reply::Status(status)) => *status,
        other => panic!("status replied {other:?}"),
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn system_lines(h: &Harness) -> Vec<String> {
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    feed.into_iter()
        .filter_map(|item| match item.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

async fn handed_off(h: &Harness) {
    tokio::time::timeout(WAIT, h.ctx.ops.stopping())
        .await
        .expect("the restart was handed off");
}

#[tokio::test]
async fn a_restart_holds_dispatch_until_running_work_finishes() {
    let h = Harness::new().await;
    let first = h.enqueue("running").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "one stage agent runs");
    let worker = worker(&h);

    let text = start(&h, restart(60, false)).await;
    assert!(text.starts_with("Accepted a drained restart"), "{text}");
    eventually("the drain holds dispatch", || async {
        h.ctx.ops.draining()
    })
    .await;
    let waiting = h.enqueue("waits for the restart").await;
    h.dispatch().await;
    let now = status(&h).await;
    let held = now
        .waiting_items
        .iter()
        .find(|row| row.item.id == waiting.id)
        .and_then(|row| row.waiting_reason.as_ref())
        .expect("the new item waits");
    assert_eq!(held.code, WaitingReasonCode::Draining);
    assert!(
        now.upgrade
            .as_deref()
            .is_some_and(|u| u.contains("draining"))
    );
    assert!(h.ops.snapshot().restarts.is_empty(), "nothing restarts yet");

    h.settle_running(|_| completed("done")).await;
    handed_off(&h).await;
    let plan = RestartPlan {
        restart_aether: true,
    };
    assert_eq!(h.ops.snapshot().restarts, [plan], "dedicated: aetherd too");
    assert!(
        h.kv("upgrade_restart").await.is_some(),
        "recorded for the next archond"
    );
    assert!(h.ctx.ops.draining(), "the hold lasts until archond stops");
    assert_eq!(h.ops.snapshot().installs, [], "a restart installs nothing");
    assert_ne!(first.id, waiting.id);
    worker.abort();
}

#[tokio::test]
async fn a_drain_that_runs_out_gives_up_and_lets_dispatch_go_on() {
    let h = Harness::new().await;
    h.enqueue("still running").await;
    h.dispatch().await;
    let worker = worker(&h);

    start(&h, restart(1, false)).await;
    eventually("the upgrade gave up", || async {
        status(&h).await.upgrade.is_none()
    })
    .await;
    assert!(!h.ctx.ops.draining(), "dispatch is released");
    assert!(h.ops.snapshot().restarts.is_empty());
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes
            .iter()
            .any(|t| t.contains("gave up: 1 stage agents still ran after 1s")),
        "{outcomes:?}"
    );
    assert_eq!(h.kv("upgrade_restart").await, None);
    // The next one is accepted again.
    start(&h, restart(1, true)).await;
    handed_off(&h).await;
    let lines = system_lines(&h).await;
    assert!(
        lines
            .iter()
            .any(|t| t.contains("the 1 stage agents still running are stopped")),
        "{lines:?}"
    );
    worker.abort();
}

#[tokio::test]
async fn a_failed_install_holds_nothing_and_restarts_nothing() {
    let h = Harness::new().await;
    {
        let mut fake = h.ops.state.lock().unwrap();
        fake.binaries
            .insert(Program::Archon, PathBuf::from("/opt/archon/bin/archon"));
        fake.install_fails = Some("no release v9.9.9".into());
    }
    let worker = worker(&h);
    let request = UpgradeRequest {
        archon: Some(UpgradeTarget::Version("9.9.9".into())),
        ..restart(60, false)
    };
    start(&h, request).await;
    eventually("the upgrade stopped", || async {
        status(&h).await.upgrade.is_none()
    })
    .await;
    let fake = h.ops.snapshot();
    assert_eq!(fake.installs.len(), 1);
    assert_eq!(fake.installs[0].args, ["update", "--version", "v9.9.9"]);
    assert!(fake.restarts.is_empty());
    assert!(!h.ctx.ops.draining());
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes
            .iter()
            .any(|t| t.contains("no release v9.9.9") && t.contains("Nothing was restarted")),
        "{outcomes:?}"
    );
    worker.abort();
}

#[tokio::test]
async fn a_cargo_build_installs_the_latest_release_by_its_tag() {
    let h = Harness::new().await;
    {
        let mut fake = h.ops.state.lock().unwrap();
        let cargo_bin = PathBuf::from("/home/op/.cargo/bin");
        fake.binaries
            .insert(Program::Archon, cargo_bin.join("archon"));
        fake.cargo_bin = Some(cargo_bin);
        fake.latest.insert(Program::Archon, "1.2.3".into());
    }
    let worker = worker(&h);
    let request = UpgradeRequest {
        archon: Some(UpgradeTarget::Latest),
        ..restart(60, false)
    };
    start(&h, request).await;
    handed_off(&h).await;
    let fake = h.ops.snapshot();
    assert_eq!(fake.installs.len(), 1);
    assert_eq!(fake.installs[0].command, PathBuf::from("cargo"));
    assert!(
        fake.installs[0]
            .args
            .join(" ")
            .ends_with("--tag v1.2.3 archon"),
        "{:?}",
        fake.installs[0].args
    );
    worker.abort();
}

#[tokio::test]
async fn one_upgrade_runs_at_a_time_and_shared_mode_keeps_aether_out() {
    let h = Harness::new().await;
    h.enqueue("keeps the drain waiting").await;
    h.dispatch().await;
    let worker = worker(&h);
    start(&h, restart(60, false)).await;
    let second = h.cli(Request::Upgrade(restart(60, false))).await;
    assert!(
        matches!(&second, Err(ArchonError::Unavailable(m)) if m.contains("already under way")),
        "{second:?}"
    );
    worker.abort();

    let mut config = Config::default();
    config.aether.mode = AetherMode::Shared;
    let shared = Harness::with_config(config).await;
    let request = UpgradeRequest {
        aether: Some(UpgradeTarget::Latest),
        ..restart(60, false)
    };
    let refused = shared.cli(Request::Upgrade(request)).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(m)) if m.contains("aether update")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn only_the_operator_upgrades_checks_releases_or_watches_github() {
    let h = Harness::new().await;
    let watch = RepoAdd {
        path: h.repo.clone(),
        name: Some("watched".into()),
        ship: None,
        base_branch: None,
        default_pipeline: None,
        auto_merge: false,
        github_watch: true,
    };
    for request in [
        Request::Upgrade(restart(60, false)),
        Request::CheckUpdates,
        Request::RepoAdd(watch.clone()),
    ] {
        let over_mcp = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request.clone()).await;
        assert!(
            matches!(over_mcp, Err(ArchonError::Forbidden(_))),
            "{over_mcp:?}"
        );
        let for_agent = h.orchestrator(request).await;
        assert!(
            matches!(for_agent, Err(ArchonError::Forbidden(_))),
            "{for_agent:?}"
        );
    }
    assert!(h.ops.snapshot().restarts.is_empty() && h.ops.snapshot().latest_checks == 0);
    let Reply::Repos(repos) = h.cli(Request::RepoList).await.unwrap() else {
        panic!("repos")
    };
    assert!(
        repos.iter().all(|r| r.name != "watched"),
        "nothing was registered"
    );
}

#[tokio::test]
async fn a_watch_that_cannot_be_set_up_registers_nothing() {
    let h = Harness::new().await;
    // The test repository has no origin remote.
    let add = RepoAdd {
        path: h.repo.clone(),
        name: Some("unwatchable".into()),
        ship: None,
        base_branch: None,
        default_pipeline: None,
        auto_merge: false,
        github_watch: true,
    };
    let refused = h.cli(Request::RepoAdd(add.clone())).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(m)) if m.contains("no origin remote")),
        "{refused:?}"
    );
    common::git(
        &h.repo,
        &["remote", "add", "origin", "https://gitlab.com/o/r.git"],
    );
    let refused = h.cli(Request::RepoAdd(add.clone())).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(m)) if m.contains("not a github.com repository")),
        "{refused:?}"
    );

    let mut config = Config::default();
    config.aether.mode = AetherMode::Shared;
    let shared = Harness::with_config(config).await;
    let add = RepoAdd {
        path: shared.repo.clone(),
        ..add
    };
    let refused = shared.cli(Request::RepoAdd(add)).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(m)) if m.contains("needs Archon's own Aether")),
        "{refused:?}"
    );
    for h in [&h, &shared] {
        let Reply::Repos(repos) = h.cli(Request::RepoList).await.unwrap() else {
            panic!("repos")
        };
        assert!(repos.iter().all(|r| r.name != "unwatchable"));
    }
}

#[tokio::test]
async fn status_shows_versions_that_differ_and_check_updates_reads_the_latest() {
    let h = Harness::new().await;
    let running = env!("CARGO_PKG_VERSION");
    {
        let mut fake = h.ops.state.lock().unwrap();
        fake.installed.insert(Program::Archon, running.into());
        fake.latest.insert(Program::Archon, "99.0.0".into());
    }
    let reply = h.cli(Request::CheckUpdates).await.unwrap();
    let Reply::Versions(versions) = reply else {
        panic!("versions: {reply:?}")
    };
    assert_eq!(versions.archon.running.as_deref(), Some(running));
    assert_eq!(versions.archon.latest.as_deref(), Some("99.0.0"));
    assert_eq!(h.ops.snapshot().latest_checks, 2, "archon and aether");
    let lines = status(&h).await.versions.expect("versions").lines();
    assert_eq!(
        lines,
        [format!(
            "archon running {running}, installed {running}, latest 99.0.0"
        )]
    );
}

#[tokio::test]
async fn an_upgrade_installs_only_after_running_work_finishes() {
    let h = Harness::new().await;
    h.ops
        .state
        .lock()
        .unwrap()
        .binaries
        .insert(Program::Archon, PathBuf::from("/opt/archon/bin/archon"));
    h.enqueue("finishes with the old tools").await;
    h.dispatch().await;
    let worker = worker(&h);
    let request = UpgradeRequest {
        archon: Some(UpgradeTarget::Latest),
        ..restart(60, false)
    };
    start(&h, request).await;
    eventually("the drain holds dispatch", || async {
        h.ctx.ops.draining()
    })
    .await;
    assert!(
        h.ops.snapshot().installs.is_empty(),
        "old binary stays installed during the drain"
    );
    h.settle_running(|_| completed("done")).await;
    handed_off(&h).await;
    assert_eq!(h.ops.snapshot().installs.len(), 1);
    assert!(
        h.ctx.ops.draining(),
        "dispatch stays held through installation"
    );
    worker.abort();
}

#[tokio::test]
async fn an_expired_upgrade_drain_installs_nothing() {
    let h = Harness::new().await;
    h.ops
        .state
        .lock()
        .unwrap()
        .binaries
        .insert(Program::Archon, PathBuf::from("/opt/archon/bin/archon"));
    h.enqueue("still running").await;
    h.dispatch().await;
    let worker = worker(&h);
    start(
        &h,
        UpgradeRequest {
            archon: Some(UpgradeTarget::Latest),
            ..restart(1, false)
        },
    )
    .await;
    eventually("the upgrade gave up", || async {
        status(&h).await.upgrade.is_none()
    })
    .await;
    assert!(h.ops.snapshot().installs.is_empty());
    assert!(h.ops.snapshot().restarts.is_empty());
    assert!(!h.ctx.ops.draining());
    worker.abort();
}

#[tokio::test]
async fn update_checks_start_all_four_probes_concurrently() {
    let h = Harness::new().await;
    h.ops.state.lock().unwrap().probe_barrier =
        Some(std::sync::Arc::new(tokio::sync::Barrier::new(4)));
    let reply = tokio::time::timeout(Duration::from_secs(2), h.cli(Request::CheckUpdates))
        .await
        .expect("all four probes must start together")
        .unwrap();
    assert!(matches!(reply, Reply::Versions(_)));
    assert_eq!(h.ops.snapshot().latest_checks, 2);
}
