//! Live design sessions in the store: ids, the one-in-flight rule, finishing, assets.

mod common;

use archon_core::design::{
    AssetSource, DesignAsset, DesignAssetId, DesignSessionId, NoteKind, NoteStatus, SessionState,
    Viewport, ViewportPreset,
};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::intent::{
    ApprovedIntent, IntentBinding, IntentSource, ReferenceImage, Requirement, RequirementKind,
};
use archon_queue::Store;
use archon_queue::store::{
    FinishRecord, NewDesignNote, NewDesignSession, NoteChange, SessionPatch, keys,
};
use common::*;

fn new_session(item: &QueueItemId) -> NewDesignSession {
    NewDesignSession {
        item: item.clone(),
        run: PipelineRunId::new(),
        stage_id: "design".into(),
        attempt: 1,
        viewport_preset: ViewportPreset::Desktop,
        sandbox_off: false,
        agent: None,
    }
}

fn live(store: &mut Store, item: &QueueItemId) -> DesignSessionId {
    let session = store.design_session_create(new_session(item)).unwrap();
    assert_eq!(session.state, SessionState::Starting);
    store
        .design_session_transition(
            &session.id,
            SessionState::Starting,
            SessionState::Live,
            None,
            false,
        )
        .unwrap();
    session.id
}

fn new_note(item: &QueueItemId, session: &DesignSessionId, key: &str) -> NewDesignNote {
    NewDesignNote {
        item: item.clone(),
        session: session.clone(),
        kind: NoteKind::Change,
        text: format!("note {key}"),
        details_text: None,
        send_picture: false,
        target: None,
        viewport: Viewport {
            w: 1440,
            h: 900,
            preset: ViewportPreset::Desktop,
        },
        client_key: key.into(),
        reverts: None,
        resent_from: None,
        before_asset: None,
        created_by_device: None,
    }
}

fn add(store: &mut Store, item: &QueueItemId, session: &DesignSessionId, key: &str) -> String {
    let (note, created) = store.design_note_add(new_note(item, session, key)).unwrap();
    assert!(created);
    note.id
}

fn status(
    store: &mut Store,
    item: &QueueItemId,
    id: &str,
    to: NoteStatus,
) -> Result<(), ArchonError> {
    store
        .design_note_change(
            item,
            id,
            NoteChange::Status {
                to,
                fail_reason: None,
                commit: None,
                no_change: false,
            },
        )
        .map(|_| ())
}

fn send(store: &mut Store, item: &QueueItemId, id: &str) -> Result<(), ArchonError> {
    store
        .design_note_change(
            item,
            id,
            NoteChange::Sent {
                base_commit: "abc".into(),
                delivery: 1,
            },
        )
        .map(|_| ())
}

fn intent(id: &str, version: u32, ids: &[&str], images: &[&str]) -> ApprovedIntent {
    ApprovedIntent {
        id: id.into(),
        version,
        operator_words: "words".into(),
        source: IntentSource::Inbox { seq: 1 },
        artifacts: vec![],
        images: images
            .iter()
            .map(|i| ReferenceImage {
                id: (*i).into(),
                path: "a.png".into(),
                sha256: "0".repeat(64),
                viewport: None,
                theme: None,
                state: None,
            })
            .collect(),
        requirements: ids
            .iter()
            .map(|r| Requirement {
                id: (*r).into(),
                text: "t".into(),
                // A visual requirement must name a picture; without one it is behaviour.
                kind: if images.is_empty() {
                    RequirementKind::Behaviour
                } else {
                    RequirementKind::Visual
                },
                images: images.iter().map(|i| (*i).into()).collect(),
                owner: "app".into(),
            })
            .collect(),
    }
}

fn asset(item: &QueueItemId, sha: &str, source: AssetSource, bytes: u64) -> DesignAsset {
    DesignAsset {
        id: DesignAssetId::new(),
        item: item.clone(),
        sha256: sha.into(),
        mime: "image/png".into(),
        bytes,
        width: 10,
        height: 10,
        path: format!("design/assets/x/{sha}.png"),
        source,
        target: None,
        note: None,
        commit: None,
        created_at: t(0),
    }
}

fn refusal(error: &ArchonError, code: &str) -> bool {
    matches!(error, ArchonError::Design { code: found, .. } if format!("{found:?}") == code)
}

