mod common;

use std::collections::BTreeMap;

use archon_core::cost::DayRange;
use archon_core::protocol::WaitingReasonCode as Code;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState, StageState};
use archon_queue::Store;
use common::{item, run_for, stage, store, t};

fn range() -> DayRange {
    let day = t(0).date_naive();
    DayRange {
        since: day,
        until: day,
    }
}

#[test]
fn queue_reason_history_coalesces_observations_and_closes_on_state_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let mut store = Store::open(&path).unwrap();
    let dep = store.insert_item(&item(None, "dependency"), None).unwrap();
    let queued = store
        .insert_item(
            &QueueItem {
                dependencies: vec![dep.id],
                ..item(None, "target")
            },
            None,
        )
        .unwrap();
    for (seconds, reason) in [
        (5, Code::DependencyWaiting),
        (10, Code::Capacity),
        (15, Code::Capacity),
        (20, Code::TotalBudget),
        (30, Code::GlobalPause),
        (40, Code::ProviderQuota),
        (50, Code::NextInLine),
    ] {
        store
            .observe_queue_reasons(&BTreeMap::from([(queued.id.clone(), reason)]), t(seconds))
            .unwrap();
    }
    store
        .transition_item(&queued.id, QueueState::Dispatching, t(60))
        .unwrap();
    store
        .observe_queue_reasons(
            &BTreeMap::from([(queued.id.clone(), Code::Capacity)]),
            t(61),
        )
        .unwrap();
    drop(store);

    let reopened = Store::open_read_only(&path).unwrap();
    let report = reopened.timing_report(&range()).unwrap();
    assert_eq!(report.queue_ms["dependency"], 9_000);
    for reason in ["cap", "budget", "pause", "quota", "ready"] {
        assert_eq!(report.queue_ms[reason], 10_000, "{reason}");
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    let intervals: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM queue_reason_intervals WHERE item = ?1 AND ended_at IS NOT NULL",
            [queued.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        intervals, 6,
        "a repeated or stale observation starts no interval"
    );
}

#[test]
fn requeued_items_start_a_fresh_interval_and_ignore_old_snapshots() {
    let mut store = store();
    let queued = store.insert_item(&item(None, "requeue"), None).unwrap();
    store
        .transition_item(&queued.id, QueueState::Dispatching, t(10))
        .unwrap();
    assert_eq!(
        store.requeue_orphaned_dispatching(t(20)).unwrap(),
        vec![queued.id.clone()]
    );
    let observe = BTreeMap::from([(queued.id.clone(), Code::Capacity)]);
    store.observe_queue_reasons(&observe, t(15)).unwrap();
    store.observe_queue_reasons(&observe, t(25)).unwrap();
    store
        .transition_item(&queued.id, QueueState::Cancelled { reason: None }, t(30))
        .unwrap();
    let report = store.timing_report(&range()).unwrap();
    assert_eq!(report.queue_ms["other"], 14_000);
    assert_eq!(report.queue_ms["cap"], 5_000);
    assert_eq!(
        report.gap_ms, 10_000,
        "orphaned dispatch was still worktree setup time"
    );
}

#[test]
fn stages_use_wall_time_and_include_attempt_and_whole_run_retries() {
    let mut store = store();
    let item = store.insert_item(&item(None, "two runs"), None).unwrap();
    let first = start_run(&mut store, &item, 10);
    recorded_stage(&mut store, &first, 1, 0, 15, 25);
    recorded_stage(&mut store, &first, 1, 1, 20, 30);
    recorded_stage(&mut store, &first, 2, 0, 31, 36);
    let mut finished = first;
    finished
        .finish(
            RunState::Failed {
                stage_id: "implement".into(),
                error: "retry".into(),
            },
            t(40),
        )
        .unwrap();
    let failed = QueueState::Failed {
        run: Some(finished.id.clone()),
        error: "retry".into(),
    };
    store.finish_run(&finished, failed, t(40)).unwrap();
    store
        .transition_item(&item.id, QueueState::Queued, t(45))
        .unwrap();
    let mut next = start_run(&mut store, &item, 50);
    recorded_stage(&mut store, &next, 1, 0, 55, 59);
    recorded_stage(&mut store, &next, 1, 1, 56, 61);
    next.finish(
        RunState::Succeeded {
            summary: "done".into(),
        },
        t(65),
    )
    .unwrap();
    let succeeded = QueueState::Succeeded {
        run: next.id.clone(),
        summary: "done".into(),
    };
    store.finish_run(&next, succeeded, t(65)).unwrap();

    let report = store.timing_report(&range()).unwrap();
    assert_eq!(report.stage_ms, 26_000);
    assert_eq!(report.retry_ms, 11_000);
    assert_eq!(report.gap_ms, 19_000);
    assert_eq!(report.queue_ms["other"], 14_000);
}

#[test]
fn pull_request_intervals_clip_to_utc_days_and_count_a_shared_pr_once() {
    use archon_core::pull_request::PullRequestRef;
    use chrono::TimeDelta;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timing.db");
    let mut store = Store::open(&path).unwrap();
    let pr = PullRequestRef::from_url("https://github.com/example/archon/pull/10").unwrap();
    for label in ["delivery", "refresh"] {
        let item = store.insert_item(&item(None, label), None).unwrap();
        let run = start_run(&mut store, &item, 10);
        store.record_run_pull_request(&run.id, &pr, None).unwrap();
    }
    drop(store);
    let day = range().since.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let timestamp = |seconds| {
        (day + TimeDelta::seconds(seconds)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    };
    let conn = rusqlite::Connection::open(&path).unwrap();
    let rows = conn
        .execute(
            "UPDATE run_pull_requests
         SET created_at = ?1, checks_started_at = ?2, checks_finished_at = ?3, merged_at = ?4",
            [timestamp(-20), timestamp(-5), timestamp(15), timestamp(30)],
        )
        .unwrap();
    assert_eq!(rows, 2);
    drop(conn);
    let report = Store::open_read_only(&path)
        .unwrap()
        .timing_report(&range())
        .unwrap();
    assert_eq!(
        report.merge_wait_ms, 30_000,
        "clip the shared PR at midnight"
    );
    assert_eq!(report.ci_wait_ms, 15_000, "clip the same PR's CI window");
}

fn start_run(store: &mut Store, item: &QueueItem, seconds: i64) -> PipelineRun {
    store
        .transition_item(&item.id, QueueState::Dispatching, t(seconds))
        .unwrap();
    let mut run = run_for(item);
    run.started_at = t(seconds);
    assert_eq!(run.item, item.id);
    store.insert_run(&run).unwrap();
    store
        .transition_item(
            &item.id,
            QueueState::Running {
                run: run.id.clone(),
            },
            t(seconds),
        )
        .unwrap();
    run
}

fn recorded_stage(
    store: &mut Store,
    run: &PipelineRun,
    attempt: u32,
    slot: u32,
    start: i64,
    end: i64,
) {
    assert!(attempt > 0 && start < end);
    assert!(t(start) >= run.started_at);
    let mut stage = stage(run, slot);
    stage.attempt = attempt;
    stage.started_at = t(start);
    stage.finished_at = Some(t(end));
    stage.state = StageState::Completed;
    store.upsert_stage_run(&stage).unwrap();
}
