//! The change stream (CONTRACTS.md §1.3): replaying it reproduces the
//! store, its seqs never skip, retention refuses expired cursors, and a
//! snapshot's `as_of` lets the §5.3 client resync lose nothing.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    Change, ChangeSubject, ItemQuery, QueueStateKind, RunStateKind, TransitionActor,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageState, Verdict};
use archon_queue::Store;
use archon_queue::store::{CHANGES_KEEP_DAYS, CHANGES_KEEP_ROWS};
use common::*;

/// Every change after `after`, read a page at a time.
fn all_changes(store: &Store, after: u64) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    loop {
        let cursor = out.last().map_or(after, |c| c.seq);
        let page = store.changes_after(cursor, 257).unwrap();
        let done = page.len() < 257;
        out.extend(page);
        if done {
            return out;
        }
    }
}

fn all_items(store: &Store) -> Vec<QueueItem> {
    let query = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    store.items(&query).unwrap()
}

/// What a client that applied `changes` knows: each item's state kind and
/// the dependency edges.
#[derive(Debug, Default, PartialEq)]
struct Replayed {
    states: BTreeMap<QueueItemId, QueueStateKind>,
    edges: BTreeSet<(QueueItemId, QueueItemId)>,
}

impl Replayed {
    fn apply(&mut self, change: &Change) {
        match &change.subject {
            ChangeSubject::Item { item, state, .. } => {
                self.states.insert(item.clone(), *state);
            }
            ChangeSubject::Dependency {
                item,
                depends_on,
                added,
            } => {
                let edge = (item.clone(), depends_on.clone());
                assert_eq!(self.edges.contains(&edge), !added, "{change:?}");
                if *added {
                    self.edges.insert(edge);
                } else {
                    self.edges.remove(&edge);
                }
            }
            _ => {}
        }
    }

    fn of_store(store: &Store) -> Self {
        let mut replayed = Self::default();
        for item in all_items(store) {
            for dep in &item.dependencies {
                replayed.edges.insert((item.id.clone(), dep.clone()));
            }
            replayed
                .states
                .insert(item.id.clone(), QueueStateKind::from(&item.state));
        }
        replayed
    }
}

fn replay(changes: &[Change]) -> Replayed {
    let mut replayed = Replayed::default();
    for change in changes {
        replayed.apply(change);
    }
    replayed
}

fn assert_gapless(changes: &[Change], first: u64) {
    for (offset, change) in changes.iter().enumerate() {
        assert_eq!(change.seq, first + offset as u64, "seqs skip at {change:?}");
    }
}

fn pick<'a>(rng: &mut Rng, items: &'a [QueueItem]) -> Option<&'a QueueItem> {
    (!items.is_empty()).then(|| &items[rng.below(items.len())])
}

fn simple_state(rng: &mut Rng) -> QueueState {
    match rng.below(5) {
        0 => QueueState::Queued,
        1 => QueueState::Dispatching,
        2 => QueueState::Blocked {
            reason: "waits".into(),
        },
        3 => QueueState::Cancelled { reason: None },
        _ => QueueState::Failed {
            run: None,
            error: "broke".into(),
        },
    }
}

/// One random store operation. Refused operations are part of the test: a
/// refusal must record nothing.
fn step(store: &mut Store, repo: &archon_core::repo::Repo, rng: &mut Rng, n: i64) {
    let items = all_items(store);
    let at = t(100 + n);
    match rng.below(7) {
        0 => {
            let mut new = item(Some(repo), &format!("item {n}"));
            if let Some(dep) = pick(rng, &items) {
                new.dependencies.push(dep.id.clone());
            }
            store.insert_item(&new, None).unwrap();
        }
        1 => {
            if let Some(target) = pick(rng, &items) {
                let _ = store.transition_item(&target.id, simple_state(rng), at);
            }
        }
        2 => {
            let dispatching: Vec<_> = items
                .iter()
                .filter(|i| i.state == QueueState::Dispatching)
                .cloned()
                .collect();
            if let Some(target) = pick(rng, &dispatching) {
                let run = run_for(target);
                store.insert_run(&run).unwrap();
                let running = QueueState::Running { run: run.id };
                store.transition_item(&target.id, running, at).unwrap();
            }
        }
        3 => {
            let runs = store.active_runs().unwrap();
            if !runs.is_empty() {
                let run = &runs[rng.below(runs.len())];
                let next = match rng.below(3) {
                    0 => QueueState::Succeeded {
                        run: run.id.clone(),
                        summary: "done".into(),
                    },
                    1 => QueueState::Failed {
                        run: Some(run.id.clone()),
                        error: "broke".into(),
                    },
                    _ => QueueState::Cancelled { reason: None },
                };
                settle_run(store, run, next, at);
            }
        }
        4 => {
            store.requeue_orphaned_dispatching(at).unwrap();
        }
        5 => {
            let runs = store.active_runs().unwrap();
            if !runs.is_empty() {
                let run = &runs[rng.below(runs.len())];
                store
                    .upsert_stage_run(&running_stage(run, 0, n as u32))
                    .unwrap();
            }
        }
        _ => {
            let (Some(target), Some(dep)) = (pick(rng, &items), pick(rng, &items)) else {
                return;
            };
            let mut changed = target.clone();
            if changed.dependencies.contains(&dep.id) {
                changed.dependencies.retain(|d| d != &dep.id);
            } else {
                changed.dependencies.push(dep.id.clone());
            }
            // A cycle or self-dependency is refused, and records nothing.
            let _ = store.update_item(&changed);
        }
    }
}

