mod common;

use std::path::Path;

use archon_core::Timestamp;
use archon_core::id::PipelineRunId;
use archon_core::pull_request::PullRequestRef;
use archon_queue::Store;
use archon_queue::store::PullRequestTimes;
use common::{item, start, t};

fn pr(number: u64) -> PullRequestRef {
    PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{number}")).unwrap()
}

fn start_delivery(store: &mut Store, title: &str) -> PipelineRunId {
    let item = store.insert_item(&item(None, title), None).unwrap();
    start(store, &item.id).id
}

fn read_times(path: &Path, run: &PipelineRunId) -> [Option<Timestamp>; 4] {
    let conn = rusqlite::Connection::open(path).unwrap();
    let raw: [Option<String>; 4] = conn
        .query_row(
            "SELECT created_at, checks_started_at, checks_finished_at, merged_at
             FROM run_pull_requests WHERE run = ?1",
            [run.as_str()],
            |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]),
        )
        .unwrap();
    raw.map(|text| text.map(|text| text.parse().unwrap()))
}

#[test]
fn pending_reruns_preserve_the_last_complete_interval_until_observed_finished() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let mut store = Store::open(&path).unwrap();
    let run = start_delivery(&mut store, "first delivery");
    store
        .record_run_pull_request_with_times(
            &run,
            &pr(1),
            None,
            PullRequestTimes {
                created_at: Some(t(1)),
                checks_started_at: Some(t(2)),
                checks_finished_at: Some(t(3)),
                merged_at: Some("0001-01-01T00:00:00Z".parse().unwrap()),
            },
        )
        .unwrap();
    store
        .observe_pull_request_times(
            &pr(1),
            PullRequestTimes {
                checks_started_at: Some(t(4)),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        read_times(&path, &run),
        [Some(t(1)), Some(t(2)), Some(t(3)), None]
    );
    store
        .observe_pull_request_times(
            &pr(1),
            PullRequestTimes {
                checks_started_at: Some(t(4)),
                checks_finished_at: Some(t(6)),
                merged_at: Some(t(7)),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        read_times(&path, &run),
        [Some(t(1)), Some(t(2)), Some(t(6)), Some(t(7))]
    );
}

#[test]
fn timestamps_follow_pr_identity_across_delivery_replay_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let mut store = Store::open(&path).unwrap();
    let first = start_delivery(&mut store, "first delivery");
    let second = start_delivery(&mut store, "follow-up");
    store
        .record_run_pull_request_with_times(
            &first,
            &pr(1),
            None,
            PullRequestTimes {
                created_at: Some(t(1)),
                checks_started_at: Some(t(2)),
                checks_finished_at: Some(t(6)),
                merged_at: Some(t(7)),
            },
        )
        .unwrap();
    let observed = [Some(t(1)), Some(t(2)), Some(t(6)), Some(t(7))];
    store
        .record_run_pull_request(&second, &pr(1), None)
        .unwrap();
    store.record_run_pull_request(&first, &pr(1), None).unwrap();
    assert_eq!(read_times(&path, &first), observed);
    assert_eq!(read_times(&path, &second), observed);
    store
        .record_run_pull_request(&second, &pr(2), None)
        .unwrap();
    assert_eq!(read_times(&path, &second), [None; 4]);
    drop(store);

    let mut reopened = Store::open(&path).unwrap();
    reopened
        .record_run_pull_request(&second, &pr(1), None)
        .unwrap();
    assert_eq!(read_times(&path, &second), observed);
}
