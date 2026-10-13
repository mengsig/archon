//! On-disk behaviour: reopen, crash without a clean close, migrations,
//! local-day spend and its corrections, and the concurrent access patterns
//! the daemon uses.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QuestionId};
use archon_core::notify::{FeedEntry, FeedItem, Notification, NotificationKind, Question};
use archon_core::protocol::{ItemQuery, RepoRef};
use archon_core::queue::{PauseState, QueueItem, QueueState};
use archon_queue::dispatch::Hold;
use archon_queue::store::{SpendRow, keys};
use archon_queue::{Store, plan_dispatch};
use chrono::TimeZone;
use common::*;

fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("nested").join("archon.db")
}

fn all_items(store: &Store) -> Vec<QueueItem> {
    let query = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    store.items(&query).unwrap()
}

fn progress(text: &str) -> FeedEntry {
    FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind: NotificationKind::Progress,
        text: text.into(),
        item: None,
        run: None,
        at: t(0),
    })
}

/// Writes one of everything; returns what a reopened store must show.
fn populate(store: &mut Store) -> (Vec<QueueItem>, archon_core::run::PipelineRun, Question) {
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let a = store
        .insert_item(&item(Some(&repo), "a"), Some("k-a"))
        .unwrap();
    let b = QueueItem {
        dependencies: vec![a.id.clone()],
        ..item(Some(&repo), "b")
    };
    store.insert_item(&b, None).unwrap();
    let run = start(store, &a.id);
    store.upsert_stage_run(&running_stage(&run, 0, 1)).unwrap();
    store
        .append_feed(FeedEntry::Operator { text: "go".into() }, t(1))
        .unwrap();
    store.append_feed(progress("half"), t(2)).unwrap();
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: "ok?".into(),
        options: vec![],
        item: Some(a.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: t(3),
        answer: None,
        answered_at: None,
    };
    let question = store.insert_question(&question).unwrap();
    assert_eq!(question.number, 1);
    let row = SpendRow {
        agent: agent(1),
        item: Some(a.id.clone()),
        cost_usd: 1.25,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: 0.5,
        input_tokens: 10,
        output_tokens: 5,
        at: t(4),
    };
    store.record_spend(&row).unwrap();
    store
        .set_pause_state(&PauseState {
            global: true,
            repos: vec![repo.id.clone()],
            pipelines: vec![],
        })
        .unwrap();
    store.kv_set(keys::ORCHESTRATOR_AGENT, "agt_orch").unwrap();
    (all_items(store), store.run(&run.id).unwrap(), question)
}

fn assert_populated(
    store: &Store,
    items: &[QueueItem],
    run: &archon_core::run::PipelineRun,
    question: &Question,
) {
    assert_eq!(all_items(store), items);
    assert_eq!(store.active_runs().unwrap(), vec![run.clone()]);
    assert_eq!(
        store.stage_run_by_agent(&agent(1)).unwrap().unwrap().run,
        run.id
    );
    assert_eq!(store.questions(true).unwrap(), vec![question.clone()]);
    assert_eq!(store.feed_after(0, 10).unwrap().len(), 2);
    assert_eq!(store.spend_for_item(&run.item, true).unwrap(), 1.75);
    assert!(store.pause_state().unwrap().global);
    assert_eq!(
        store.kv_get(keys::ORCHESTRATOR_AGENT).unwrap().as_deref(),
        Some("agt_orch")
    );
    assert_eq!(store.dependents(&run.item).unwrap().len(), 1);
}

#[test]
fn everything_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let (items, run, question) = populate(&mut Store::open(&path).unwrap());
    let mut store = Store::open(&path).unwrap();
    assert_populated(&store, &items, &run, &question);
    // The feed continues without a gap and idempotency keys still hold.
    let next = store
        .append_feed(
            FeedEntry::System {
                text: "restarted".into(),
            },
            t(9),
        )
        .unwrap();
    assert_eq!(next.seq, 3);
    let again = QueueItem {
        id: archon_core::id::QueueItemId::new(),
        ..items.last().unwrap().clone()
    };
    let again = QueueItem {
        state: QueueState::Queued,
        ..again
    };
    assert_eq!(
        store.insert_item(&again, Some("k-a")).unwrap().id,
        items.last().unwrap().id
    );
}

