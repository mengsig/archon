//! Tracked requests (claim, outcome, restart, retention) and the reader
//! connections archond serves reads on while its writer holds the lock.

mod common;

use std::time::{Duration, Instant};

use archon_core::id::RequestId;
use archon_core::protocol::{ErrorCode, ErrorReply, Reply};
use archon_core::tracked::TrackedState;
use archon_queue::store::{Claim, TrackedRequest};
use archon_queue::{Checkpointer, Store};
use common::*;

fn tracked(key: Option<&str>, request: &str) -> TrackedRequest {
    TrackedRequest {
        id: RequestId::new(),
        caller: "agt_1".into(),
        command: "refresh_item".into(),
        idempotency_key: key.map(str::to_owned),
        request: request.into(),
        started_at: t(10),
    }
}

#[test]
fn a_keyed_request_is_pending_until_it_ends_and_its_key_finds_it_after() {
    let mut store = store();
    let first = tracked(Some("k1"), r#"{"cmd":"refresh_item"}"#);
    assert_eq!(store.claim_tracked(&first).unwrap(), Claim::New);
    let (caller, pending) = store.tracked_status(&first.id).unwrap().unwrap();
    assert_eq!(caller, "agt_1");
    assert_eq!(pending.state, TrackedState::Pending);

    let repeat = tracked(Some("k1"), r#"{"cmd":"other"}"#);
    let Claim::Earlier { request, status } = store.claim_tracked(&repeat).unwrap() else {
        panic!("the key was used before");
    };
    assert_eq!(
        request, first.request,
        "the first request is what the key holds"
    );
    assert_eq!(status.request, first.id);
    assert!(
        store.tracked_status(&repeat.id).unwrap().is_none(),
        "the repeat is not recorded"
    );

    store.finish_tracked(&first, Ok(&Reply::Ok), t(11)).unwrap();
    let Claim::Earlier { status, .. } = store.claim_tracked(&repeat).unwrap() else {
        panic!("the key still holds the first request");
    };
    assert_eq!(status.state, TrackedState::Applied);
    assert_eq!(status.reply.as_deref(), Some(&Reply::Ok));
    assert_eq!(status.finished_at, Some(t(11)));
    let again = store.finish_tracked(&first, Ok(&Reply::Ok), t(12));
    assert!(again.unwrap_err().to_string().contains("was not pending"));
}

#[test]
fn keys_are_scoped_to_their_caller_and_command() {
    let mut store = store();
    assert_eq!(
        store.claim_tracked(&tracked(Some("k"), "{}")).unwrap(),
        Claim::New
    );
    let other_caller = TrackedRequest {
        caller: "agt_2".into(),
        ..tracked(Some("k"), "{}")
    };
    assert_eq!(store.claim_tracked(&other_caller).unwrap(), Claim::New);
    let other_command = TrackedRequest {
        command: "notify".into(),
        ..tracked(Some("k"), "{}")
    };
    assert_eq!(store.claim_tracked(&other_command).unwrap(), Claim::New);
}

#[test]
fn an_unkeyed_request_is_recorded_once_it_ends_with_its_error() {
    let mut store = store();
    let request = tracked(None, "{}");
    let refused = ErrorReply {
        code: ErrorCode::InvalidArgument,
        message: "no such item".into(),
    };
    store
        .finish_tracked(&request, Err(&refused), t(12))
        .unwrap();
    let (_, status) = store.tracked_status(&request.id).unwrap().unwrap();
    assert_eq!(status.state, TrackedState::Failed);
    assert_eq!(status.error, Some(refused));
    assert!(status.reply.is_none() && status.idempotency_key.is_none());
}

#[test]
fn a_restart_interrupts_pending_requests_and_retention_prunes_only_finished_ones() {
    let mut store = store();
    let pending = tracked(Some("left"), "{}");
    store.claim_tracked(&pending).unwrap();
    let old = tracked(None, "{}");
    store.finish_tracked(&old, Ok(&Reply::Ok), t(11)).unwrap();
    let young = TrackedRequest {
        started_at: t(100),
        ..tracked(None, "{}")
    };
    store
        .finish_tracked(&young, Ok(&Reply::Ok), t(101))
        .unwrap();

    assert_eq!(
        store.prune_tracked(t(50)).unwrap(),
        1,
        "pending stays, the young one stays"
    );
    assert!(store.tracked_status(&old.id).unwrap().is_none());
    assert_eq!(store.interrupt_tracked(t(200)).unwrap(), 1);
    let (_, status) = store.tracked_status(&pending.id).unwrap().unwrap();
    assert_eq!(status.state, TrackedState::Interrupted);
    assert_eq!(status.finished_at, Some(t(200)));
    assert_eq!(
        store.prune_tracked(t(50)).unwrap(),
        1,
        "an interrupted request ages out"
    );
    assert!(store.tracked_status(&young.id).unwrap().is_some());
}

/// The writer's lock held by an open write transaction, as a long commit holds it.
fn hold_write_lock(path: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "BEGIN IMMEDIATE; INSERT INTO kv (key, value) VALUES ('held', 'uncommitted');",
    )
    .unwrap();
    conn
}

#[test]
fn a_reader_answers_at_once_while_a_writer_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    writer.insert_repo(&repo("alpha")).unwrap();
    let reader = Store::open_reader(&path).unwrap();
    let held = hold_write_lock(&path);

    let started = Instant::now();
    assert_eq!(
        reader.repos().unwrap().len(),
        1,
        "the reader sees the last commit"
    );
    assert_eq!(
        reader.kv_get("held").unwrap(),
        None,
        "and nothing uncommitted"
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "the reader waited {took:?}");
    let refused = reader.kv_get("held").and_then(|_| {
        let mut reader = Store::open_reader(&path)?;
        reader.kv_set("x", "y")
    });
    assert!(refused.is_err(), "a reader cannot write");

    held.execute_batch("COMMIT").unwrap();
    assert_eq!(
        reader.kv_get("held").unwrap().as_deref(),
        Some("uncommitted")
    );
    drop(writer);
}

#[test]
fn a_checkpoint_runs_beside_an_open_write_and_copies_what_is_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    writer.insert_repo(&repo("alpha")).unwrap();
    let checkpointer = Checkpointer::open(&path).unwrap();
    let held = hold_write_lock(&path);

    let started = Instant::now();
    let (log, done) = checkpointer.checkpoint().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a passive checkpoint never waits"
    );
    assert!(done <= log);
    held.execute_batch("ROLLBACK").unwrap();
    let (log, done) = checkpointer.checkpoint().unwrap();
    assert_eq!(done, log, "with nothing open, the whole WAL is copied");
}

#[test]
fn a_reader_refuses_a_store_that_is_not_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    drop(Store::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    drop(conn);
    let refused = Store::open_reader(&path).unwrap_err().to_string();
    assert!(refused.contains("WAL is required"), "{refused}");
}