#[test]
fn replaying_the_changes_reproduces_every_item_and_edge() {
    for seed in 1..=24 {
        let mut rng = Rng::new(seed * 7919);
        let (mut store, repo, _) = seeded(2);
        for n in 0..90 {
            step(&mut store, &repo, &mut rng, n);
        }
        let changes = all_changes(&store, 0);
        assert_gapless(&changes, 1);
        assert_eq!(
            changes.last().map_or(0, |c| c.seq),
            store.last_change_seq().unwrap()
        );
        assert_eq!(replay(&changes), Replayed::of_store(&store), "seed {seed}");
        // A client resuming from any cursor reaches the same state.
        let cut = rng.below(changes.len()) as u64;
        let mut resumed = replay(&changes[..cut as usize]);
        for change in all_changes(&store, cut) {
            resumed.apply(&change);
        }
        assert_eq!(
            resumed,
            Replayed::of_store(&store),
            "seed {seed} from {cut}"
        );
    }
}

#[test]
fn runs_stages_and_questions_are_told_with_their_items() {
    let (mut store, _, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let mut stage = running_stage(&run, 0, 1);
    store.upsert_stage_run(&stage).unwrap();
    // A write that keeps the kind and the verdict tells nothing new.
    let before = store.last_change_seq().unwrap();
    store.upsert_stage_run(&stage).unwrap();
    assert_eq!(store.last_change_seq().unwrap(), before);
    stage.state = StageState::Completed;
    stage.verdict = Some(Verdict::Pass);
    stage.finished_at = Some(t(30));
    store.upsert_stage_run(&stage).unwrap();
    let changes = all_changes(&store, before);
    let [stage_change] = changes.as_slice() else {
        panic!("one change for the finished stage: {changes:?}");
    };
    assert!(matches!(
        &stage_change.subject,
        ChangeSubject::Stage { item, verdict: Some(Verdict::Pass), .. } if item == &items[0].id
    ));
    let runs: Vec<_> = all_changes(&store, 0)
        .into_iter()
        .filter_map(|c| match c.subject {
            ChangeSubject::Run { run: id, state, .. } if id == run.id => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(runs, vec![RunStateKind::Running]);
}

#[test]
fn item_history_lists_each_transition_oldest_first() {
    let (mut store, _, items) = seeded(1);
    let id = &items[0].id;
    let run = start(&mut store, id);
    let failed = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "tests failed".into(),
    };
    settle_run(&mut store, &run, failed, t(40));
    archon_queue::store::acting_as(TransitionActor::Web, || {
        store.transition_item(id, QueueState::Queued, t(41))
    })
    .unwrap();
    let history = store.item_history(id, 1000).unwrap();
    let steps: Vec<_> = history.iter().map(|h| (h.from, h.to)).collect();
    use QueueStateKind as K;
    assert_eq!(
        steps,
        vec![
            (None, K::Queued),
            (Some(K::Queued), K::Dispatching),
            (Some(K::Dispatching), K::Running),
            (Some(K::Running), K::Failed),
            (Some(K::Failed), K::Queued),
        ]
    );
    assert_eq!(history[3].reason.as_deref(), Some("tests failed"));
    let by: Vec<_> = history.iter().map(|h| h.by).collect();
    use TransitionActor as A;
    assert_eq!(by, [A::Daemon, A::Daemon, A::Daemon, A::Daemon, A::Web]);
    let after = store.transition_item(id, QueueState::Cancelled { reason: None }, t(42));
    let after = after.unwrap();
    let last = store.item_history(&after.id, 1).unwrap();
    assert_eq!(last[0].by, A::Daemon, "the attribution ends with its scope");
    assert!(history.windows(2).all(|w| w[0].seq < w[1].seq));
    let newest = store.item_history(id, 3).unwrap();
    assert_eq!(newest[..2], history[3..], "a limit keeps the newest");
    let missing = store.item_history(&QueueItemId::new(), 10);
    assert!(matches!(missing, Err(ArchonError::NotFound(_))));
}

fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("archon.db")
}

/// Rows straight into `changes`, `at` given, in one transaction.
fn insert_raw_changes(path: &Path, rows: &[(u64, &str)]) {
    let mut conn = rusqlite::Connection::open(path).unwrap();
    let tx = conn.transaction().unwrap();
    {
        let mut stmt = tx
            .prepare("INSERT INTO changes (at, data) VALUES (?1, ?2)")
            .unwrap();
        let data = format!(
            r#"{{"subject":"item","item":"{}","state":"queued"}}"#,
            QueueItemId::new()
        );
        for (count, at) in rows {
            for _ in 0..*count {
                stmt.execute(rusqlite::params![at, data]).unwrap();
            }
        }
    }
    tx.commit().unwrap();
}

#[test]
fn retention_keeps_thirty_days_or_the_newest_rows_and_expires_older_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(Store::open(&path).unwrap());
    let now = archon_core::now();
    let old = (now - chrono::Duration::days(CHANGES_KEEP_DAYS + 1))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let recent = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    insert_raw_changes(&path, &[(CHANGES_KEEP_ROWS + 10, &old), (5, &recent)]);
    let mut store = Store::open(&path).unwrap();
    let last = CHANGES_KEEP_ROWS + 15;
    assert_eq!(store.last_change_seq().unwrap(), last);

    assert_eq!(store.prune_changes(now, 4).unwrap(), 4, "a batch at a time");
    assert_eq!(store.prune_changes(now, 1_000_000).unwrap(), 11);
    assert_eq!(
        store.prune_changes(now, 1_000_000).unwrap(),
        0,
        "the newest rows stay"
    );

    for cursor in [0, 14] {
        let Err(ArchonError::HistoryExpired { kept, .. }) = store.changes_after(cursor, 10) else {
            panic!("cursor {cursor} is older than retention");
        };
        assert_eq!((kept.first_seq, kept.last_seq), (16, last));
        assert!(store.check_change_cursor(cursor).is_err());
    }
    let resumed = store.changes_after(15, 10).unwrap();
    assert_eq!(resumed.first().map(|c| c.seq), Some(16));
    assert!(
        store.changes_after(last + 1, 10).is_err(),
        "a cursor of another store"
    );
    assert!(store.changes_after(last, 10).unwrap().is_empty());
}