#[test]
fn committed_writes_survive_a_crash_without_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let expected = populate(&mut store);
    // Leak the connection: no close, no checkpoint, like a killed process.
    std::mem::forget(store);
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(wal.exists(), "commits should still sit in the WAL");
    let store = Store::open(&path).unwrap();
    assert_populated(&store, &expected.0, &expected.1, &expected.2);
}

#[test]
fn recovery_after_a_crash_mid_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let orphan = store
        .insert_item(&item(Some(&repo), "orphan"), None)
        .unwrap();
    let with_run = store
        .insert_item(&item(Some(&repo), "with run"), None)
        .unwrap();
    store
        .transition_item(&orphan.id, QueueState::Dispatching, t(2))
        .unwrap();
    let dispatching = store
        .transition_item(&with_run.id, QueueState::Dispatching, t(2))
        .unwrap();
    let run = run_for(&dispatching);
    store.insert_run(&run).unwrap();
    let spawning = stage(&run, 0);
    store.upsert_stage_run(&spawning).unwrap();
    std::mem::forget(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.requeue_orphaned_dispatching(t(60)).unwrap(),
        vec![orphan.id.clone()]
    );
    assert_eq!(store.item(&orphan.id).unwrap().state, QueueState::Queued);
    // The run survives, so its `spawning` stage is re-spawned under the same key.
    assert_eq!(store.active_runs().unwrap(), vec![run.clone()]);
    assert_eq!(store.stage_runs(&run.id).unwrap(), vec![spawning]);
    let running = store
        .transition_item(
            &with_run.id,
            QueueState::Running {
                run: run.id.clone(),
            },
            t(61),
        )
        .unwrap();
    assert_eq!(running.state.kind(), "running");
}

fn schema_versions(path: &Path) -> Vec<i64> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn migrations_apply_once_and_a_newer_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(Store::open(&path).unwrap());
    drop(Store::open(&path).unwrap());
    assert_eq!(schema_versions(&path), vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (99, 'future')",
        [],
    )
    .unwrap();
    drop(conn);
    let err = Store::open(&path).unwrap_err();
    assert!(
        matches!(&err, ArchonError::Store(m) if m.contains("newer")),
        "{err}"
    );
}

#[test]
fn a_stored_ship_mode_that_is_none_is_an_error_naming_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let (web, lib) = (repo("web"), repo("lib"));
    store.insert_repo(&web).unwrap();
    store.insert_repo(&lib).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let changed = conn
        .execute(
            "UPDATE repos SET data = json_set(data, '$.ship', 'gated') WHERE name = 'web'",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    drop(conn);

    let store = Store::open(&path).unwrap();
    let expected = "store: decode repository \"web\": ship mode \"gated\" is not one of pr, local";
    for reference in [web.name.clone(), web.id.to_string()] {
        let error = store.repo(&RepoRef(reference)).unwrap_err();
        assert!(matches!(error, ArchonError::Store(_)), "{error}");
        assert!(error.to_string().starts_with(expected), "{error}");
    }
    let listed = store.repos().unwrap_err();
    assert!(listed.to_string().starts_with(expected), "{listed}");
    assert_eq!(store.repo(&RepoRef("lib".into())).unwrap(), lib);
}

