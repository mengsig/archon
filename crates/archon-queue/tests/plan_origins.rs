//! Plan origins at the store's boundary: a save stores origins no writer
//! accounted for as unattributed, and the count finds every such field.

mod common;

use archon_core::id::QueueItemId;
use archon_core::plan_origin::{PlanSource, PlanWriter};
use archon_core::queue::QueueItem;
use archon_queue::store::Store;
use common::{item, repo, t};

/// `title` with its plan filled, as archond records it.
fn planned(repo: &archon_core::repo::Repo, title: &str) -> QueueItem {
    let mut made = item(Some(repo), title);
    let mut writer = PlanWriter::new(&made.id, t(1));
    let plan = made.model_plan.plan().clone();
    writer.filled(plan, (PlanSource::Fallback, PlanSource::Fallback));
    made.model_plan = writer.finish();
    assert_eq!(made.model_plan.unattributed_fields(&made.id), 0);
    made
}

fn fields(item: &QueueItem) -> usize {
    item.model_plan
        .0
        .values()
        .map(|entry| 2 * entry.len())
        .sum()
}

#[test]
fn the_count_finds_stored_origins_of_another_item_or_another_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let mut store = Store::open(&path).unwrap();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let stored: Vec<QueueItem> = ["sound", "foreign", "misshapen"]
        .into_iter()
        .map(|title| store.insert_item(&planned(&alpha, title), None).unwrap())
        .collect();
    assert_eq!(store.unattributed_plan_fields().unwrap(), 0);
    // Rows no save reconciled, as another writer of the database leaves them.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let foreign =
        "UPDATE items SET data = json_set(data, '$.model_origins.item', ?2) WHERE id = ?1";
    let ids = [stored[1].id.as_str(), stored[0].id.as_str()];
    assert_eq!(conn.execute(foreign, ids).unwrap(), 1);
    let misshapen = "UPDATE items SET data =
        json_set(data, '$.model_origins.entries.implement', json('[]')) WHERE id = ?1";
    assert_eq!(conn.execute(misshapen, [stored[2].id.as_str()]).unwrap(), 1);
    let expected = fields(&stored[1]) + fields(&stored[2]);
    assert!(expected > 0);
    assert_eq!(store.unattributed_plan_fields().unwrap(), expected);
    let read = store.item(&stored[1].id).unwrap();
    assert_eq!(read.model_plan.unattributed_fields(&read.id), fields(&read));
}

#[test]
fn the_count_finds_a_stored_field_without_an_origin_or_with_an_unknown_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let mut store = Store::open(&path).unwrap();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let edits = [
        "json_remove(data, '$.model_origins.entries.implement[0].effort')",
        "json_remove(data, '$.model_origins.entries.implement[0].model.source')",
        "json_set(data, '$.model_origins.entries.implement[0].model.source', 'comparison')",
        "json_set(data, '$.model_origins', json_array(
            json_extract(data, '$.model_origins.item'),
            json_set(json_extract(data, '$.model_origins.entries'),
                     '$.implement[0].model.source', 'future')))",
    ];
    let conn = rusqlite::Connection::open(&path).unwrap();
    for (n, edit) in edits.iter().enumerate() {
        let stored = store
            .insert_item(&planned(&alpha, &format!("edited {n}")), None)
            .unwrap();
        let update = format!("UPDATE items SET data = {edit} WHERE id = ?1");
        assert_eq!(conn.execute(&update, [stored.id.as_str()]).unwrap(), 1);
        let read = store.item(&stored.id).unwrap();
        assert_eq!(read.model_plan.unattributed_fields(&read.id), 1, "{edit}");
        assert_eq!(store.unattributed_plan_fields().unwrap(), n + 1, "{edit}");
    }
}

/// A test build fails loudly; a release build stores the fields as unattributed.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "origins written for"))]
fn a_save_stores_origins_kept_from_another_item_as_unattributed() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("store.db")).unwrap();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let first = store.insert_item(&planned(&alpha, "first"), None).unwrap();
    let clone = QueueItem {
        id: QueueItemId::new(),
        ..first.clone()
    };
    let saved = store.insert_item(&clone, None).unwrap();
    let read = store.item(&saved.id).unwrap();
    assert!(
        read.model_plan.faults(&read.id).is_empty(),
        "stored reconciled"
    );
    assert_eq!(store.unattributed_plan_fields().unwrap(), fields(&first));
}
