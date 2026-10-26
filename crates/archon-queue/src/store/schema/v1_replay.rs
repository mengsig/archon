//! Keyed enqueues written by a V1 store, retried after the autonomy
//! migrations: the original request returns its item, another is refused.

use std::path::Path;

use archon_core::autonomy::{Autonomy, AutonomyPatch, MergeMode, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model::ModelPlan;
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use chrono::TimeZone;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::V1;
use crate::Store;
use crate::store::Asked;

/// Long before any V2 migration of this test's store.
fn v1_time() -> archon_core::Timestamp {
    chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap()
}

fn repo(name: &str) -> Repo {
    Repo {
        id: RepoId::new(),
        name: name.into(),
        path: format!("/src/{name}").into(),
        ship: ShipMode::Pr,
        base_branch: None,
        default_pipeline: None,
        autonomy_max: AutonomyPatch::default(),
        grant_merge: false,
        merge_waive_checks: Vec::new(),
        attribution: Default::default(),
        gh_user: None,
        tiers: Default::default(),
        confirmed_fast_check: None,
        confirmed_full_check: None,
        confirmed_release_check: None,
        added_at: v1_time(),
    }
}

fn item(repo: &Repo, pipeline: &str) -> QueueItem {
    QueueItem {
        onto: None,
        id: QueueItemId::new(),
        repo: Some(repo.id.clone()),
        pipeline: pipeline.into(),
        title: format!("{pipeline} it"),
        brief: "as before".into(),
        priority: Priority::High,
        dependencies: vec![],
        tags: vec!["legacy".into()],
        model_plan: ModelPlan::default(),
        model_reasons: None,
        budget_usd: Some(1.5),
        autonomy: Autonomy::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        risk: None,
        state: QueueState::Queued,
        created_at: v1_time(),
        updated_at: v1_time(),
    }
}

/// Serialized as V1 wrote it: no knobs anywhere, `auto_merge` on the repo.
fn without(value: impl serde::Serialize, key: &str) -> Value {
    let mut value = serde_json::to_value(value).unwrap();
    assert!(value.as_object_mut().unwrap().remove(key).is_some());
    value
}

fn write_v1_repo(conn: &Connection, repo: &Repo, auto_merge: bool) {
    let mut data = without(repo, "autonomy_max");
    data["auto_merge"] = json!(auto_merge);
    conn.execute(
        "INSERT INTO repos (id, name, path, added_at, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            repo.id.as_str(),
            repo.name,
            repo.path.to_str().unwrap(),
            super::ts(&repo.added_at),
            data.to_string()
        ],
    )
    .unwrap();
}

fn write_v1_item(conn: &Connection, item: &QueueItem, key: &str) {
    let request = json!({
        "repo": item.repo, "pipeline": item.pipeline, "title": item.title,
        "brief": item.brief, "priority": item.priority,
        "dependencies": item.dependencies, "tags": item.tags,
        "model_plan": item.model_plan, "budget_usd": item.budget_usd,
    });
    let created = super::ts(&item.created_at);
    conn.execute(
        "INSERT INTO items (id, repo, pipeline, priority, state, tags, idempotency_key,
                            request, created_at, updated_at, data)
         VALUES (?1, ?2, ?3, 2, 'queued', '[\"legacy\"]', ?4, ?5, ?6, ?6, ?7)",
        params![
            item.id.as_str(),
            item.repo.as_ref().map(|r| r.as_str()),
            item.pipeline,
            key,
            request.to_string(),
            created,
            without(item, "autonomy").to_string(),
        ],
    )
    .unwrap();
}

/// A V1 store at `path` holding `ask` in a plain repository and `quick-pr`
/// in an auto-merge one, keyed by their pipeline names.
fn v1_store(path: &Path) -> Vec<QueueItem> {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
         INSERT INTO schema_migrations VALUES (1, '2023-01-01T00:00:00.000000000Z');",
    )
    .unwrap();
    conn.execute_batch(V1).unwrap();
    let (plain, auto) = (repo("plain"), repo("auto"));
    write_v1_repo(&conn, &plain, false);
    write_v1_repo(&conn, &auto, true);
    let items = vec![item(&plain, "ask"), item(&auto, "quick-pr")];
    for item in &items {
        write_v1_item(&conn, item, &item.pipeline);
    }
    items
}

/// What a current enqueue of the same request resolves to: `review=none`
/// for both pipelines, and `merge=manual` without a configured default.
fn retry(original: &QueueItem) -> QueueItem {
    QueueItem {
        id: QueueItemId::new(),
        autonomy: Autonomy {
            review: ReviewLevel::None,
            ..Autonomy::default()
        },
        created_at: archon_core::now(),
        updated_at: archon_core::now(),
        ..original.clone()
    }
}

fn refused(result: Result<QueueItem, ArchonError>) -> String {
    match result {
        Err(ArchonError::Invalid(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_v1_enqueue_retried_after_the_migrations_returns_its_item() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let originals = v1_store(&path);
    let mut store = Store::open(&path).unwrap();
    let none = Asked::default();
    for original in &originals {
        let key = original.pipeline.as_str();
        let again = store.insert_enqueued(&retry(original), Some(key), &none);
        let again = again.unwrap_or_else(|e| panic!("{key}: {e}"));
        assert_eq!(again.id, original.id, "{key}");
    }
    let quick = store.item(&originals[1].id).unwrap();
    assert_eq!(quick.autonomy.merge, MergeMode::Auto, "V1 auto-merge kept");
    assert_eq!(store.items(&Default::default()).unwrap().len(), 2);
}

#[test]
fn a_v1_key_still_refuses_a_different_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let originals = v1_store(&path);
    let mut store = Store::open(&path).unwrap();
    let ask = &originals[0];
    let none = Asked::default();

    let mut other = retry(ask);
    other.title = "something else".into();
    let message = refused(store.insert_enqueued(&other, Some("ask"), &none));
    assert!(message.contains("different request"), "{message}");

    let asked = Asked {
        autonomy: AutonomyPatch {
            review: Some(ReviewLevel::None),
            ..AutonomyPatch::default()
        },
        ..Asked::default()
    };
    let message = refused(store.insert_enqueued(&retry(ask), Some("ask"), &asked));
    assert!(message.contains("before autonomy knobs"), "{message}");
    let message = refused(store.insert_item(&retry(ask), Some("ask")));
    assert!(message.contains("before autonomy knobs"), "{message}");
    assert_eq!(store.items(&Default::default()).unwrap().len(), 2);
}