#[test]
fn concurrent_first_opens_migrate_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(db_path(&dir));
    let barrier = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
            thread::spawn(move || {
                barrier.wait();
                Store::open(&path).map(drop)
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(schema_versions(&path), vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

/// Appends `entry`, trying again while another writer holds the write
/// lock past the busy timeout, as four writers syncing in full can on a
/// loaded machine. Taking the lock is the first step, so a refused append
/// wrote nothing, and retrying it cannot duplicate an entry.
fn append_retrying_busy(store: &mut Store, entry: FeedEntry, at: Timestamp) -> FeedItem {
    const ATTEMPTS: usize = 5;
    let mut busy = 0;
    loop {
        match store.append_feed(entry.clone(), at) {
            Ok(item) => return item,
            Err(ArchonError::Store(why)) if is_busy_begin(&why) && busy + 1 < ATTEMPTS => {
                busy += 1;
            }
            Err(error) => panic!("append after {busy} busy retries: {error}"),
        }
    }
}

fn is_busy_begin(why: &str) -> bool {
    why.starts_with("begin write transaction:")
        && (why.contains("database is locked") || why.contains("database is busy"))
}

/// Several connections append at once: the feed stays gapless and each
/// writer sees its own entries in increasing order.
#[test]
fn concurrent_feed_appends_stay_gapless() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(db_path(&dir));
    drop(Store::open(&path).unwrap());
    let (writers, per_writer): (usize, i64) = (4, 50);
    let barrier = Arc::new(Barrier::new(writers));
    let handles: Vec<_> = (0..writers)
        .map(|w| {
            let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
            thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                barrier.wait();
                (0..per_writer)
                    .map(|i| {
                        let text = format!("{w}:{i}");
                        append_retrying_busy(&mut store, FeedEntry::System { text }, t(i)).seq
                    })
                    .collect::<Vec<u64>>()
            })
        })
        .collect();
    let mut all = Vec::new();
    for handle in handles {
        let seqs = handle.join().unwrap();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        all.extend(seqs);
    }
    all.sort_unstable();
    let expected: Vec<u64> = (1..=(writers as u64 * per_writer as u64)).collect();
    assert_eq!(all, expected);
    let store = Store::open(&path).unwrap();
    let feed = store.feed_after(0, usize::MAX).unwrap();
    assert_eq!(feed.iter().map(|f| f.seq).collect::<Vec<_>>(), expected);
}

/// An MCP retry storm: the same enqueue with the same key from many
/// connections creates exactly one item.
#[test]
fn racing_idempotent_enqueues_create_one_item() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(db_path(&dir));
    let repo = repo("alpha");
    Store::open(&path).unwrap().insert_repo(&repo).unwrap();
    let template = item(Some(&repo), "once");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (path, barrier) = (Arc::clone(&path), Arc::clone(&barrier));
            let attempt = QueueItem {
                id: archon_core::id::QueueItemId::new(),
                ..template.clone()
            };
            thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                barrier.wait();
                store.insert_item(&attempt, Some("storm")).unwrap().id
            })
        })
        .collect();
    let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    assert_eq!(all_items(&Store::open(&path).unwrap()).len(), 1);
}

/// The daemon shares one store behind a mutex; tasks interleave item
/// lifecycles, feed appends and spend updates.
#[test]
fn a_shared_store_serves_interleaved_daemon_tasks() {
    let (store, repo, items) = seeded(8);
    let shared = Arc::new(Mutex::new(store));
    let handles: Vec<_> = items
        .into_iter()
        .enumerate()
        .map(|(n, it)| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || drive_item(&shared, &it, n as u32))
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let store = shared.lock().unwrap();
    assert!(
        all_items(&store)
            .iter()
            .all(|i| i.state.kind() == "succeeded")
    );
    assert!(store.active_runs().unwrap().is_empty());
    let feed = store.feed_after(0, usize::MAX).unwrap();
    assert_eq!(feed.len(), 16);
    assert!(feed.iter().enumerate().all(|(i, f)| f.seq == i as u64 + 1));
    assert_eq!(store.spend_totals(t(100), false).unwrap().1, 8.0);
    assert_eq!(
        store
            .repo(&archon_core::protocol::RepoRef(repo.name))
            .unwrap()
            .id,
        repo.id
    );
}

