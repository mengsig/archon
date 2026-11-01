//! The socket while start-up recovery runs: `status` answers at once with
//! the progress, every change is refused as `recovering` with a retry hint,
//! and nothing is dispatched until recovery is done.

use crate::common;

use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, PauseTarget, RecoveryProgress, Reply, Request};
use archon_core::queue::{Caps, QueueState};
use archon_daemon::{executor, server};
use common::Harness;

/// Live runs in the fixture store, all recovered on restart.
const RUNS: usize = 40;
/// Queued items besides them, which every `status` reads.
const QUEUED: usize = 200;
/// How long a `status` during recovery may take before the test fails: far
/// above the 1 s the acceptance criterion allows, so host load cannot fail
/// it. A `status` that waited on recovery still fails it: while the test
/// holds the run table, recovery cannot finish, so that `status` never
/// answers.
const STATUS_BOUND: Duration = Duration::from_secs(30);

/// A store with [`RUNS`] live runs and [`QUEUED`] items waiting behind them.
async fn large_store() -> Harness {
    let h = Harness::new().await;
    let caps = Caps {
        max_parallel_agents: u32::try_from(RUNS).unwrap(),
        ..Caps::default()
    };
    h.cli(Request::SetCaps { caps }).await.expect("caps");
    for n in 0..RUNS {
        h.enqueue(&format!("live {n}")).await;
    }
    h.dispatch().await;
    assert_eq!(h.running().await.len(), RUNS, "every run has a live stage");
    for n in 0..QUEUED {
        h.enqueue(&format!("waiting {n}")).await;
    }
    h
}

