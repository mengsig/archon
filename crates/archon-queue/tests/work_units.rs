mod common;

use archon_core::protocol::WorkUnitCounts;
use archon_core::queue::QueueState;
use archon_queue::Store;
use common::{item, repo, store, t};

#[test]
fn counts_transitive_and_branched_chains_as_active_when_any_member_is_active() {
    let mut store = store();
    let repo = repo("units");
    store.insert_repo(&repo).unwrap();
    let first = store
        .insert_item(&item(Some(&repo), "first"), None)
        .unwrap();
    let mut second = item(Some(&repo), "second");
    second.created_at = t(2);
    second.supersedes = Some(first.id.clone());
    let second = store.insert_item(&second, None).unwrap();
    let mut third = item(Some(&repo), "third");
    third.created_at = t(3);
    third.supersedes = Some(second.id.clone());
    store.insert_item(&third, None).unwrap();
    let mut branch = item(Some(&repo), "branch");
    branch.created_at = t(4);
    branch.supersedes = Some(first.id.clone());
    let branch = store.insert_item(&branch, None).unwrap();
    store
        .transition_item(
            &branch.id,
            QueueState::Cancelled {
                reason: Some("done".into()),
            },
            t(5),
        )
        .unwrap();
    store.insert_item(&item(None, "independent"), None).unwrap();
    assert_eq!(
        store.work_unit_counts().unwrap(),
        WorkUnitCounts {
            active: 2,
            finished: 0,
            missing_targets: 0
        }
    );
}

#[test]
fn missing_targets_are_reported_and_shared_missing_targets_still_connect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let mut store = Store::open(&path).unwrap();
    let repo = repo("missing");
    store.insert_repo(&repo).unwrap();
    let first = store
        .insert_item(&item(Some(&repo), "first"), None)
        .unwrap();
    for title in ["second", "third"] {
        let mut next = item(Some(&repo), title);
        next.supersedes = Some(first.id.clone());
        store.insert_item(&next, None).unwrap();
    }
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("DELETE FROM items WHERE id = ?1", [first.id.as_str()])
        .unwrap();
    assert_eq!(
        store.work_unit_counts().unwrap(),
        WorkUnitCounts {
            active: 1,
            finished: 0,
            missing_targets: 1
        }
    );
}

#[test]
fn counts_all_retained_items_beyond_a_filtered_page() {
    let mut store = store();
    for n in 0..205 {
        store
            .insert_item(&item(None, &format!("work {n}")), None)
            .unwrap();
    }
    let page = store
        .items(&archon_core::protocol::ItemQuery {
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.len(), 10);
    assert_eq!(store.work_unit_counts().unwrap().active, 205);
}