fn drive_item(shared: &Mutex<Store>, it: &QueueItem, n: u32) {
    let run = start(&mut shared.lock().unwrap(), &it.id);
    let feed = |text: String| {
        shared
            .lock()
            .unwrap()
            .append_feed(FeedEntry::System { text }, t(30))
            .unwrap();
    };
    feed(format!("started {n}"));
    let mut stage = running_stage(&run, 0, n + 1);
    shared.lock().unwrap().upsert_stage_run(&stage).unwrap();
    let row = SpendRow {
        agent: agent(n + 1),
        item: Some(it.id.clone()),
        cost_usd: 1.0,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: 0.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(40),
    };
    shared.lock().unwrap().record_spend(&row).unwrap();
    stage.state = archon_core::run::StageState::Completed;
    stage.finished_at = Some(t(50));
    let mut store = shared.lock().unwrap();
    store.upsert_stage_run(&stage).unwrap();
    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "ok".into(),
    };
    settle_run(&mut store, &run, done, t(52));
    drop(store);
    feed(format!("finished {n}"));
}

fn local(day: u32, hour: u32) -> Timestamp {
    chrono::Local
        .with_ymd_and_hms(2026, 3, day, hour, 0, 0)
        .earliest()
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// Absolute totals are split into the local days in which they grew.
#[test]
fn daily_spend_counts_only_growth_within_the_local_day() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let row = |n: u32, cost: f64, est: f64, at: Timestamp| SpendRow {
        agent: agent(n),
        item: None,
        cost_usd: cost,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: est,
        input_tokens: 0,
        output_tokens: 0,
        at,
    };
    store
        .record_spend(&row(1, 40.0, 4.0, local(10, 12)))
        .unwrap();
    store
        .record_spend(&row(1, 41.0, 4.5, local(11, 9)))
        .unwrap();
    store
        .record_spend(&row(1, 43.0, 5.0, local(11, 18)))
        .unwrap();
    store
        .record_spend(&row(2, 2.0, 0.0, local(11, 10)))
        .unwrap();
    // Replays of old totals change nothing.
    store
        .record_spend(&row(1, 41.0, 4.5, local(11, 9)))
        .unwrap();
    store
        .record_spend(&row(1, 40.0, 4.0, local(10, 12)))
        .unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.spend_totals(local(10, 20), false).unwrap(),
        (40.0, 45.0)
    );
    assert_eq!(
        store.spend_totals(local(11, 20), false).unwrap(),
        (5.0, 45.0)
    );
    assert_eq!(
        store.spend_totals(local(11, 20), true).unwrap(),
        (6.0, 50.0)
    );
    assert_eq!(store.spend_totals(local(12, 1), true).unwrap(), (0.0, 50.0));
    let snapshot = store.dispatch_snapshot(local(11, 23)).unwrap();
    assert_eq!(
        (snapshot.spend_today_usd, snapshot.spend_total_usd),
        (6.0, 50.0)
    );
}

fn usage(n: u32, cost: f64, estimate: f64, at: Timestamp) -> SpendRow {
    SpendRow {
        agent: agent(n),
        item: None,
        cost_usd: cost,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: estimate,
        input_tokens: 0,
        output_tokens: 0,
        at,
    }
}

/// A total the provider reported before the upgrade is a floor after it;
/// a charge of unknown source is corrected by the first settled total.
#[test]
fn v9_keeps_an_earlier_provider_reported_total_as_the_floor() {
    use archon_core::cost::CostBasis;
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    store
        .record_spend(&usage(1, 2.0, 0.0, local(8, 12)))
        .unwrap();
    let unknown = SpendRow {
        cost_basis: CostBasis::Unknown,
        ..usage(2, 2.0, 0.0, local(8, 12))
    };
    store.record_spend(&unknown).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE spend DROP COLUMN settled_cost_usd;
         DELETE FROM schema_migrations WHERE version = 9;",
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(schema_versions(&path).last(), Some(&9));
    for n in [1, 2] {
        let settled = SpendRow {
            cost_complete: Some(true),
            ..usage(n, 0.5, 0.0, local(8, 13))
        };
        store.record_spend(&settled).unwrap();
    }
    assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 2.0);
    assert_eq!(store.spend_for_agent(&agent(2), false).unwrap(), 0.5);
    assert_eq!(store.spend_totals(local(8, 14), false).unwrap(), (2.5, 2.5));
}

