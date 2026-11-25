//! Live design sessions in the store: ids, the one-in-flight rule, finishing, assets.

mod common;

use archon_core::design::{
    AssetSource, DesignAsset, DesignAssetId, DesignSessionId, MAX_ITEM_ASSET_BYTES, NoteKind,
    NoteStatus, Rect, SessionState, Target, Viewport, ViewportPreset,
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
        follows: None,
        refs: Vec::new(),
        before_asset: None,
        created_by_device: None,
    }
}

fn add(store: &mut Store, item: &QueueItemId, session: &DesignSessionId, key: &str) -> String {
    let (note, created) = store.design_note_add(new_note(item, session, key)).unwrap();
    assert!(created);
    note.id
}

#[test]
fn a_continue_with_an_invalid_followup_rolls_back_its_undos_and_sequence() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let undo = new_note(item, &session, "undo:one");
    let mut follow = new_note(item, &session, "follow");
    follow.text.clear();
    assert!(
        store
            .design_notes_add_group(vec![undo, follow], None)
            .is_err()
    );
    assert!(store.design_notes(item).unwrap().is_empty());
    assert_eq!(add(&mut store, item, &session, "next"), "D1");
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
                capture: None,
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
        capture: None,
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
fn superseding_a_session_settles_notes_and_clears_owed_work() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let sent = add(&mut store, item, &session, "sent");
    send(&mut store, item, &sent).unwrap();
    let queued = add(&mut store, item, &session, "queued");
    let key = keys::design_owed(&session);
    store.kv_set(&key, "notice").unwrap();
    let initial = store.design_session(&session).unwrap().unwrap();
    live(&mut store, item);
    assert_eq!(
        store
            .design_session(&session)
            .unwrap()
            .unwrap()
            .note_version,
        initial.note_version + 2
    );
    assert!(store.kv_get(&key).unwrap().is_none());
    for (id, status) in [(sent, NoteStatus::Failed), (queued, NoteStatus::Cancelled)] {
        let note = store.design_note(item, &id).unwrap().unwrap();
        assert_eq!(note.status, status);
        assert_eq!(
            note.fail_reason,
            Some(archon_core::design::FailReason::NotDelivered)
        );
    }
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
fn confirmation_rolls_back_inclusion_and_delivery_if_a_note_write_fails() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let note = add(&mut store, item, &session, "k1");
    send(&mut store, item, &note).unwrap();
    status(&mut store, item, &note, NoteStatus::Done).unwrap();
    let current = store.design_session(&session).unwrap().unwrap();
    let mut record = FinishRecord {
        capture: "capture".into(),
        head: "head".into(),
        note_version: current.note_version,
        choices: None,
    };
    let review = store
        .design_session_transition(
            &session,
            SessionState::Live,
            SessionState::Finishing,
            Some(record.clone()),
            false,
        )
        .unwrap();
    record.choices = Some(archon_core::design::FinishChoices {
        include: Default::default(),
        superseded: Default::default(),
    });
    let key = keys::design_owed(&session);
    let allowed = (note.clone(), true);
    let invalid = ("D99".to_owned(), true);
    let error = store
        .design_finish_confirm(
            &session,
            record.clone(),
            &[allowed.clone(), invalid],
            r#"[{"text":"Finish"}]"#,
        )
        .unwrap_err();

    assert!(error.to_string().contains("D99"));
    let after = store.design_session(&session).unwrap().unwrap();
    assert_eq!(after.state, SessionState::Finishing);
    assert_eq!(after.note_version, review.note_version);
    assert_eq!(
        store
            .design_note(item, &note)
            .unwrap()
            .unwrap()
            .final_included,
        None
    );
    assert_eq!(store.kv_get(&key).unwrap(), None);

    let confirmed = store
        .design_finish_confirm(&session, record, &[allowed], r#"[{"text":"Finish"}]"#)
        .unwrap();
    assert_eq!(confirmed.state, SessionState::Confirmed);
    assert!(store.kv_get(&key).unwrap().is_some());
}

