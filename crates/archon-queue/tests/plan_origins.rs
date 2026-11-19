//! Plan origins at the store's boundary: a save stores origins no writer
//! accounted for as unattributed, and the count reads unfinished items as
//! any item read does.

mod common;

use archon_core::id::QueueItemId;
use archon_core::plan_origin::{PlanSource, PlanWriter, Sizing};
use archon_core::queue::QueueItem;
use archon_queue::store::Store;
use common::{item, repo, t};

/// `title` with its plan filled, as archond records it.
fn planned(repo: &archon_core::repo::Repo, title: &str) -> QueueItem {
    let mut made = item(Some(repo), title);
    let mut writer = PlanWriter::new(&made.id, t(1));
    let plan = made.model_plan.plan().clone();
    writer.filled(plan, (PlanSource::Fallback, PlanSource::Fallback));
    made.model_plan = writer.finish(Sizing::Off { policy_cap: None });
    assert_eq!(made.model_plan.unattributed_fields(&made.id), 0);
    made
}

/// The model and the effort of every entry; the common plans have no tier.
fn fields(item: &QueueItem) -> usize {
    let entries = item.model_plan.0.values().flatten();
    entries.inspect(|c| assert!(c.tier.is_none())).count() * 2
}

fn opened(dir: &tempfile::TempDir) -> (Store, rusqlite::Connection, archon_core::repo::Repo) {
    let path = dir.path().join("store.db");
    let mut store = Store::open(&path).unwrap();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    (store, rusqlite::Connection::open(&path).unwrap(), alpha)
}

/// S1 at the store: origins that do not decode, do not fit or are another
/// item's leave the item readable, with every field unattributed.
#[test]
fn stored_origins_that_do_not_decode_or_fit_read_the_whole_item_as_unattributed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, conn, alpha) = opened(&dir);
    let other = store.insert_item(&planned(&alpha, "other"), None).unwrap();
    let edits = [
        "json_remove(data, '$.model_origins.item')".to_owned(),
        "json_remove(data, '$.model_origins.entries')".to_owned(),
        format!("json_set(data, '$.model_origins.item', '{}')", other.id),
        "json_set(data, '$.model_origins', 'garbage')".to_owned(),
        "json_set(data, '$.model_origins.entries.implement', json('[]'))".to_owned(),
        "json_remove(data, '$.model_origins.entries.implement[0].effort')".to_owned(),
        "json_set(data, '$.model_origins.entries.implement[0].model.source', 'comparison')"
            .to_owned(),
        "json_remove(data, '$.model_origins.entries.implement[0].model.by')".to_owned(),
        "json_remove(data, '$.model_origins.entries.implement[0].model.at')".to_owned(),
    ];
    let mut expected = 0;
    for edit in &edits {
        let stored = store.insert_item(&planned(&alpha, edit), None).unwrap();
        let update = format!("UPDATE items SET data = {edit} WHERE id = ?1");
        assert_eq!(conn.execute(&update, [stored.id.as_str()]).unwrap(), 1);
        let read = store.item(&stored.id).expect("the item stays readable");
        assert_eq!(read.model_plan.plan(), stored.model_plan.plan(), "{edit}");
        for (key, slots) in read.model_plan.origins(&read.id) {
            for origin in slots {
                assert_eq!(
                    origin.model.source,
                    PlanSource::Unattributed,
                    "{edit}: {key}"
                );
                assert_eq!(
                    origin.effort.source,
                    PlanSource::Unattributed,
                    "{edit}: {key}"
                );
                assert!(origin.model.is_pin() && origin.effort.is_pin());
            }
        }
        expected += fields(&read);
        let count = store.plan_field_count().unwrap();
        assert_eq!(
            (count.unattributed, count.unreadable),
            (expected, 0),
            "{edit}"
        );
    }
    assert!(expected >= 2 * edits.len());
}

/// S5: the count reads unfinished items in Rust, legacy fields apart; a row
/// that does not decode as an item is counted as unreadable and aborts nothing.
#[test]
fn the_count_reads_unfinished_items_and_counts_a_row_it_cannot_decode() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, conn, alpha) = opened(&dir);
    let mut insert = |made: QueueItem| store.insert_item(&made, None).unwrap();
    let sound = insert(planned(&alpha, "sound"));
    let legacy = insert(item(Some(&alpha), "legacy"));
    let foreign = insert(planned(&alpha, "foreign"));
    let finished = insert(planned(&alpha, "finished"));
    let finished_corrupt = insert(planned(&alpha, "finished and corrupt"));
    let corrupt = insert(planned(&alpha, "corrupt"));
    let planless = insert(planned(&alpha, "planless"));
    let misplanned = insert(planned(&alpha, "misplanned"));
    let binary = insert(planned(&alpha, "binary"));
    let misfiled = insert(planned(&alpha, "finished in a row that is not"));
    let twice = insert(planned(&alpha, "two origins"));
    assert_eq!(
        store.plan_field_count().unwrap(),
        archon_queue::store::PlanFieldCount {
            unattributed: 0,
            legacy: fields(&legacy),
            unreadable: 0,
        }
    );
    let run =
        |sql: &str, id: &QueueItemId| assert_eq!(conn.execute(sql, [id.as_str()]).unwrap(), 1);
    let unbound = "UPDATE items SET data = json_remove(data, '$.model_origins.item') WHERE id = ?1";
    run(unbound, &foreign.id);
    run(unbound, &finished.id);
    run(
        "UPDATE items SET state = 'succeeded' WHERE id = ?1",
        &finished.id,
    );
    run(
        "UPDATE items SET state = 'cancelled', data = '{not json' WHERE id = ?1",
        &finished_corrupt.id,
    );
    run(
        "UPDATE items SET data = '{not json' WHERE id = ?1",
        &corrupt.id,
    );
    run(
        "UPDATE items SET data = json_remove(data, '$.model_plan') WHERE id = ?1",
        &planless.id,
    );
    run(
        "UPDATE items SET data = json_set(data, '$.model_plan', json_object('review', 1)) WHERE id = ?1",
        &misplanned.id,
    );
    run("UPDATE items SET data = x'00ff' WHERE id = ?1", &binary.id);
    run(
        "UPDATE items SET data = json_set(data, '$.state', json_object('state', 'cancelled')) WHERE id = ?1",
        &misfiled.id,
    );
    run(
        "UPDATE items SET data = replace(data, '\"model_plan\":', '\"model_origins\":null,\"model_plan\":') WHERE id = ?1",
        &twice.id,
    );
    let count = store.plan_field_count().expect("status never aborts");
    assert_eq!(
        count.unattributed,
        fields(&foreign),
        "finished items are not read"
    );
    assert_eq!(count.legacy, fields(&legacy));
    assert_eq!(
        count.unreadable, 6,
        "corrupt, planless, misplanned, binary, misfiled and twice-keyed"
    );
    assert_eq!(store.item(&sound.id).unwrap().id, sound.id);
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
    assert_eq!(read.model_plan.fault(&read.id), None, "stored reconciled");
    assert_eq!(
        store.plan_field_count().unwrap().unattributed,
        fields(&first)
    );
}