#[test]
fn v7_does_not_infer_history_from_the_latest_cumulative_basis() {
    use archon_core::cost::{CostBasis, CostGroup, DayRange};
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    store
        .record_spend(&SpendRow {
            cost_basis: CostBasis::Unknown,
            ..usage(1, 2.0, 0.5, local(8, 12))
        })
        .unwrap();
    store
        .record_spend(&usage(1, 3.0, 0.75, local(9, 12)))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE spend_daily DROP COLUMN billed_cost_usd;
         ALTER TABLE spend_daily DROP COLUMN list_price_cost_usd;
         ALTER TABLE spend_daily DROP COLUMN unclassified_cost_usd;
         DROP TABLE delegated_agents;
         ALTER TABLE spend DROP COLUMN settled_cost_usd;
         DELETE FROM schema_migrations WHERE version >= 7;",
    )
    .unwrap();
    drop(conn);

    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        for (day, cost, estimate) in [(8, 2.0, 0.5), (9, 1.0, 0.25)] {
            let date = local(day, 12).with_timezone(&chrono::Local).date_naive();
            let report = store
                .cost_report(
                    &DayRange {
                        since: date,
                        until: date,
                    },
                    CostGroup::Day,
                    true,
                )
                .unwrap();
            assert_eq!(report.total.unclassified_cost_usd, cost);
            assert_eq!(report.total.cost_usd, 0.0);
            assert_eq!(report.total.estimated_cost_usd, estimate);
            assert_eq!(report.total.counted_usd, cost + estimate);
        }
        assert_eq!(
            store.spend_totals(local(9, 12), true).unwrap(),
            (1.25, 3.75)
        );
    }
}

#[test]
fn migrated_unknown_charges_keep_their_basis_after_new_usage() {
    use archon_core::cost::{CostAmounts, CostBasis, CostGroup, DayRange};

    for basis in [CostBasis::ProviderReported, CostBasis::ListPrice] {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut store = Store::open(&path).unwrap();
        let owner = store
            .insert_item(&item(None, "legacy charges"), None)
            .unwrap();
        let run = start(&mut store, &owner.id);
        store.upsert_stage_run(&running_stage(&run, 0, 1)).unwrap();
        let old = SpendRow {
            item: Some(owner.id.clone()),
            cost_basis: CostBasis::Unknown,
            ..usage(1, 2.0, 0.5, local(8, 12))
        };
        store.record_spend(&old).unwrap();
        drop(store);

        // Recreate the pre-provenance ledger; every other record is unchanged.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE legacy_days AS SELECT agent, day, cost_usd, estimated_cost_usd FROM spend_daily;
             DROP TABLE spend_daily;
             CREATE TABLE spend_daily (
                 agent TEXT NOT NULL REFERENCES spend(agent), day TEXT NOT NULL,
                 cost_usd REAL NOT NULL, estimated_cost_usd REAL NOT NULL,
                 PRIMARY KEY (agent, day)
             ) WITHOUT ROWID;
             INSERT INTO spend_daily SELECT * FROM legacy_days;
             DROP TABLE legacy_days;
             CREATE INDEX spend_daily_by_day ON spend_daily(day);
             ALTER TABLE spend DROP COLUMN cost_basis;
             DROP TABLE agent_runs;
             DROP TABLE delegated_agents;
             ALTER TABLE spend DROP COLUMN settled_cost_usd;
             DELETE FROM schema_migrations WHERE version >= 5;",
        )
        .unwrap();
        drop(conn);

        let mut store = Store::open(&path).unwrap();
        let first_day = local(8, 12).with_timezone(&chrono::Local).date_naive();
        let first_range = DayRange {
            since: first_day,
            until: first_day,
        };
        let before = store
            .cost_report(&first_range, CostGroup::Day, true)
            .unwrap();
        assert_eq!(before.total.unclassified_cost_usd, 2.0);
        let new = SpendRow {
            cost_usd: 3.0,
            cost_basis: basis,
            estimated_cost_usd: 0.75,
            at: local(9, 12),
            ..old.clone()
        };
        store.record_spend(&new).unwrap();
        store.record_spend(&new).unwrap();
        store.record_spend(&old).unwrap();
        drop(store);

        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(
                store
                    .cost_report(&first_range, CostGroup::Day, true)
                    .unwrap(),
                before
            );
            let second_day = new.at.with_timezone(&chrono::Local).date_naive();
            for estimates in [false, true] {
                let daily = if estimates { 1.25 } else { 1.0 };
                let total = if estimates { 3.75 } else { 3.0 };
                assert_eq!(
                    store.spend_totals(new.at, estimates).unwrap(),
                    (daily, total)
                );
                assert_eq!(store.spend_for_agent(&new.agent, estimates).unwrap(), total);
                assert_eq!(store.spend_for_item(&owner.id, estimates).unwrap(), total);
                assert_eq!(store.spend_for_run(&run.id, estimates).unwrap(), total);
                for by in CostGroup::ALL {
                    let report = store
                        .cost_report(
                            &DayRange {
                                since: second_day,
                                until: second_day,
                            },
                            by,
                            estimates,
                        )
                        .unwrap();
                    assert_eq!(
                        report.total,
                        CostAmounts {
                            counted_usd: daily,
                            cost_usd: if basis == CostBasis::ProviderReported {
                                1.0
                            } else {
                                0.0
                            },
                            estimated_cost_usd: if basis == CostBasis::ListPrice {
                                1.25
                            } else {
                                0.25
                            },
                            unclassified_cost_usd: 0.0,
                        }
                    );
                    let all = store
                        .cost_report(
                            &DayRange {
                                since: first_day,
                                until: second_day,
                            },
                            by,
                            estimates,
                        )
                        .unwrap();
                    assert_eq!(all.total.counted_usd, total);
                    assert_eq!(all.total.unclassified_cost_usd, 2.0);
                }
            }
        }
    }
}

