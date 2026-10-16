//! The disk hold (ARCHITECTURE §7.3, §16) against a fake disk: low free
//! space holds dispatch with one `Blocker` per episode, a failure on a full
//! disk waits for space without using an infra retry, and standby serves
//! only reads.

mod common;

use std::time::Instant;

use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{DiskStatus, Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageState};
use archon_daemon::disk::{FullCause, FullDisk};
use common::Harness;

const GIB: u64 = 1 << 30;
const DISK_HOLD_KEY: &str = "dispatch_disk_hold";
const SQLITE_FULL: &str = "internal error: sqlite: database or disk is full";

/// A config whose low-disk hold is off, so only disk-full failures hold.
fn without_low_disk_hold() -> Config {
    let mut config = Config::default();
    config.limits.min_free_gb = 0;
    config
}

async fn disk_status(h: &Harness) -> DiskStatus {
    match h.cli(Request::Status(Default::default())).await {
        Ok(Reply::Status(status)) => status.disk.expect("archond reports its disk"),
        other => panic!("status: {other:?}"),
    }
}

/// System lines of the feed, oldest first.
async fn system_lines(h: &Harness) -> Vec<String> {
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    feed.into_iter()
        .filter_map(|item| match item.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn low_disk_holds_dispatch_with_one_blocker_per_episode() {
    let h = Harness::new().await;
    h.set_free_bytes(GIB);
    let item = h.enqueue("waits for disk").await;
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(
        h.state(&item.id).await,
        QueueState::Queued,
        "nothing starts"
    );
    assert!(h.backend.requests().is_empty());
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "one per episode: {blockers:?}");
    assert!(blockers[0].contains("1.0 GiB free at "), "{}", blockers[0]);
    assert!(
        blockers[0].contains("limits.min_free_gb (10.0 GiB)"),
        "{}",
        blockers[0]
    );
    assert!(blockers[0].contains("archon gc"), "{}", blockers[0]);
    let status = disk_status(&h).await;
    assert!(status.held && status.disk_full.is_none(), "{status:?}");
    assert_eq!(
        (status.free_bytes, status.min_free_bytes),
        (Some(GIB), 10 * GIB)
    );

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "space is back: it starts");
    assert_eq!(h.kv(DISK_HOLD_KEY).await.as_deref(), Some(""));
    let resumed = system_lines(&h).await;
    assert!(
        resumed.iter().any(|l| l.contains("dispatch resumes")),
        "{resumed:?}"
    );
    assert!(!disk_status(&h).await.held);

    h.set_free_bytes(GIB);
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(
        h.notices(NotificationKind::Blocker).await.len(),
        2,
        "a new episode"
    );
}

#[tokio::test]
async fn low_disk_also_holds_the_next_stage_of_an_already_running_item() {
    let h = Harness::new().await;
    h.enqueue("running before the disk filled").await;
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1);
    h.set_free_bytes(GIB);
    h.dispatch().await;
    h.settle_running(|_| common::completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(
        h.backend.requests().len(),
        1,
        "no next-stage agent starts below the floor"
    );
    assert!(disk_status(&h).await.held);
    assert!(h.notices(NotificationKind::Blocker).await[0].contains("limits.min_free_gb"));

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    assert!(
        h.backend.requests().len() > 1,
        "next stage starts once space returns"
    );
    assert!(!disk_status(&h).await.held);
}

#[tokio::test]
async fn min_free_gb_zero_turns_the_low_disk_hold_off() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    h.set_free_bytes(0);
    h.enqueue("runs on an empty disk").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    assert_eq!(disk_status(&h).await.min_free_bytes, 0);
}