async fn recovery_of(client: &ArchonClient) -> Option<RecoveryProgress> {
    let asked = client.request(Request::Status(Default::default()));
    let answer = tokio::time::timeout(STATUS_BOUND, asked)
        .await
        .expect("status answers during recovery, without waiting for it");
    match answer.expect("status is served during recovery") {
        Reply::Status(status) => status.recovery,
        other => panic!("status replied {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_answers_with_progress_and_changes_wait_while_recovery_runs() {
    let mut h = large_store().await;
    let waiting = h.enqueue("dispatched only after recovery").await;
    h.restart();
    h.ctx.recovery.begin();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let client = ArchonClient::connect(&h.ctx.paths.socket, ClientKind::ArchonCli)
        .await
        .expect("hello is answered during recovery");

    // Recovery waits for the run table, held here: it is under way, unfinished.
    let table = h.ctx.runs.lock().await;
    let recovering = tokio::spawn({
        let ctx = h.ctx.clone();
        async move { executor::recover(&ctx).await }
    });
    let counting = recovery_of(&client).await.expect("recovery is shown");
    assert_eq!(counting.describe(), "recovering, counting runs");

    let pause = Request::Pause(PauseTarget::Global { hard: false });
    let refused = client.request(pause.clone()).await.expect_err("refused");
    let ArchonError::Recovering(why) = &refused else {
        panic!("a change during recovery is refused as recovering: {refused:?}");
    };
    assert!(
        why.contains("refuses pause") && why.contains("retry"),
        "{why}"
    );
    let printed = format!("archon: {refused:#}");
    assert!(
        printed.starts_with("archon: recovering: archond is recovering"),
        "{printed}"
    );
    drop(table);

    let mut seen = Vec::new();
    while let Some(progress) = recovery_of(&client).await {
        seen.push(progress);
    }
    recovering.await.expect("recovery task").expect("recovered");
    assert!(
        seen.windows(2).all(|w| w[0].runs_done <= w[1].runs_done),
        "progress only grows: {seen:?}"
    );
    let total = u32::try_from(RUNS).unwrap();
    assert!(
        seen.iter().all(|p| p.runs_total.is_none_or(|t| t == total)),
        "every live run is counted: {seen:?}"
    );
    assert_eq!(h.state(&waiting.id).await, QueueState::Queued);
    client
        .request(pause)
        .await
        .expect("changes are served once recovered");
    serving.abort();
}

#[tokio::test]
async fn nothing_is_dispatched_until_recovery_is_done() {
    let mut h = Harness::new().await;
    let waiting = h.enqueue("held by recovery").await;
    h.restart();
    h.ctx.recovery.begin();

    h.dispatch().await;
    assert_eq!(h.state(&waiting.id).await, QueueState::Queued, "held");
    assert_eq!(h.backend.created(), 0, "no agent spawned during recovery");

    executor::recover(&h.ctx).await.expect("recover");
    assert_eq!(h.ctx.recovery.progress(), None, "recovery is done");
    h.dispatch().await;
    assert!(
        matches!(h.state(&waiting.id).await, QueueState::Running { .. }),
        "dispatched once recovered"
    );
    assert_eq!(h.backend.created(), 1);
}

#[tokio::test]
async fn reads_are_served_and_changes_refused_during_recovery() {
    let h = Harness::new().await;
    let item = h.enqueue("read during recovery").await;
    h.ctx.recovery.begin();
    h.ctx.recovery.count(120);
    for _ in 0..40 {
        h.ctx.recovery.advance();
    }

    let read = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await;
    assert!(matches!(read, Ok(Reply::ItemDetail(_))), "{read:?}");
    let status = match h.cli(Request::Status(Default::default())).await {
        Ok(Reply::Status(status)) => status,
        other => panic!("status replied {other:?}"),
    };
    let progress = status.recovery.expect("recovery is shown");
    assert_eq!(progress.describe(), "recovering 40/120 runs");

    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    let refused = h.orchestrator(cancel).await.expect_err("refused");
    assert!(matches!(refused, ArchonError::Recovering(_)), "{refused:?}");
    let shutdown = h.cli(Request::Shutdown).await.expect_err("refused");
    assert!(shutdown.to_string().contains("40/120"), "{shutdown}");
    assert_eq!(
        h.state(&item.id).await,
        QueueState::Queued,
        "nothing changed"
    );
}

#[tokio::test]
async fn a_change_waits_out_a_short_recovery_instead_of_failing() {
    let h = Harness::new().await;
    h.ctx.recovery.begin();
    let finishing = tokio::spawn({
        let ctx = h.ctx.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ctx.recovery.finish();
        }
    });

    let pause = Request::Pause(PauseTarget::Global { hard: false });
    let within = archon_daemon::recovery::RECOVERY_GRACE * 2;
    let served = tokio::time::timeout(within, h.cli(pause))
        .await
        .expect("bounded by the grace");
    assert!(served.is_ok(), "served once recovery ended: {served:?}");
    finishing.await.expect("finisher");
    assert_eq!(h.ctx.recovery.progress(), None);
}

/// Pause globally while recovery runs; the request waits out the grace.
fn pause_during_recovery(h: &Harness) -> tokio::task::JoinHandle<Result<Reply, ArchonError>> {
    let ctx = h.ctx.clone();
    tokio::spawn(async move {
        let pause = Request::Pause(PauseTarget::Global { hard: false });
        archon_daemon::handlers::handle(&ctx, ClientKind::ArchonCli, pause).await
    })
}

#[tokio::test]
async fn a_change_held_by_a_recovery_that_fails_into_standby_is_refused_by_it() {
    let h = Harness::new().await;
    h.ctx.recovery.begin();
    let pausing = pause_during_recovery(&h);
    // Let it reach the wait; one that arrives later meets the same refusal.
    tokio::time::sleep(Duration::from_millis(100)).await;

    h.ctx
        .standby
        .set("store: disk full: test".into())
        .expect("standby entered once");
    h.ctx.recovery.fail();

    let answer = pausing.await.expect("pause task");
    let refused = answer.expect_err("standby refuses the change");
    assert!(matches!(refused, ArchonError::DiskFull(_)), "{refused:?}");
    let read = h.cli(Request::Status(Default::default())).await;
    assert!(read.is_ok(), "reads are still served: {read:?}");
}

#[tokio::test]
async fn a_change_held_by_a_recovery_that_fails_is_never_admitted() {
    let h = Harness::new().await;
    h.ctx.recovery.begin();
    let pausing = pause_during_recovery(&h);
    tokio::time::sleep(Duration::from_millis(100)).await;

    h.ctx.recovery.fail();

    let refused = pausing.await.expect("pause task").expect_err("refused");
    assert!(
        matches!(refused, ArchonError::Unavailable(_)),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("failed to recover"),
        "{refused}"
    );
    let held = h
        .cli(Request::Pause(PauseTarget::Global { hard: false }))
        .await;
    assert!(held.is_err(), "a later change is refused too: {held:?}");
}