#[test]
fn finish_is_a_compare_and_set_with_the_operators_sentences() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let finish = |store: &mut Store, cancel: bool| {
        let current = store.design_session(&session).unwrap().unwrap();
        let queued = store
            .design_notes(item)
            .unwrap()
            .into_iter()
            .filter(|n| n.status == NoteStatus::Queued && cancel)
            .count() as u64;
        store.design_session_transition(
            &session,
            SessionState::Live,
            SessionState::Finishing,
            Some(FinishRecord {
                capture: "cap1".into(),
                head: "h".into(),
                note_version: current.note_version + queued,
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
                agent: None,
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
fn a_retaken_before_picture_replaces_the_old_one_and_survives_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut store = Store::open(&path).unwrap();
    let item = &store.insert_item(&item(None, "design"), None).unwrap().id;
    let session = live(&mut store, item);
    let (note, _) = store
        .design_note_add(new_note(item, &session, "k1"))
        .unwrap();
    assert_eq!(note.before_asset, None);
    let first = store
        .design_asset_put(asset(item, "b1", AssetSource::Capture, 10))
        .unwrap();
    let second = store
        .design_asset_put(asset(item, "b2", AssetSource::Capture, 20))
        .unwrap();
    for kept in [&first, &second] {
        store
            .design_note_change(
                item,
                &note.id,
                NoteChange::Before {
                    asset: kept.id.clone(),
                },
            )
            .unwrap();
        let read = store.design_note(item, &note.id).unwrap().unwrap();
        assert_eq!(read.before_asset.as_ref(), Some(&kept.id));
        assert_eq!(read.after_asset, None, "only the before picture moves");
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    let reloaded = store.design_note(item, &note.id).unwrap().unwrap();
    assert_eq!(reloaded.before_asset, Some(second.id.clone()));
    assert_eq!(store.design_asset(item, &second.id).unwrap(), Some(second));
}

/// A picture keeps how the browser took it, and the same bytes taken another
/// way are another record: the ledger must name the conditions of its own.
#[test]
fn a_picture_keeps_its_capture_conditions() {
    use archon_core::intent::{CaptureArea, CaptureMethod, ImageCapture, Viewport as Size};
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let capture = ImageCapture {
        method: CaptureMethod::PageScreenshot,
        viewport: Size {
            width: 1440,
            height: 900,
        },
        area: CaptureArea {
            x: 0,
            y: 0,
            width: 1440,
            height: 900,
        },
        pixels: Size {
            width: 2880,
            height: 1800,
        },
    };
    let taken = DesignAsset {
        capture: Some(capture),
        ..asset(item, "cc", AssetSource::Final, 100)
    };
    let stored = store.design_asset_put(taken).unwrap();
    assert_eq!(stored.capture, Some(capture));
    let read = store.design_asset(item, &stored.id).unwrap().unwrap();
    assert_eq!(read.capture, Some(capture));
    let unknown = store
        .design_asset_put(asset(item, "cc", AssetSource::Final, 100))
        .unwrap();
    assert_ne!(unknown.id, stored.id);
    assert_eq!(unknown.capture, None);
}

#[test]
fn sweeping_one_capture_keeps_a_shared_picture_used_by_a_note() {
    let (mut store, _repo, items) = seeded(2);
    let item = &items[0].id;
    let a = asset(item, "aa", AssetSource::Capture, 100);
    let stored = store.design_asset_put(a.clone()).unwrap();
    let again = store
        .design_asset_put(asset(item, "aa", AssetSource::Capture, 100))
        .unwrap();
    assert_eq!(again.id, stored.id);
    let other = store
        .design_asset_put(asset(item, "aa", AssetSource::Upload, 100))
        .unwrap();
    assert_ne!(other.id, stored.id);
    store
        .design_asset_put(asset(&items[1].id, "aa", AssetSource::Capture, 7))
        .unwrap();
    assert_eq!(store.design_asset_bytes(item).unwrap(), 100);
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
    assert!(
        store
            .design_asset_sweep(item, &other.id, t(100), |_| {
                panic!("another capture still uses the file")
            })
            .unwrap()
    );
    assert!(store.design_asset(item, &other.id).unwrap().is_none());
    assert!(
        !store
            .design_asset_sweep(item, &stored.id, t(100), |_| {
                panic!("a note still uses the capture")
            })
            .unwrap()
    );
    let gone = store.design_item_delete(item).unwrap();
    assert_eq!(gone.len(), 1);
    assert!(store.design_notes(item).unwrap().is_empty());
    assert!(store.design_sessions(item).unwrap().is_empty());
    assert_eq!(store.kv_get(&keys::design_seq(item)).unwrap(), None);
    assert_eq!(store.design_asset_bytes(&items[1].id).unwrap(), 7);
}

#[test]
fn a_failed_file_operation_rolls_back_the_asset_change() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let picture = asset(item, "same", AssetSource::Capture, 100);
    let failed = store.design_asset_publish(picture.clone(), None, |_| {
        Err(ArchonError::Store("picture write failed".into()))
    });
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("picture write failed")
    );
    assert_eq!(store.design_asset(item, &picture.id).unwrap(), None);
    assert_eq!(store.design_asset_put(picture.clone()).unwrap(), picture);
    let failed = store.design_asset_sweep(item, &picture.id, t(10), |_| {
        Err(ArchonError::Store("picture removal failed".into()))
    });
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("picture removal failed")
    );
    assert_eq!(
        store.design_asset(item, &picture.id).unwrap(),
        Some(picture)
    );
}

#[test]
fn identical_picture_bytes_keep_independent_capture_metadata() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let mut original = asset(item, "same", AssetSource::Capture, 100);
    let target = Target {
        path: "/first".into(),
        selector: None,
        role_name: None,
        component: None,
        rect: Rect {
            x: 0.,
            y: 0.,
            w: 10.,
            h: 10.,
        },
        viewport: Viewport {
            w: 800,
            h: 600,
            preset: ViewportPreset::Desktop,
        },
        styles: Default::default(),
        region: true,
        steps: Box::default(),
    };
    original.target = Some(target);
    original.commit = Some("old".into());
    original.note = Some("D1".into());
    assert_eq!(store.design_asset_put(original.clone()).unwrap(), original);
    let mut different_target = original.clone();
    different_target.target.as_mut().unwrap().path = "/second".into();
    let mut different_commit = original.clone();
    different_commit.commit = Some("new".into());
    let mut different_note = original.clone();
    different_note.note = None;
    for mut capture in [different_target, different_commit, different_note] {
        capture.id = DesignAssetId::new();
        assert_eq!(store.design_asset_put(capture.clone()).unwrap(), capture);
        let mut retry = capture.clone();
        retry.id = DesignAssetId::new();
        retry.created_at = t(50);
        assert_eq!(store.design_asset_put(retry).unwrap(), capture);
    }
    assert_eq!(
        store.design_asset(item, &original.id).unwrap(),
        Some(original)
    );
    assert_eq!(store.design_asset_bytes(item).unwrap(), 100);
}