#[test]
fn numbers_are_monotonic_across_sessions_and_replays_are_idempotent() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let first = live(&mut store, item);
    assert_eq!(add(&mut store, item, &first, "k1"), "D1");
    assert_eq!(add(&mut store, item, &first, "k2"), "D2");
    let (again, created) = store.design_note_add(new_note(item, &first, "k1")).unwrap();
    assert!(!created);
    assert_eq!(again.id, "D1");
    assert_eq!(store.design_notes(item).unwrap().len(), 2);
    assert_eq!(
        store.kv_get(&keys::design_seq(item)).unwrap().as_deref(),
        Some("3")
    );
    // A Revise: a second session supersedes the first; numbering goes on.
    let second = live(&mut store, item);
    let old = store.design_session(&first).unwrap().unwrap();
    assert_eq!(old.state, SessionState::Ended);
    assert_eq!(old.end_reason.as_deref(), Some("superseded"));
    assert_eq!(add(&mut store, item, &second, "k3"), "D3");
    // The ended session takes no note.
    let refused = store
        .design_note_add(new_note(item, &first, "k4"))
        .unwrap_err();
    assert!(refusal(&refused, "Gone"));
}

#[test]
fn two_handles_on_one_database_never_duplicate_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut a = Store::open(&path).unwrap();
    let repo = repo("alpha");
    a.insert_repo(&repo).unwrap();
    let item = a.insert_item(&item(Some(&repo), "x"), None).unwrap().id;
    let session = live(&mut a, &item);
    let mut b = Store::open(&path).unwrap();
    let mut ids = Vec::new();
    for n in 0..6 {
        let store = if n % 2 == 0 { &mut a } else { &mut b };
        ids.push(add(store, &item, &session, &format!("k{n}")));
    }
    assert_eq!(ids, ["D1", "D2", "D3", "D4", "D5", "D6"]);
}

#[test]
fn ids_of_the_approved_and_pending_intents_are_never_reused() {
    let mut store = store();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let mut queued = item(Some(&repo), "x");
    queued.intent = Some(Box::new(IntentBinding {
        id: "inbox".into(),
        version: 1,
        requirements: vec![],
    }));
    let item = store.insert_item(&queued, None).unwrap();
    store
        .save_approved_intent(&intent("inbox", 1, &["D1"], &["D1-final"]))
        .unwrap();
    let run = start(&mut store, &item.id);
    let session = live(&mut store, &item.id);
    assert_eq!(add(&mut store, &item.id, &session, "a"), "D2");
    let pending = serde_json::json!({
        "record": intent("gate", 1, &["D4-mobile"], &[]),
        "page": "p.html",
        "page_sha256": "0",
    });
    store
        .kv_set(
            &format!("proposal_intent:{}:design:1", run.id),
            &pending.to_string(),
        )
        .unwrap();
    assert_eq!(add(&mut store, &item.id, &session, "b"), "D5");
    assert_eq!(add(&mut store, &item.id, &session, "c"), "D6");
}

#[test]
fn one_note_is_in_flight_edits_wait_for_queued_and_history_is_kept() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let first = live(&mut store, item);
    let d1 = add(&mut store, item, &first, "k1");
    let d2 = add(&mut store, item, &first, "k2");
    store
        .design_note_change(
            item,
            &d1,
            NoteChange::Edit {
                text: "better".into(),
            },
        )
        .unwrap();
    send(&mut store, item, &d1).unwrap();
    // Edit only while queued; a second send waits.
    assert!(matches!(
        store.design_note_change(item, &d1, NoteChange::Edit { text: "x".into() }),
        Err(ArchonError::InvalidTransition(_))
    ));
    assert!(matches!(
        send(&mut store, item, &d2),
        Err(ArchonError::InvalidTransition(_))
    ));
    // Not allowed by the table.
    assert!(matches!(
        status(&mut store, item, &d1, NoteStatus::Queued),
        Err(ArchonError::InvalidTransition(_))
    ));
    status(&mut store, item, &d1, NoteStatus::Working).unwrap();
    status(&mut store, item, &d1, NoteStatus::Done).unwrap();
    send(&mut store, item, &d2).unwrap();
    assert_eq!(
        store.design_note(item, &d1).unwrap().unwrap().text,
        "better"
    );

    // A revert never goes through Sent and runs only when none is in flight.
    let mut revert = new_note(item, &first, "r1");
    revert.kind = NoteKind::Revert;
    revert.reverts = Some(d1.clone());
    let (revert, _) = store.design_note_add(revert).unwrap();
    assert!(send(&mut store, item, &revert.id).is_err());
    let prepare = |store: &mut Store| {
        store.design_note_change(
            item,
            &revert.id,
            NoteChange::RevertPrepared {
                base_commit: "b".into(),
                commit: "c".into(),
            },
        )
    };
    assert!(matches!(
        prepare(&mut store),
        Err(ArchonError::InvalidTransition(_))
    ));
    status(&mut store, item, &d2, NoteStatus::Done).unwrap();
    let prepared = prepare(&mut store).unwrap();
    assert_eq!(prepared.status, NoteStatus::Working);
    assert_eq!(prepared.base_commit.as_deref(), Some("b"));
    assert_eq!(prepared.commit.as_deref(), Some("c"));

    // A new session makes the earlier rows read-only and leaves them as they were.
    status(&mut store, item, &revert.id, NoteStatus::Done).unwrap();
    let before = store.design_notes(item).unwrap();
    let second = live(&mut store, item);
    for change in [
        NoteChange::Edit { text: "no".into() },
        NoteChange::Superseded {
            by: Some("D9".into()),
        },
    ] {
        let refused = store.design_note_change(item, &d1, change).unwrap_err();
        assert!(refusal(&refused, "ReadOnly"));
        assert!(
            refused
                .to_string()
                .contains("D1 belongs to an earlier design session; add a new note instead")
        );
    }
    assert_eq!(store.design_notes(item).unwrap(), before);
    assert_eq!(add(&mut store, item, &second, "n"), "D4");
}

