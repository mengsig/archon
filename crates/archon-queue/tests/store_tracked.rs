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
    let reader = writer.open_reader(&path).unwrap();
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
        let mut reader = writer.open_reader(&path)?;
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
    let writer = Store::open_in_memory().unwrap();
    let refused = writer.open_reader(&path).unwrap_err().to_string();
    assert!(refused.contains("WAL is required"), "{refused}");
}

#[test]
fn a_reader_shares_its_writer_s_learning_config_and_model_catalog() {
    use archon_core::model::ModelRef;
    use archon_core::model_tier::{CatalogModel, ModelCatalog};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let writer = Store::open(&path).unwrap();
    let reader = writer.open_reader(&path).unwrap();
    let mut config = writer.learning_config().unwrap();
    config.enabled = !config.enabled;
    writer.configure_learning(&config).unwrap();
    let model = ModelRef {
        provider: "codex".into(),
        model: "gpt".into(),
    };
    writer.set_model_catalog(ModelCatalog::new(vec![CatalogModel {
        model: model.clone(),
        tier: None,
        is_default: false,
    }]));

    assert_eq!(
        reader.learning_config().unwrap(),
        config,
        "set after it opened"
    );
    assert_eq!(reader.learning_models(), vec![model]);
    assert!(!reader.learning_models_stale());
}

#[test]
fn stored_aliases_read_canonical_without_rewriting_or_changing_origins() {
    use archon_core::model_tier::{CatalogModel, ModelCatalog};
    use archon_core::plan_origin::{PlanSource, PlannedPlan};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    let mut legacy = item(None, "legacy");
    legacy
        .model_plan
        .legacy_mut()
        .0
        .get_mut("implement")
        .unwrap()[0]
        .model = "codex/alias".parse().unwrap();
    let mut unattributed = item(None, "unattributed");
    unattributed.model_plan =
        PlannedPlan::unattributed(&unattributed.id, legacy.model_plan.plan().clone());
    writer.insert_item(&legacy, Some("legacy-key")).unwrap();
    writer.insert_item(&unattributed, None).unwrap();
    let reader = writer.open_reader(&path).unwrap();
    let mut catalog = ModelCatalog::new(vec![CatalogModel {
        model: "codex/canonical".parse().unwrap(),
        tier: None,
        is_default: false,
    }]);
    catalog.add_alias(
        "codex/alias".parse().unwrap(),
        "codex/canonical".parse().unwrap(),
    );
    writer.set_model_catalog(catalog);
    for (id, source) in [
        (&legacy.id, PlanSource::LegacyUnknown),
        (&unattributed.id, PlanSource::Unattributed),
    ] {
        let loaded = reader.item(id).unwrap();
        assert_eq!(
            loaded.model_plan.0["implement"][0].model.to_string(),
            "codex/canonical"
        );
        assert_eq!(
            loaded.model_plan.origins(id)["implement"][0].model.source,
            source
        );
        assert!(loaded.model_plan.origins(id)["implement"][0].model.is_pin());
    }
    assert_eq!(
        reader
            .item_by_key("legacy-key")
            .unwrap()
            .unwrap()
            .model_plan
            .0["implement"][0]
            .model
            .to_string(),
        "codex/canonical"
    );
    assert!(
        reader
            .items(&Default::default())
            .unwrap()
            .iter()
            .all(|i| i.model_plan.0["implement"][0].model.to_string() == "codex/canonical")
    );
    assert_eq!(
        reader.items_with_id_prefix(legacy.id.as_str(), 1).unwrap()[0]
            .model_plan
            .0["implement"][0]
            .model
            .to_string(),
        "codex/canonical"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT data FROM items WHERE id = ?1",
            [legacy.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let stored: archon_core::queue::QueueItem = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored.model_plan, legacy.model_plan);
    writer.invalidate_model_catalog("offline".into());
    assert_eq!(
        reader.item(&legacy.id).unwrap().model_plan.0["implement"][0]
            .model
            .to_string(),
        "codex/alias"
    );
}