#[test]
fn the_asset_cap_charges_new_file_and_sketch_bytes_only() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let picture = asset(item, "same", AssetSource::Sketch, MAX_ITEM_ASSET_BYTES - 4);
    let first = store
        .design_asset_publish(picture.clone(), Some("é"), |_| Ok(()))
        .unwrap()
        .unwrap();
    let mut changed = picture.clone();
    changed.id = DesignAssetId::new();
    let second = store
        .design_asset_publish(changed.clone(), Some("ø"), |_| Ok(()))
        .unwrap()
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(
        store.design_asset_bytes(item).unwrap(),
        MAX_ITEM_ASSET_BYTES
    );
    let mut retry = picture.clone();
    retry.id = DesignAssetId::new();
    assert_eq!(
        store
            .design_asset_publish(retry, Some("é"), |_| Ok(()))
            .unwrap(),
        Some(first)
    );
    changed.id = DesignAssetId::new();
    assert!(
        store
            .design_asset_publish(changed, Some("x"), |_| panic!("over cap"))
            .unwrap()
            .is_none()
    );
    let shared = asset(item, "same", AssetSource::Capture, picture.bytes);
    assert_eq!(store.design_asset_put(shared.clone()).unwrap(), shared);
    let extra = asset(item, "extra", AssetSource::Capture, 1);
    assert!(
        store
            .design_asset_publish(extra, None, |_| panic!("over cap"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_reused_pending_capture_outlives_a_stale_sweep_candidate() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let original = store
        .design_asset_put(asset(item, "same", AssetSource::Upload, 1))
        .unwrap();
    assert_eq!(
        store.design_unused_assets(t(10)).unwrap(),
        vec![original.clone()]
    );
    let mut retry = original.clone();
    retry.id = DesignAssetId::new();
    retry.created_at = t(20);
    assert_eq!(store.design_asset_put(retry).unwrap(), original);
    assert!(
        !store
            .design_asset_sweep(item, &original.id, t(10), |_| panic!("recently reused"))
            .unwrap()
    );
    assert!(
        store
            .design_asset_sweep(item, &original.id, t(30), |capture| {
                assert_eq!(capture, &original);
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(store.design_asset_bytes(item).unwrap(), 0);
}

#[test]
fn concurrent_exact_asset_publishes_reuse_one_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut first = Store::open(&path).unwrap();
    let mut second = Store::open(&path).unwrap();
    let item = first.insert_item(&item(None, "design"), None).unwrap().id;
    let a = asset(&item, "same", AssetSource::Capture, 1);
    let mut b = a.clone();
    b.id = DesignAssetId::new();
    let barrier = std::sync::Barrier::new(2);
    let (left, right) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            barrier.wait();
            first.design_asset_put(a).unwrap()
        });
        barrier.wait();
        let right = second.design_asset_put(b).unwrap();
        (worker.join().unwrap(), right)
    });
    assert_eq!(left, right);
    assert_eq!(second.design_asset_bytes(&item).unwrap(), 1);
    assert_eq!(second.design_unused_assets(t(10)).unwrap(), vec![left]);
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
    let versions = |conn: &rusqlite::Connection| {
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    conn.execute_batch(
        "DROP TABLE design_notes; DROP TABLE design_sessions; DROP TABLE design_assets;
         DELETE FROM schema_migrations WHERE version >= 29",
    )
    .unwrap();
    assert_eq!(versions(&conn), 28);
    drop(conn);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(versions(&rusqlite::Connection::open(&path).unwrap()), 32);
    assert_eq!(store.item(&item.id).unwrap().title, "x");
    let session = live(&mut store, &item.id);
    assert_eq!(add(&mut store, &item.id, &session, "k"), "D1");
}

/// A design finished with no change names its requirement after the item's
/// note counter: past every note made, never D1 again.
#[test]
fn the_next_note_id_follows_the_counter() {
    let mut store = store();
    let item = store.insert_item(&item(None, "design"), None).unwrap().id;
    assert_eq!(store.design_next_note_id(&item).unwrap(), "D1");
    let session = live(&mut store, &item);
    add(&mut store, &item, &session, "a");
    add(&mut store, &item, &session, "b");
    assert_eq!(store.design_next_note_id(&item).unwrap(), "D3");
    assert_eq!(
        store.design_next_note_id(&item).unwrap(),
        "D3",
        "nothing reserved"
    );
}

#[test]
fn stale_finish_capture_keeps_the_queued_note() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    let id = add(&mut store, item, &session, "queued");
    let record = FinishRecord {
        capture: "cap".into(),
        head: "h".into(),
        note_version: 0,
        choices: None,
    };
    let error = store
        .design_session_transition(
            &session,
            SessionState::Live,
            SessionState::Finishing,
            Some(record),
            true,
        )
        .unwrap_err();
    assert!(refusal(&error, "Conflict"));
    assert_eq!(
        store.design_session(&session).unwrap().unwrap().state,
        SessionState::Live
    );
    assert_eq!(
        store.design_note(item, &id).unwrap().unwrap().status,
        NoteStatus::Queued
    );
}