/// A lower absolute total after midnight must not credit today with
/// yesterday's spend, or unrelated agents could spend past the daily cap.
#[test]
fn a_usage_correction_never_credits_the_daily_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    store
        .record_spend(&usage(1, 40.0, 0.0, local(10, 12)))
        .unwrap();
    store
        .record_spend(&usage(1, 0.0, 0.0, local(11, 9)))
        .unwrap();
    store
        .record_spend(&usage(2, 10.0, 0.0, local(11, 10)))
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.spend_totals(local(11, 20), false).unwrap(),
        (10.0, 50.0),
        "agent 1 keeps the $40 already charged, and today only agent 2 spent"
    );
    assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 40.0);
    let mut snapshot = store.dispatch_snapshot(local(11, 20)).unwrap();
    snapshot.caps.budget_usd_daily = 5.0;
    assert_eq!(plan_dispatch(&snapshot).held_by, Some(Hold::DailyBudget));

    // Growth is charged again only above what was already charged.
    store
        .record_spend(&usage(1, 30.0, 0.0, local(11, 11)))
        .unwrap();
    assert_eq!(
        store.spend_totals(local(11, 20), false).unwrap(),
        (10.0, 50.0)
    );
    store
        .record_spend(&usage(1, 45.0, 0.0, local(11, 12)))
        .unwrap();
    assert_eq!(
        store.spend_totals(local(11, 20), false).unwrap(),
        (15.0, 55.0)
    );
    assert_eq!(
        store.spend_totals(local(10, 20), false).unwrap(),
        (40.0, 55.0)
    );
}

