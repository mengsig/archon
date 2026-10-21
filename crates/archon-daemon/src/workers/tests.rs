use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

const FAST: Policy = Policy {
    first: Duration::from_millis(5),
    longest: Duration::from_millis(20),
    healthy_for: Duration::from_secs(60),
    give_up_after: 4,
    tell_at: 2,
};

async fn ended(workers: &mut Workers) -> Ended {
    tokio::time::timeout(Duration::from_secs(5), workers.next_ended())
        .await
        .expect("a worker ends within the test")
        .expect("a worker is left")
}

#[test]
fn pauses_double_up_to_the_longest() {
    assert_eq!(FAST.pause(1), Duration::from_millis(5));
    assert_eq!(FAST.pause(2), Duration::from_millis(10));
    assert_eq!(FAST.pause(3), Duration::from_millis(20));
    assert_eq!(FAST.pause(30), Duration::from_millis(20));
}

#[tokio::test]
async fn a_panicking_worker_is_restarted_and_the_others_keep_running() {
    let runs = Arc::new(AtomicU32::new(0));
    let mut workers = Workers::new(FAST);
    workers.spawn("steady", || std::future::pending());
    let counted = runs.clone();
    workers.spawn("flaky", move || {
        let attempt = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                panic!("bad stored data");
            }
            std::future::pending().await
        }
    });
    match ended(&mut workers).await {
        Ended::Restarting {
            name,
            error,
            failures,
            tell,
            ..
        } => {
            assert_eq!(name, "flaky");
            assert!(error.contains("panicked"), "{error}");
            assert_eq!(failures, 1);
            assert!(!tell);
        }
        other => panic!("expected a restart, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 2, "the flaky worker ran again");
    assert_eq!(workers.len(), 2);
    assert!(workers.stop(Duration::from_secs(1)).await.is_empty());
}

#[tokio::test]
async fn a_worker_that_stops_cleanly_is_restarted_too() {
    let mut workers = Workers::new(FAST);
    workers.spawn("quits", || async { Ok(()) });
    match ended(&mut workers).await {
        Ended::Restarting { error, .. } => assert!(error.contains("stopped"), "{error}"),
        other => panic!("expected a restart, got {other:?}"),
    }
}

#[tokio::test]
async fn a_worker_failing_in_a_row_is_told_then_disabled() {
    let mut workers = Workers::new(FAST);
    workers.spawn("broken", || async {
        Err(ArchonError::Invalid("no".into()))
    });
    let mut told = 0;
    loop {
        match ended(&mut workers).await {
            Ended::Restarting { tell, .. } => told += u32::from(tell),
            Ended::Disabled { name, failures, .. } => {
                assert_eq!((name, failures), ("broken", 4));
                break;
            }
            Ended::Fatal(error) => panic!("unexpected {error}"),
        }
    }
    assert_eq!(
        told, 1,
        "the operator hears once before the worker is disabled"
    );
    assert!(
        workers.next_ended().await.is_none(),
        "a disabled worker is not rerun"
    );
}

#[tokio::test]
async fn a_full_disk_is_fatal_not_restarted() {
    let mut workers = Workers::new(FAST);
    workers.spawn("writes", || async {
        Err(ArchonError::DiskFull("low".into()))
    });
    assert!(matches!(
        ended(&mut workers).await,
        Ended::Fatal(ArchonError::DiskFull(_))
    ));
}

#[tokio::test]
async fn stopping_cancels_a_worker_waiting_out_its_pause() {
    let slow = Policy {
        first: Duration::from_secs(3600),
        longest: Duration::from_secs(3600),
        ..FAST
    };
    let mut workers = Workers::new(slow);
    workers.spawn("failing", || async {
        Err(ArchonError::Invalid("no".into()))
    });
    assert!(matches!(
        ended(&mut workers).await,
        Ended::Restarting { .. }
    ));
    let left = tokio::time::timeout(Duration::from_secs(2), workers.stop(Duration::from_secs(1)))
        .await
        .expect("stop does not wait out the pause");
    assert!(left.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_leaves_a_worker_that_never_yields_behind() {
    let (started, running) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let started = std::sync::Mutex::new(Some(started));
    let blocked = Arc::new(std::sync::Mutex::new(Some(blocked)));
    let mut workers = Workers::new(FAST);
    workers.spawn("waits", || std::future::pending());
    workers.spawn("blocks", move || {
        let started = started.lock().unwrap().take();
        let blocked = blocked.lock().unwrap().take();
        async move {
            if let (Some(started), Some(blocked)) = (started, blocked) {
                started.send(()).expect("the test waits for the start");
                // Blocks its thread outside any `.await`, so an abort cannot land.
                let _released = blocked.recv();
            }
            Ok(())
        }
    });
    running.await.expect("the blocking worker started");

    let stop = workers.stop(Duration::from_millis(200));
    let stopped = tokio::time::timeout(Duration::from_secs(2), stop).await;
    release.send(()).expect("the blocked worker still waits");

    let left = stopped.expect("the stop is bounded");
    assert_eq!(left, ["blocks"], "only the blocked worker is left behind");
}
