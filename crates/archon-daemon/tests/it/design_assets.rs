//! Design slots, browser teardown and design data collection, through the
//! daemon with a fake browser: nothing here starts Chromium.

use std::time::Duration;

use crate::common::design::{Fixture, add_worktree, design_config};
use archon_core::aether_ids::AetherAgentId;
use archon_core::protocol::{Request, ServerFrame, WelcomeDevice};
use archon_daemon::design::StageEnd;
use archon_remote::DesignSubscription;
use tokio::sync::mpsc;

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Another watched design session on the fixture's daemon.
async fn new_session(f: &Fixture, name: &str) -> archon_core::design::DesignSessionId {
    let item =
        crate::common::design::enqueue_live(&f.h, &format!("design the {name} button")).await;
    f.h.dispatch().await;
    let run = f.h.run(&item.id).await;
    let stage =
        f.h.running()
            .await
            .into_iter()
            .find(|s| s.run == run.id)
            .expect("the item's stage runs");
    let agent: AetherAgentId = stage.aether_agent.clone().expect("an agent");
    let worktree = add_worktree(&f.h.repo, f.h.dir.path(), name);
    let session =
        f.h.ctx
            .design
            .stage_started(
                &f.h.ctx,
                &item.id,
                &run.id,
                &stage.stage_id,
                stage.attempt,
                &agent,
                worktree,
            )
            .await
            .expect("session starts");
    f.as_agent(Request::DesignPreview {
        url: crate::common::design::PREVIEW.into(),
        caller: Some(agent),
    })
    .await
    .expect("preview reported");
    session
}

/// An operator watching `session`.
fn watch(
    f: &Fixture,
    session: &archon_core::design::DesignSessionId,
) -> (mpsc::Receiver<ServerFrame>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let sub = DesignSubscription {
        id: 7,
        request: 7,
        session: session.clone(),
        device: WelcomeDevice {
            id: "device-1".into(),
            name: "phone".into(),
            role: archon_core::protocol::DeviceRole::Operator,
        },
    };
    let task = tokio::spawn(archon_daemon::design::stream(f.h.ctx.clone(), sub, tx));
    (rx, task)
}

/// Two sessions starting together under one slot never both launch.
#[tokio::test]
async fn concurrent_starts_never_exceed_max_sessions() {
    let mut config = design_config();
    config.design.max_sessions = 1;
    let f = Fixture::with(config).await;
    f.end(StageEnd::Cancelled).await;
    *f.factory.delay.lock().unwrap() = Duration::from_millis(300);
    let before = f.factory.launched();
    let a = new_session(&f, "a").await;
    let b = new_session(&f, "b").await;
    let (_rx_a, task_a) = watch(&f, &a);
    let (_rx_b, task_b) = watch(&f, &b);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(f.factory.launched(), before + 1, "one slot, one browser");
    task_a.abort();
    task_b.abort();
}

/// A crash give-up frees the slot: a queued session starts.
#[tokio::test]
async fn a_crash_give_up_starts_a_queued_session() {
    let mut config = design_config();
    config.design.max_sessions = 1;
    let f = Fixture::with(config).await;
    let second = new_session(&f, "queued").await;
    let (_rx, task) = watch(&f, &second);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.factory.launched(), 1, "queued behind the first");
    // More crashes than the budget allows: the first session gives up.
    for n in 0..6 {
        until("the browser to crash", || f.factory.launched() > n).await;
        f.factory.browser(n).crash();
        if n < 5 {
            until("the relaunch", || f.factory.launched() > n + 1).await;
        }
    }
    let factory = f.factory.clone();
    until("the queued session's browser", || factory.launched() == 7).await;
    task.abort();
}

fn upload(f: &Fixture, bytes: Vec<u8>) -> Request {
    use base64::Engine;
    Request::DesignUpload {
        session: f.session.clone(),
        media_type: "image/jpeg".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        name: None,
        sketch: None,
        base: None,
        blank: false,
    }
}

fn picture_files(f: &Fixture) -> Vec<std::path::PathBuf> {
    let dir =
        f.h.dir
            .path()
            .canonicalize()
            .unwrap()
            .join("data/design/assets")
            .join(f.item.to_string());
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// The asset sweep deletes a picture no note uses once it is old enough, file
/// and record, and leaves a young one.
#[tokio::test]
async fn the_asset_sweep_deletes_old_unused_pictures_only() {
    let f = Fixture::new().await;
    f.operator(upload(&f, crate::common::design::jpeg(32, 32)))
        .await
        .expect("uploaded");
    assert_eq!(picture_files(&f).len(), 1);
    let design = &f.h.ctx.design;
    let young = archon_core::now() - chrono::Duration::hours(1);
    assert_eq!(
        design.sweep_assets_before(&f.h.ctx, young).await.unwrap(),
        0
    );
    assert_eq!(picture_files(&f).len(), 1, "a young picture stays");
    let later = archon_core::now() + chrono::Duration::minutes(1);
    assert_eq!(
        design.sweep_assets_before(&f.h.ctx, later).await.unwrap(),
        1
    );
    assert!(
        picture_files(&f).is_empty(),
        "the file went with its record"
    );
    assert_eq!(
        design.sweep_assets_before(&f.h.ctx, later).await.unwrap(),
        0
    );
}

/// gc collects an ended item's design data even when `[design] enabled` is
/// off now.
#[tokio::test]
async fn gc_collects_design_data_with_design_turned_off() {
    let mut f = Fixture::new().await;
    f.operator(upload(&f, crate::common::design::jpeg(32, 32)))
        .await
        .expect("uploaded");
    assert_eq!(picture_files(&f).len(), 1);
    f.h.cli(Request::CancelItem {
        item: f.item.clone(),
        reason: None,
    })
    .await
    .expect("cancel");
    f.h.dispatch().await;
    {
        let mut store = f.h.ctx.store.lock().await;
        let mut settled = store.item(&f.item).unwrap();
        settled.updated_at -= chrono::Duration::seconds(2);
        store.update_item(&settled).unwrap();
        for mut run in store.runs_for_item(&f.item).unwrap() {
            if let Some(at) = &mut run.finished_at {
                *at -= chrono::Duration::seconds(2);
                store.update_run(&run).unwrap();
            }
        }
        assert!(!store.design_sessions(&f.item).unwrap().is_empty());
    }
    let mut config = f.h.ctx.config.clone();
    config.design.enabled = false;
    f.h.restart_with(config);
    assert!(!f.h.ctx.design.enabled());
    let pass = archon_daemon::gc::plan(&f.h.ctx, Some(archon_core::age::Age::from_secs(0)), true)
        .await
        .expect("plan");
    archon_daemon::gc::collect_planned(&f.h.ctx, pass)
        .await
        .expect("pass");
    let store = f.h.ctx.store.lock().await;
    assert!(store.design_sessions(&f.item).unwrap().is_empty());
    drop(store);
    assert!(picture_files(&f).is_empty(), "the pictures were collected");
}

/// Ending the session closes its browser and removes its profile, and a look
/// at the ended session starts no new browser.
#[tokio::test]
async fn an_ended_session_keeps_no_browser_and_starts_none() {
    let f = Fixture::new().await;
    let profile = f.factory.launches.lock().unwrap()[0].profile_dir.clone();
    f.end(StageEnd::Cancelled).await;
    assert!(
        f.factory.browser(0).calls(|c| c.closed),
        "the browser closed"
    );
    assert!(!profile.exists(), "the profile is gone");
    let _ = f
        .operator(Request::DesignReload {
            session: f.session.clone(),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.factory.launched(), 1, "nothing relaunched it");
}