#[tokio::test]
async fn a_stage_failing_on_a_full_disk_waits_for_space_and_keeps_its_infra_retry() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    let item = h.enqueue("disk fills mid-run").await;
    h.dispatch().await;
    h.set_free_bytes(GIB / 2);
    let full = || StageOutcome::Failed {
        error: SQLITE_FULL.into(),
        infra: false,
    };
    h.settle_running(|_| full()).await;
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 2, "a replacement is reserved");
    assert!(matches!(&stages[0].state, StageState::Failed { error } if error == SQLITE_FULL));
    assert_eq!(
        stages[1].state,
        StageState::Spawning,
        "not issued while held"
    );
    assert_eq!(h.backend.requests().len(), 1);
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(
        blockers[0].contains("database or disk is full"),
        "{}",
        blockers[0]
    );
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress
            .iter()
            .any(|p| p.contains("once disk space is free (1 of 5)")),
        "{progress:?}"
    );

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "issued once space is back");
    assert_eq!(h.backend.requests().len(), 2);

    // `infra_retries = 1` is still unused: an infrastructure failure retries.
    let infra = || StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    };
    h.settle_running(|_| infra()).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "the infra retry was left");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn create_agent_on_a_full_disk_waits_instead_of_failing_the_stage() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    h.set_free_bytes(GIB / 4);
    let refusal = format!("internal: {SQLITE_FULL}");
    h.backend.fail_next(ArchonError::Aether(refusal));
    let item = h.enqueue("aether's disk is full").await;
    h.dispatch().await;
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].state, StageState::Spawning, "held, not refused");
    assert_eq!(h.backend.requests().len(), 1, "not re-issued while held");
    assert!(disk_status(&h).await.disk_full.is_some());

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);
    let requests = h.backend.requests();
    assert_eq!(
        requests[0], requests[1],
        "the same request, under the same key"
    );
}

/// Fail the next `create_agent` on a full disk.
fn fail_next_spawn_on_a_full_disk(h: &Harness) {
    let refusal = format!("internal: {SQLITE_FULL}");
    h.backend.fail_next(ArchonError::Aether(refusal));
}

/// A check while Archon's disk dips, as the fake disk-full hold ends as
/// soon as space is back; the real one lasts minutes and is always seen.
async fn check_the_hold(h: &Harness) {
    h.set_free_bytes(GIB / 4);
    h.dispatch().await;
    assert!(disk_status(h).await.held, "held while the disk is low");
    h.set_free_bytes(100 * GIB);
}

/// Aether's own disk is full, which Archon cannot see: each hold ends
/// with room on Archon's disk, and `create_agent` fails again.
#[tokio::test]
async fn create_agent_on_a_disk_archon_cannot_see_fails_the_stage_after_five_tries() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    for _ in 0..5 {
        fail_next_spawn_on_a_full_disk(&h);
    }
    let item = h.enqueue("aether's disk stays full").await;
    for tries in 1..=4 {
        h.dispatch().await;
        check_the_hold(&h).await;
        assert_eq!(h.backend.requests().len(), tries, "once per hold");
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    }
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "one Blocker for one cause: {blockers:?}");
    let resumed = system_lines(&h).await;
    let resumes = resumed.iter().filter(|l| l.contains("dispatch resumes"));
    assert_eq!(resumes.count(), 1, "repeats end quietly: {resumed:?}");

    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 5);
    let stage = h.stages(&item.id).await.remove(0);
    assert!(
        matches!(&stage.state, StageState::Failed { error } if error.contains("full disk 5 times")),
        "{:?}",
        stage.state
    );
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 2, "{blockers:?}");
    assert!(blockers[1].contains("Aether's"), "{}", blockers[1]);
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 5, "never issued again");
}