#[test]
fn retention_keeps_every_change_younger_than_thirty_days() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(Store::open(&path).unwrap());
    let now = archon_core::now();
    let recent = (now - chrono::Duration::days(CHANGES_KEEP_DAYS - 1))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    insert_raw_changes(&path, &[(CHANGES_KEEP_ROWS + 20, &recent)]);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.prune_changes(now, 1_000_000).unwrap(), 0);
    assert_eq!(store.changes_after(0, 1).unwrap()[0].seq, 1);
}

/// The client of CONTRACTS.md §5.3: subscribe first and buffer, read the
/// snapshot with its `as_of`, then apply the buffered changes after it. A
/// change committed while the snapshot is open is in the buffer, not in the
/// snapshot, and `seq > as_of` keeps it.
#[test]
fn a_change_committed_during_the_snapshot_is_not_lost_on_resync() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut writer = Store::open(&path).unwrap();
    let repo = repo("alpha");
    writer.insert_repo(&repo).unwrap();
    let a = writer.insert_item(&item(Some(&repo), "a"), None).unwrap();
    let b = writer.insert_item(&item(Some(&repo), "b"), None).unwrap();
    let reader = writer.open_reader(&path).unwrap();

    let subscribed_at = writer.last_change_seq().unwrap();
    reader.begin_snapshot().unwrap();
    let as_of = reader.last_change_seq().unwrap();
    // Committed between the snapshot's first read and its reply.
    writer
        .transition_item(&a.id, QueueState::Dispatching, t(5))
        .unwrap();
    let snapshot = Replayed::of_store(&reader);
    assert_eq!(reader.last_change_seq().unwrap(), as_of, "one snapshot");
    reader.end_snapshot().unwrap();
    writer
        .transition_item(&b.id, QueueState::Cancelled { reason: None }, t(6))
        .unwrap();

    assert_eq!(
        snapshot.states[&a.id],
        QueueStateKind::Queued,
        "not in the snapshot"
    );
    let buffered = all_changes(&writer, subscribed_at);
    let mut client = snapshot;
    for change in buffered.iter().filter(|c| c.seq > as_of) {
        client.apply(change);
    }
    assert_eq!(client, Replayed::of_store(&writer), "nothing lost");

    // A watermark read after the snapshot, outside its transaction, would
    // skip the change committed during it.
    let late = as_of + 1;
    assert!(buffered.iter().any(|c| c.seq == late
        && matches!(&c.subject, ChangeSubject::Item { item, .. } if item == &a.id)));
}