#[test]
fn a_corrupt_counter_refuses_reads_and_writes_together() {
    let (mut store, _, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    for value in ["broken", "18446744073709551615"] {
        store.kv_set(&keys::design_seq(item), value).unwrap();
        assert!(store.design_next_note_id(item).is_err());
        assert!(
            store
                .design_note_add(new_note(item, &session, "key"))
                .is_err()
        );
        assert!(store.design_notes(item).unwrap().is_empty());
    }
}

/// Schema 30: a follow-up keeps the note it follows and its references, and a
/// note from before reads with none.
#[test]
fn a_follow_up_keeps_its_link_and_references() {
    let (mut store, _repo, items) = seeded(1);
    let item = &items[0].id;
    let session = live(&mut store, item);
    assert_eq!(add(&mut store, item, &session, "k1"), "D1");
    let mut follow = new_note(item, &session, "k2");
    follow.follows = Some("D1".into());
    follow.refs = vec!["D1".into()];
    let (note, created) = store.design_note_add(follow).unwrap();
    assert!(created);
    let notes = store.design_notes(item).unwrap();
    let read = notes.iter().find(|n| n.id == note.id).unwrap();
    assert_eq!(read.follows.as_deref(), Some("D1"));
    assert_eq!(read.refs, ["D1"]);
    let first = notes.iter().find(|n| n.id == "D1").unwrap();
    assert!(first.follows.is_none() && first.refs.is_empty());
}