#[test]
fn note_version_counts_every_insert_and_change() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let d1 = add(&mut store, item, &session, "k1");
    assert_eq!(
        store
            .design_session(&session)
            .unwrap()
            .unwrap()
            .note_version,
        1
    );
    status(&mut store, item, &d1, NoteStatus::Cancelled).unwrap();
    assert_eq!(
        store
            .design_session(&session)
            .unwrap()
            .unwrap()
            .note_version,
        2
    );
}

#[test]
fn finish_is_a_compare_and_set_with_the_operators_sentences() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let finish = |store: &mut Store, cancel: bool| {
        store.design_session_transition(
            &session,
            SessionState::Live,
            SessionState::Finishing,
            Some(FinishRecord {
                capture: "cap1".into(),
                head: "h".into(),
                note_version: 0,
                choices: None,
            }),
            cancel,
        )
    };
    let d1 = add(&mut store, item, &session, "k1");
    let message = |r: Result<_, ArchonError>| {
        r.map(|_: archon_core::design::DesignSession| ())
            .unwrap_err()
            .to_string()
    };
    // Queued notes need the operator's word.
    let d2 = add(&mut store, item, &session, "k2");
    assert!(message(finish(&mut store, false)).contains("Cancel 2 queued notes and finish?"));
    send(&mut store, item, &d1).unwrap();
    assert!(message(finish(&mut store, true)).contains("Finish after D1 is done"));
    assert_eq!(
        store.design_note(item, &d2).unwrap().unwrap().status,
        NoteStatus::Queued
    );
    status(&mut store, item, &d1, NoteStatus::NeedsReply).unwrap();
    assert!(message(finish(&mut store, true)).contains("D1 needs your answer"));
    status(&mut store, item, &d1, NoteStatus::Done).unwrap();
    // An owed delivery blocks too.
    store.kv_set(&keys::design_owed(&session), "1").unwrap();
    assert!(finish(&mut store, true).is_err());
    store.kv_delete(&keys::design_owed(&session)).unwrap();
    // A stale `from` is a conflict.
    let stale = store
        .design_session_transition(
            &session,
            SessionState::Finishing,
            SessionState::Live,
            None,
            false,
        )
        .unwrap_err();
    assert!(refusal(&stale, "Conflict"));
    let done = finish(&mut store, true).unwrap();
    assert_eq!(done.state, SessionState::Finishing);
    assert_eq!(done.finish_capture.as_deref(), Some("cap1"));
    assert_eq!(done.finish_head.as_deref(), Some("h"));
    assert_eq!(
        store.design_note(item, &d2).unwrap().unwrap().status,
        NoteStatus::Cancelled
    );
    // Finishing takes no new note.
    let refused = store
        .design_note_add(new_note(item, &session, "k3"))
        .unwrap_err();
    assert!(refusal(&refused, "Finishing"));
    // Back to editing clears the finish fields.
    let back = store
        .design_session_transition(
            &session,
            SessionState::Finishing,
            SessionState::Live,
            None,
            false,
        )
        .unwrap();
    assert_eq!(back.finish_capture, None);
    assert_eq!(back.finish_head, None);
    // Confirm after another finish; a second confirm from the same state conflicts.
    finish(&mut store, false).unwrap();
    let confirm = |store: &mut Store| {
        store.design_session_transition(
            &session,
            SessionState::Finishing,
            SessionState::Confirmed,
            None,
            false,
        )
    };
    assert_eq!(confirm(&mut store).unwrap().state, SessionState::Confirmed);
    assert!(refusal(&confirm(&mut store).unwrap_err(), "Conflict"));
    assert!(
        store
            .design_session_end(&session, "done")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .design_session_end(&session, "again")
            .unwrap()
            .is_none()
    );
}

