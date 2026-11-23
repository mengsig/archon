mod common;

use common::{item, repo, store};

#[test]
fn detects_titles_at_the_threshold_only_in_the_same_repo() {
    let mut store = store();
    let first_repo = repo("first");
    let other_repo = repo("other");
    store.insert_repo(&first_repo).unwrap();
    store.insert_repo(&other_repo).unwrap();
    let first = store
        .insert_item(&item(Some(&first_repo), "abcdefghij"), None)
        .unwrap();
    let below = store
        .insert_item(&item(Some(&first_repo), "abcdefghXY"), None)
        .unwrap();
    assert_eq!(below.supersedes, None);
    let other = store
        .insert_item(&item(Some(&other_repo), "abcdefghij"), None)
        .unwrap();
    assert_eq!(other.supersedes, None);
    let next = store
        .insert_item(&item(Some(&first_repo), "abcdefghiX"), None)
        .unwrap();
    assert_eq!(next.supersedes, Some(first.id));
    assert_eq!(store.item(&next.id).unwrap().supersedes, next.supersedes);
}

#[test]
fn explicit_link_and_idempotent_replay_are_preserved() {
    let mut store = store();
    let repo = repo("first");
    store.insert_repo(&repo).unwrap();
    let first = store
        .insert_item(&item(Some(&repo), "original"), None)
        .unwrap();
    let mut request = item(Some(&repo), "unrelated title");
    request.supersedes = Some(first.id.clone());
    let stored = store.insert_item(&request, Some("once")).unwrap();
    assert_eq!(stored.supersedes.as_ref(), Some(&first.id));
    let replay = store.insert_item(&request, Some("once")).unwrap();
    assert_eq!(replay.id, stored.id);
    assert_eq!(replay.supersedes, stored.supersedes);
    let unlinked = item(Some(&repo), "unique title");
    let original = store.insert_item(&unlinked, Some("unlinked")).unwrap();
    let replay = store.insert_item(&unlinked, Some("unlinked")).unwrap();
    assert_eq!(replay.id, original.id);
    assert_eq!(replay.supersedes, None);
}

#[test]
fn simultaneous_enqueues_link_inside_the_write_transaction() {
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let repo = repo("concurrent");
    let mut seed = archon_queue::Store::open(&path).unwrap();
    seed.insert_repo(&repo).unwrap();
    drop(seed);
    let barrier = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();
    for _ in 0..2 {
        let mut store = archon_queue::Store::open(&path).unwrap();
        let barrier = barrier.clone();
        let tx = tx.clone();
        let request = item(Some(&repo), "the same concurrent task");
        std::thread::spawn(move || {
            barrier.wait();
            tx.send(store.insert_item(&request, None)).unwrap();
        });
    }
    let first = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    let second = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert_ne!(first.id, second.id);
    assert!(
        first.supersedes.as_ref() == Some(&second.id)
            || second.supersedes.as_ref() == Some(&first.id)
    );
}