/// Cost and estimate are charged separately, so an estimate replaced by a
/// real cost never lowers either ledger.
#[test]
fn an_estimate_correction_never_lowers_either_ledger() {
    let mut store = store();
    store
        .record_spend(&usage(1, 0.0, 10.0, local(10, 12)))
        .unwrap();
    store
        .record_spend(&usage(1, 8.0, 0.0, local(11, 9)))
        .unwrap();
    assert_eq!(
        store.spend_totals(local(11, 20), false).unwrap(),
        (8.0, 8.0)
    );
    assert_eq!(
        store.spend_totals(local(11, 20), true).unwrap(),
        (8.0, 18.0)
    );
    assert_eq!(
        store.spend_totals(local(10, 20), true).unwrap(),
        (10.0, 18.0)
    );
    for day in [10, 11] {
        let (today, _) = store.spend_totals(local(day, 20), true).unwrap();
        assert!(today >= 0.0, "day {day} spent {today}");
    }
}

/// A different total with the same timestamp is a correction too; the same
/// total again is a replay. Neither changes what was charged.
#[test]
fn equal_timestamp_replays_and_corrections_charge_nothing() {
    let mut store = store();
    let at = local(11, 9);
    store.record_spend(&usage(1, 6.0, 2.0, at)).unwrap();
    store.record_spend(&usage(1, 6.0, 2.0, at)).unwrap();
    assert_eq!(store.spend_totals(at, true).unwrap(), (8.0, 8.0));
    store.record_spend(&usage(1, 1.0, 0.5, at)).unwrap();
    assert_eq!(store.spend_totals(at, true).unwrap(), (8.0, 8.0));
    assert_eq!(store.spend_for_agent(&agent(1), true).unwrap(), 8.0);
    store.record_spend(&usage(1, 7.0, 0.5, at)).unwrap();
    assert_eq!(store.spend_totals(at, true).unwrap(), (9.0, 9.0));
}

/// A cancelled run's agents keep their capacity until they settle. After a
/// crash, `active_runs` no longer reaches them, so recovery needs
/// `live_stage_runs` to re-spawn or reconcile them; otherwise the slots leak.
#[test]
fn recovery_finds_live_stage_runs_of_finished_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let a = store.insert_item(&item(Some(&repo), "a"), None).unwrap();
    let b = store.insert_item(&item(Some(&repo), "b"), None).unwrap();
    let cancelled_run = start(&mut store, &a.id);
    let live_run = start(&mut store, &b.id);
    let orphan = stage(&cancelled_run, 0);
    store.upsert_stage_run(&orphan).unwrap();
    let running = archon_core::run::StageRun {
        started_at: t(21),
        ..running_stage(&live_run, 0, 7)
    };
    store.upsert_stage_run(&running).unwrap();
    let cancel = QueueState::Cancelled { reason: None };
    settle_run(&mut store, &cancelled_run, cancel, t(40));
    std::mem::forget(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.active_runs().unwrap(), vec![live_run]);
    assert_eq!(
        store.live_stage_runs().unwrap(),
        vec![orphan.clone(), running]
    );
    assert_eq!(store.dispatch_snapshot(t(41)).unwrap().running_total, 2);
    let settled = archon_core::run::StageRun {
        state: archon_core::run::StageState::Cancelled,
        finished_at: Some(t(42)),
        ..orphan
    };
    store.upsert_stage_run(&settled).unwrap();
    assert_eq!(store.live_stage_runs().unwrap().len(), 1);
    assert_eq!(store.dispatch_snapshot(t(43)).unwrap().running_total, 1);
}

/// `archon gc --offline` reads the store beside a writer that may have left
/// its WAL behind, creates nothing, and cannot write.
#[test]
fn a_read_only_store_reads_what_was_written_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let missing = Store::open_read_only(&path).unwrap_err();
    assert!(matches!(missing, ArchonError::Store(_)), "{missing}");
    assert!(!path.exists(), "a read-only open creates no store");

    let mut writer = Store::open(&path).unwrap();
    let written = writer.insert_item(&item(None, "kept"), None).unwrap();
    let mut reader = Store::open_read_only(&path).unwrap();
    let read = all_items(&reader);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].id, written.id);
    let refused = reader.insert_item(&item(None, "new"), None);
    assert!(refused.is_err(), "a read-only store refuses writes");
    drop(writer);
    assert_eq!(all_items(&Store::open_read_only(&path).unwrap()).len(), 1);
}