/// Once an agent is created, the same disk-full failure is news again.
#[tokio::test]
async fn a_disk_full_failure_is_announced_again_after_an_agent_was_created() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    fail_next_spawn_on_a_full_disk(&h);
    h.enqueue("first").await;
    h.dispatch().await;
    check_the_hold(&h).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "issued once the hold ended");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);

    fail_next_spawn_on_a_full_disk(&h);
    fail_next_spawn_on_a_full_disk(&h);
    h.settle_running(|_| StageOutcome::Completed {
        summary: "done".into(),
        structured: None,
    })
    .await;
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 4, "both reviews were sent");
    assert!(
        h.running().await.is_empty(),
        "both reviews hit the full disk"
    );
    check_the_hold(&h).await;
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 2, "an agent ran in between: {blockers:?}");
    assert!(blockers[1].contains("database or disk is full"));
}

/// Failure text an agent wrote is not trusted to mean the disk is full
/// when there is room: it fails as the agent's own, with no hold.
#[tokio::test]
async fn an_agent_reporting_a_full_disk_with_room_to_spare_fails_as_its_own() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    let item = h.enqueue("agent quotes an error").await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "the tests print ENOSPC; fixing it is out of scope".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await.len(), 1, "no replacement");
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let status = disk_status(&h).await;
    assert!(!status.held && status.disk_full.is_none(), "{status:?}");
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        !progress.iter().any(|p| p.contains("a full disk")),
        "{progress:?}"
    );
}

/// Aether's infrastructure is trusted: an agent that died on a full disk
/// is a disk-full failure even with room on Archon's disk.
#[tokio::test]
async fn an_infrastructure_disk_full_failure_needs_no_free_space_check() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    let item = h.enqueue("engine's disk filled").await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine: No space left on device (os error 28)".into(),
        infra: true,
    })
    .await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 2, "a replacement is reserved");
    assert_eq!(stages[1].state, StageState::Spawning);
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress
            .iter()
            .any(|p| p.contains("once disk space is free (1 of 5)")),
        "{progress:?}"
    );
}

/// A run cancelled while its replacement waits for disk space retires at
/// once: its never-sent spawn is settled without waiting for the hold.
#[tokio::test]
async fn a_run_cancelled_during_a_disk_hold_retires_at_once() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    let item = h.enqueue("cancelled while held").await;
    h.dispatch().await;
    h.set_free_bytes(GIB / 2);
    h.settle_running(|_| StageOutcome::Failed {
        error: SQLITE_FULL.into(),
        infra: false,
    })
    .await;
    assert_eq!(h.stages(&item.id).await[1].state, StageState::Spawning);
    let run = h.run(&item.id).await;

    let cancelled = h
        .cli(Request::CancelItem {
            item: item.id.clone(),
            reason: None,
        })
        .await;
    assert!(cancelled.is_ok(), "{cancelled:?}");
    assert!(disk_status(&h).await.held, "the disk is still full");
    let stages = h.stages(&item.id).await;
    assert_eq!(stages[1].state, StageState::Cancelled, "{stages:?}");
    assert_eq!(h.backend.requests().len(), 1, "never sent to Aether");
    assert_eq!(h.backend.cancelled(), h.backend.node_ids(), "node retired");
    assert_eq!(h.kv(&format!("run_node:{}", run.id)).await, None);
}

async fn cancel(h: &Harness, item: &archon_core::id::QueueItemId) {
    let cancelled = h
        .cli(Request::CancelItem {
            item: item.clone(),
            reason: None,
        })
        .await;
    assert!(cancelled.is_ok(), "{cancelled:?}");
}

/// A cancelled run's spawn that may have reached Aether is sent once more
/// to learn what became of it. A full disk then settles it `cancelled`
/// instead of retrying it, since its run needs no agent.
#[tokio::test]
async fn a_cancelled_run_s_spawn_that_hits_a_full_disk_is_cancelled_not_retried() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    h.backend
        .fail_next(ArchonError::Unavailable("aether went quiet".into()));
    let item = h.enqueue("cancelled mid-spawn").await;
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "sent, its fate unknown");
    for _ in 0..5 {
        fail_next_spawn_on_a_full_disk(&h);
    }
    h.set_free_bytes(GIB / 4);

    cancel(&h, &item.id).await;
    for _ in 0..5 {
        h.dispatch().await;
    }
    assert_eq!(h.backend.requests().len(), 2, "sent once more, not retried");
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1);
    assert!(
        matches!(stages[0].state, StageState::Cancelled),
        "{:?}",
        stages[0].state
    );
    assert!(disk_status(&h).await.held, "the full disk still holds");
    assert_eq!(h.backend.cancelled(), h.backend.node_ids(), "node retired");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert!(
        !blockers.iter().any(|b| b.contains("had room")),
        "{blockers:?}"
    );
}