#[test]
fn sessions_patch_and_list() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let patched = store
        .design_session_patch(
            &session,
            SessionPatch {
                preview_url: Some(Some("http://127.0.0.1:3000/".into())),
                browser: Some(Some((42, 7))),
                viewport_preset: Some(ViewportPreset::Phone),
                degraded: Some(true),
            },
        )
        .unwrap();
    assert_eq!(patched.browser_pid, Some(42));
    assert_eq!(patched.browser_started_at, Some(7));
    assert!(patched.degraded);
    assert_eq!(patched.viewport_preset, ViewportPreset::Phone);
    assert!(
        store
            .design_session_patch(
                &session,
                SessionPatch {
                    preview_url: Some(Some("http://example.com/".into())),
                    ..Default::default()
                },
            )
            .is_err()
    );
    assert_eq!(
        store.design_current_session(item).unwrap().unwrap().id,
        session
    );
    assert_eq!(store.design_sessions(item).unwrap().len(), 1);
    assert_eq!(
        store
            .design_sessions_in(&[SessionState::Live])
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .design_sessions_in(&[SessionState::Ended])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_session_holds_at_most_200_notes() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    for n in 0..200 {
        add(&mut store, item, &session, &format!("k{n}"));
    }
    assert!(matches!(
        store.design_note_add(new_note(item, &session, "over")),
        Err(ArchonError::Invalid(_))
    ));
}

#[test]
fn assets_dedupe_by_item_hash_and_source_and_are_swept() {
    let (mut store, _repo, items) = seeded(2);
    let item = &items[0].id;
    let a = asset(item, "aa", AssetSource::Capture, 100);
    let stored = store.design_asset_put(a.clone()).unwrap();
    let again = store
        .design_asset_put(asset(item, "aa", AssetSource::Capture, 100))
        .unwrap();
    assert_eq!(again.id, stored.id);
    let other = store
        .design_asset_put(asset(item, "aa", AssetSource::Upload, 50))
        .unwrap();
    assert_ne!(other.id, stored.id);
    store
        .design_asset_put(asset(&items[1].id, "aa", AssetSource::Capture, 7))
        .unwrap();
    assert_eq!(store.design_asset_bytes(item).unwrap(), 150);
    assert_eq!(
        store.design_asset(item, &stored.id).unwrap().unwrap(),
        stored
    );
    // A note that uses a picture keeps it from the sweep.
    let session = live(&mut store, item);
    let mut note = new_note(item, &session, "k");
    note.before_asset = Some(stored.id.clone());
    store.design_note_add(note).unwrap();
    let unused = store.design_unused_assets(t(100)).unwrap();
    assert_eq!(
        unused
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>()
            .len(),
        2
    );
    assert!(unused.iter().all(|a| a.id != stored.id));
    store.design_asset_delete(item, &other.id).unwrap();
    assert!(store.design_asset(item, &other.id).unwrap().is_none());
    // Deleting the item returns what remains and clears its counter.
    let gone = store.design_item_delete(item).unwrap();
    assert_eq!(gone.len(), 1);
    assert!(store.design_notes(item).unwrap().is_empty());
    assert!(store.design_sessions(item).unwrap().is_empty());
    assert_eq!(store.kv_get(&keys::design_seq(item)).unwrap(), None);
    assert_eq!(store.design_asset_bytes(&items[1].id).unwrap(), 7);
}

#[test]
fn the_migration_applies_to_an_existing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut store = Store::open(&path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let item = store.insert_item(&item(Some(&repo), "x"), None).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE design_notes; DROP TABLE design_sessions; DROP TABLE design_assets;
         DELETE FROM schema_migrations WHERE version >= 25",
    )
    .unwrap();
    drop(conn);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.item(&item.id).unwrap().title, "x");
    let session = live(&mut store, &item.id);
    assert_eq!(add(&mut store, &item.id, &session, "k"), "D1");
}