/// A store its writer closed cleanly is read immutable: on a full disk no
/// `-wal` or `-shm` could be created, so none is. A `?`, `#` or `%` in the
/// path is not taken for URI syntax.
#[test]
fn a_cleanly_closed_store_is_read_without_creating_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("odd ?#% name").join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    let written = writer.insert_item(&item(None, "kept"), None).unwrap();
    drop(writer);
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let listing = || {
        let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    assert!(!beside("-wal").exists() && !beside("-shm").exists());
    let before = listing();

    let reader = Store::open_read_only(&path).unwrap();
    let read = all_items(&reader);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].id, written.id);
    drop(reader);
    assert_eq!(listing(), before, "reading created nothing");
}

#[test]
fn v6_backfills_only_unambiguous_run_ownership_and_preserves_it() {
    use archon_core::aether_ids::AetherAgentId;
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let one_item = store.insert_item(&item(None, "one run"), None).unwrap();
    let one = start(&mut store, &one_item.id);
    let stage = running_stage(&one, 0, 1);
    store.upsert_stage_run(&stage).unwrap();
    let many_item = store.insert_item(&item(None, "retried"), None).unwrap();
    let old = start(&mut store, &many_item.id);
    settle_run(
        &mut store,
        &old,
        QueueState::Cancelled { reason: None },
        t(20),
    );
    store
        .transition_item(&many_item.id, QueueState::Queued, t(21))
        .unwrap();
    let new = start(&mut store, &many_item.id);
    let child = AetherAgentId::parse("agt_single_child").unwrap();
    let ambiguous = AetherAgentId::parse("agt_ambiguous_child").unwrap();
    for (agent, item) in [(&child, &one_item), (&ambiguous, &many_item)] {
        store
            .record_spend(&SpendRow {
                agent: agent.clone(),
                item: Some(item.id.clone()),
                cost_usd: 0.5,
                cost_basis: archon_core::cost::CostBasis::Unknown,
                cost_complete: None,
                estimated_cost_usd: 0.125,
                input_tokens: 1,
                output_tokens: 1,
                at: t(30),
            })
            .unwrap();
    }
    store
        .kv_set("link_child_agt_no_usage", &format!("\"{}\"", one_item.id))
        .unwrap();
    store.kv_set("unrelated", "not JSON").unwrap();
    drop(store);
    // Recreate a pre-V6 store using its unchanged public records.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE agent_runs;
         ALTER TABLE spend_daily DROP COLUMN billed_cost_usd;
         ALTER TABLE spend_daily DROP COLUMN list_price_cost_usd;
         ALTER TABLE spend_daily DROP COLUMN unclassified_cost_usd;
         DROP TABLE delegated_agents;
         ALTER TABLE spend DROP COLUMN settled_cost_usd;
         DELETE FROM schema_migrations WHERE version >= 6;",
    )
    .unwrap();
    drop(conn);
    for _ in 0..2 {
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.agent_run(&child).unwrap(), Some(one.id.clone()));
        assert_eq!(
            store
                .agent_run(stage.aether_agent.as_ref().unwrap())
                .unwrap(),
            Some(one.id.clone())
        );
        assert_eq!(
            store
                .agent_run(&AetherAgentId::parse("agt_no_usage").unwrap())
                .unwrap(),
            Some(one.id.clone())
        );
        assert_eq!(store.agent_run(&ambiguous).unwrap(), None);
        assert_eq!(store.spend_for_run(&one.id, true).unwrap(), 0.625);
        assert_eq!(store.spend_for_run(&new.id, true).unwrap(), 0.0);
        assert_eq!(store.spend_for_item(&many_item.id, true).unwrap(), 0.625);
        let error = store.link_agent_run(&child, &new.id).unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)));
        let foreign_stage = running_stage(&new, 0, 99);
        store
            .link_agent_run(foreign_stage.aether_agent.as_ref().unwrap(), &one.id)
            .unwrap();
        let error = store.upsert_stage_run(&foreign_stage).unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)));
    }
}