/// A spawn that hit a full disk while its run was live is not re-sent
/// during the hold once its run is cancelled: it waits for the hold to
/// end, like every spawn that may have reached Aether.
#[tokio::test]
async fn a_cancelled_run_s_sent_spawn_is_not_re_sent_during_the_hold() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    fail_next_spawn_on_a_full_disk(&h);
    fail_next_spawn_on_a_full_disk(&h);
    let item = h.enqueue("cancelled while its spawn waits").await;
    h.set_free_bytes(GIB / 4);
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "held after the full disk");

    cancel(&h, &item.id).await;
    for _ in 0..3 {
        h.dispatch().await;
    }
    assert_eq!(h.backend.requests().len(), 1, "not re-sent while held");
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 2, "sent once the hold ended");
    let stages = h.stages(&item.id).await;
    assert!(
        matches!(stages[0].state, StageState::Cancelled),
        "the full disk refused it again: {:?}",
        stages[0].state
    );
    assert_eq!(h.backend.cancelled(), h.backend.node_ids(), "node retired");
}

/// Low space alone is no disk-full failure: a cancelled run's spawn that
/// may have reached Aether is sent again at once, and the agent it finds,
/// whose reply was lost, is cancelled without waiting for space.
#[tokio::test]
async fn a_cancelled_run_s_sent_spawn_is_re_sent_during_a_low_disk_hold() {
    let h = Harness::new().await;
    h.backend.lose_next_reply();
    let item = h.enqueue("cancelled while disk is low").await;
    h.dispatch().await;
    assert_eq!(h.backend.created(), 1, "created, its reply lost");
    h.set_free_bytes(5 * GIB);
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "a live run's spawn waits");
    let status = disk_status(&h).await;
    assert!(status.held && status.disk_full.is_none(), "{status:?}");

    cancel(&h, &item.id).await;
    assert_eq!(h.backend.requests().len(), 2, "sent again during the hold");
    let stages = h.stages(&item.id).await;
    let agent = stages[0].aether_agent.clone().expect("the lost agent");
    assert!(h.backend.cancelled().contains(&agent), "{stages:?}");
    assert!(
        matches!(stages[0].state, StageState::Cancelled),
        "{:?}",
        stages[0].state
    );
    assert!(disk_status(&h).await.held, "the disk is still low");
}

/// Aether words its disk-full failure differently from request to
/// request: one cause is still announced once.
#[tokio::test]
async fn a_reworded_disk_full_failure_is_not_announced_again() {
    let h = Harness::with_config(without_low_disk_hold()).await;
    for request in 1..=3 {
        let refusal = format!("internal: {SQLITE_FULL} (request {request})");
        h.backend.fail_next(ArchonError::Aether(refusal));
    }
    h.enqueue("aether rewords its full disk").await;
    for tries in 1..=3 {
        h.dispatch().await;
        check_the_hold(&h).await;
        assert_eq!(h.backend.requests().len(), tries, "once per hold");
    }
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "one Blocker for one cause: {blockers:?}");
    assert!(blockers[0].contains("(request 1)"), "{}", blockers[0]);
    let resumed = system_lines(&h).await;
    let resumes = resumed.iter().filter(|l| l.contains("dispatch resumes"));
    assert_eq!(resumes.count(), 1, "repeats end quietly: {resumed:?}");
}