const CRASH_DB: &str = "ARCHON_TEST_CHANGES_CRASH_DB";

/// Run only by the crash test: writes changes and an item state in one
/// transaction, spilling its pages to the WAL, and dies before committing.
#[test]
#[ignore = "the crash test runs this in a child process"]
fn crash_child_dies_between_write_and_commit() {
    let Some(path) = std::env::var_os(CRASH_DB) else {
        return;
    };
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA cache_size = 1; BEGIN IMMEDIATE;")
        .unwrap();
    let data = format!(
        r#"{{"subject":"item","item":"{}","state":"running"}}"#,
        "x".repeat(4000)
    );
    for _ in 0..200 {
        conn.execute(
            "INSERT INTO changes (at, data) VALUES ('2030-01-01T00:00:00.000000000Z', ?1)",
            [&data],
        )
        .unwrap();
    }
    conn.execute_batch("UPDATE items SET state = 'running'")
        .unwrap();
    std::process::abort();
}

#[test]
fn seqs_have_no_gap_after_a_crash_between_write_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let a = store.insert_item(&item(Some(&repo), "a"), None).unwrap();
    let before = store.last_change_seq().unwrap();
    drop(store);

    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_child_dies_between_write_and_commit",
            "--ignored",
        ])
        .args(["--test-threads", "1", "--quiet"])
        .env(CRASH_DB, &path)
        .output()
        .unwrap();
    assert!(
        !child.status.success(),
        "the child must die mid-transaction"
    );

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.last_change_seq().unwrap(),
        before,
        "nothing uncommitted is kept"
    );
    assert_eq!(store.item(&a.id).unwrap().state, QueueState::Queued);
    store
        .transition_item(&a.id, QueueState::Dispatching, t(5))
        .unwrap();
    let changes = all_changes(&store, 0);
    assert_gapless(&changes, 1);
    assert_eq!(changes.last().unwrap().seq, before + 1);
    assert_eq!(replay(&changes), Replayed::of_store(&store));
}

#[test]
fn a_refused_write_records_no_change() {
    let (mut store, _, items) = seeded(1);
    let before = store.last_change_seq().unwrap();
    let refused = store.transition_item(&items[0].id, QueueState::Queued, t(5));
    assert!(refused.is_err(), "queued to queued is no edge");
    let cyclic = QueueItem {
        dependencies: vec![items[0].id.clone()],
        ..items[0].clone()
    };
    assert!(store.update_item(&cyclic).is_err());
    assert_eq!(store.last_change_seq().unwrap(), before);
    let next = store
        .transition_item(&items[0].id, QueueState::Dispatching, t(6))
        .unwrap();
    assert_eq!(next.state, QueueState::Dispatching);
    assert_eq!(store.last_change_seq().unwrap(), before + 1);
}

/// V19 tells each item that predates it once, so a replay from 0 reaches it.
#[test]
fn the_migration_gives_older_items_one_change_each() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let a = store.insert_item(&item(Some(&repo), "a"), None).unwrap();
    store.insert_item(&item(Some(&repo), "b"), None).unwrap();
    store
        .transition_item(&a.id, QueueState::Dispatching, t(5))
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE item_transitions; DROP TABLE changes;
         DELETE FROM schema_migrations WHERE version = 19;",
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&path).unwrap();
    let changes = all_changes(&store, 0);
    assert_eq!(changes.len(), 2, "one per item, no dependency changes");
    assert_gapless(&changes, 1);
    let mut states = replay(&changes);
    states.edges = Replayed::of_store(&store).edges;
    assert_eq!(states, Replayed::of_store(&store));
    assert_eq!(states.states[&a.id], QueueStateKind::Dispatching);
}

/// Writes that record no change still move the writer's count, so a
/// snapshot paired with a view read before it can tell it is stale.
#[test]
fn every_write_moves_the_write_count_even_without_a_change() {
    let (mut store, _, items) = seeded(1);
    let mut item = items[0].clone();
    let seq = store.last_change_seq().unwrap();
    let count = store.write_count();
    item.priority = archon_core::queue::Priority::Critical;
    store.update_item(&item).unwrap();
    assert_eq!(store.last_change_seq().unwrap(), seq, "no change recorded");
    let edited = store.write_count();
    assert!(edited > count, "a priority edit is a write");
    store.kv_set("a key", "a value").unwrap();
    assert!(store.write_count() > edited, "a setting is a write");
    let written = store.write_count();
    store.item(&item.id).unwrap();
    assert_eq!(store.write_count(), written, "a read is not");
}