/// A quiet repeat of an announced disk-full hold (`full_again`) still
/// announces Archon's disk running low, once, and then the hold's end.
#[tokio::test]
async fn low_disk_during_a_quiet_disk_full_hold_is_announced_once() {
    let h = Harness::new().await;
    // The fake disk-full hold ends as soon as space is back, so it cannot
    // reach `full_again` by itself; the real, minutes-long one does.
    let error = format!("aether: internal: {SQLITE_FULL}");
    h.ctx
        .disk
        .report_full(&error, FullDisk::Aether, Instant::now());
    h.ctx
        .disk
        .announced_full(FullCause::of(FullDisk::Aether, &error));
    let quiet = h.ctx.store.lock().await.kv_set(DISK_HOLD_KEY, "full_again");
    quiet.expect("kv");

    h.set_free_bytes(5 * GIB);
    h.dispatch().await;
    h.dispatch().await;
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "low space is news: {blockers:?}");
    assert!(blockers[0].contains("full disk"), "{}", blockers[0]);
    assert!(
        blockers[0].contains("limits.min_free_gb (10.0 GiB)"),
        "{}",
        blockers[0]
    );
    assert_eq!(h.kv(DISK_HOLD_KEY).await.as_deref(), Some("full"));

    h.set_free_bytes(100 * GIB);
    h.dispatch().await;
    let resumed = system_lines(&h).await;
    let resumes = resumed.iter().filter(|l| l.contains("dispatch resumes"));
    assert_eq!(resumes.count(), 1, "its end is news too: {resumed:?}");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
}

#[tokio::test]
async fn an_internal_aether_error_is_retried_under_the_same_key_then_refused() {
    let h = Harness::new().await;
    for _ in 0..4 {
        h.backend
            .fail_next(ArchonError::Aether("internal: lock poisoned".into()));
    }
    let recovers = h.enqueue("aether hiccups").await;
    for _ in 0..5 {
        h.dispatch().await;
    }
    assert_eq!(h.stages(&recovers.id).await[0].state, StageState::Running);

    for _ in 0..5 {
        h.backend
            .fail_next(ArchonError::Aether("internal: lock poisoned".into()));
    }
    let refused = h.enqueue("aether keeps failing").await;
    for _ in 0..5 {
        h.dispatch().await;
    }
    let stage = h.stages(&refused.id).await.remove(0);
    assert!(
        matches!(&stage.state, StageState::Failed { error } if error.contains("kept failing")),
        "{:?}",
        stage.state
    );
}

#[tokio::test]
async fn standby_serves_reads_and_refuses_the_rest_naming_the_full_disk() {
    let h = Harness::new().await;
    let item = h.enqueue("queued before the disk filled").await;
    h.ctx
        .standby
        .set("store: disk full: insert feed entry".into())
        .expect("not in standby yet");
    let status = disk_status(&h).await;
    assert_eq!(
        status.standby.as_deref(),
        Some("store: disk full: insert feed entry")
    );
    let got = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await;
    assert!(got.is_ok(), "{got:?}");
    let refused = h
        .cli(Request::CancelItem {
            item: item.id,
            reason: None,
        })
        .await;
    let Err(ArchonError::DiskFull(why)) = refused else {
        panic!("refused as disk full: {refused:?}");
    };
    assert!(
        why.contains("standby") && why.contains("cancel_item"),
        "{why}"
    );
}

#[tokio::test]
async fn a_stalled_disk_probe_leaves_status_prompt_and_says_it_was_not_read() {
    let h = Harness::new().await;
    h.set_free_bytes(common::STALLED_DISK);
    let started = Instant::now();
    let disk = disk_status(&h).await;
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(2),
        "status took {took:?}"
    );
    assert!(took < common::STALL, "status did not wait for the probe");
    assert_eq!(disk.free_bytes, None);
    let error = disk.error.expect("the unread disk says why");
    assert!(error.contains("did not finish within"), "{error}");
}
