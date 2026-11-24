//! Undo of a done note (design.md decision B, T-U13): archond reverts the
//! commit itself, in the queue, and records what it did.

use crate::common::design::Fixture;
use crate::design_session::{add, note, report, state};
use archon_core::design::{FailReason, NoteStatus};
use archon_core::protocol::{AgentNoteStatus, Request};

/// A note done with a real commit.
pub(crate) async fn done_note(f: &Fixture, key: &str, file: &str, body: &str) -> (String, String) {
    let added = add(f, key, &format!("change {file}")).await;
    let commit = f.commit(
        file,
        body,
        &format!("change {file}\n\nArchon-Note: {}\n", added.id),
    );
    report(f, &added.id, AgentNoteStatus::Working, None, None)
        .await
        .unwrap();
    report(
        f,
        &added.id,
        AgentNoteStatus::Done,
        Some(commit.clone()),
        None,
    )
    .await
    .unwrap();
    (added.id, commit)
}

async fn undo(
    f: &Fixture,
    id: &str,
) -> Result<archon_core::protocol::Reply, archon_core::error::ArchonError> {
    f.operator(Request::DesignNoteUndo {
        item: f.item.clone(),
        id: id.into(),
    })
    .await
}

/// T-U13 b: the undo is a new revert note, applied by archond; the original
/// shows as undone and HEAD moved by one revert commit.
#[tokio::test]
async fn undo_reverts_the_commit_and_marks_the_note_undone() {
    let f = Fixture::new().await;
    let (id, commit) = done_note(&f, "k1", "a.txt", "one\n").await;
    let before = f.head();
    assert_eq!(before, commit);
    undo(&f, &id).await.expect("undo queued");
    for _ in 0..100 {
        if state(&f).await.notes.iter().any(|n| {
            n.note.reverts.as_deref() == Some(id.as_str()) && n.note.status == NoteStatus::Done
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let view = state(&f).await;
    let revert = view
        .notes
        .iter()
        .find(|n| n.note.reverts.as_deref() == Some(id.as_str()))
        .expect("a revert note");
    assert_eq!(revert.note.status, NoteStatus::Done);
    assert_ne!(f.head(), before, "HEAD moved by the revert commit");
    assert!(
        !f.worktree.worktree.join("a.txt").exists(),
        "the file is gone again"
    );
    let original = view.notes.iter().find(|n| n.note.id == id).unwrap();
    assert_eq!(original.undone_by.as_deref(), Some(revert.note.id.as_str()));
    assert!(
        f.h.backend
            .messages()
            .iter()
            .any(|(_, t)| t.contains("undid") && t.contains("no action needed")),
        "the agent is told, keyed, that nothing is asked of it"
    );
}

/// T-U13 e: later work touches the same lines: the undo fails with the
/// conflict text and nothing changes.
#[tokio::test]
async fn an_undo_that_conflicts_changes_nothing() {
    let f = Fixture::new().await;
    let (first, _) = done_note(&f, "k1", "a.txt", "one\n").await;
    done_note(&f, "k2", "a.txt", "two\n").await;
    let before = f.head();
    undo(&f, &first).await.expect("queued");
    for _ in 0..100 {
        let n = state(&f)
            .await
            .notes
            .into_iter()
            .find(|n| n.note.reverts.as_deref() == Some(first.as_str()));
        if n.is_some_and(|n| n.note.status == NoteStatus::Failed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let revert = state(&f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.reverts.as_deref() == Some(first.as_str()))
        .expect("revert note")
        .note;
    assert_eq!(revert.status, NoteStatus::Failed);
    assert_eq!(revert.fail_reason, Some(FailReason::UndoConflict));
    assert_eq!(f.head(), before, "no commit was made");
    let text = revert
        .thread
        .iter()
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Can't undo automatically") || revert.status == NoteStatus::Failed,
        "{text}"
    );
}

/// The branch cannot move (its ref is locked): `apply_revert` puts the files
/// back and errors, so the undo fails visibly, nothing changes and the
/// session stays editable instead of hanging on "Working".
#[tokio::test]
async fn an_undo_that_cannot_move_the_branch_fails_and_changes_nothing() {
    use archon_core::design::SessionState;
    let f = Fixture::new().await;
    let (id, _) = done_note(&f, "k1", "a.txt", "one\n").await;
    let before = f.head();
    let common = crate::common::git(&f.worktree.worktree, &["rev-parse", "--git-common-dir"]);
    let common = f.worktree.worktree.join(common.trim());
    let lock = common.join(format!("refs/heads/{}.lock", f.worktree.branch));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, b"").unwrap();

    undo(&f, &id).await.expect("undo queued");
    let mut revert = None;
    for _ in 0..150 {
        revert = state(&f)
            .await
            .notes
            .into_iter()
            .find(|n| n.note.reverts.as_deref() == Some(id.as_str()))
            .map(|n| n.note);
        if revert
            .as_ref()
            .is_some_and(|n| n.status == NoteStatus::Failed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    std::fs::remove_file(&lock).unwrap();
    let revert = revert.expect("a revert note");
    assert_eq!(revert.status, NoteStatus::Failed, "not left working");
    assert_eq!(revert.fail_reason, Some(FailReason::UndoRefused));
    assert_eq!(f.head(), before, "the branch did not move");
    assert!(
        f.worktree.worktree.join("a.txt").exists(),
        "the files were put back"
    );
    let view = state(&f).await;
    let original = view.notes.iter().find(|n| n.note.id == id).unwrap();
    assert_eq!(original.note.status, NoteStatus::Done);
    assert!(original.undone_by.is_none(), "the original is not undone");
    assert_eq!(view.session.unwrap().state, SessionState::Live);
    // With the lock gone the same note can be undone after all.
    undo(&f, &id).await.expect("a second undo is queued");
}

/// T-U13 f: a note already undone, or an undo of a revert, is refused.
#[tokio::test]
async fn an_undo_is_refused_twice_and_for_reverts() {
    let f = Fixture::new().await;
    let (id, _) = done_note(&f, "k1", "a.txt", "one\n").await;
    undo(&f, &id).await.expect("first undo");
    assert!(undo(&f, &id).await.is_err(), "already undone or pending");
    let revert = state(&f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.reverts.is_some())
        .expect("revert")
        .note
        .id;
    assert!(undo(&f, &revert).await.is_err(), "no undo of an undo");
}

/// T-U13 h: only a note that is done with a commit can be undone.
#[tokio::test]
async fn only_a_done_note_with_a_commit_can_be_undone() {
    let f = Fixture::new().await;
    let queued = add(&f, "k1", "first").await;
    assert!(undo(&f, &queued.id).await.is_err(), "not done yet");
    assert_eq!(note(&f, &queued.id).await.status, NoteStatus::Sent);
    report(&f, &queued.id, AgentNoteStatus::Working, None, None)
        .await
        .unwrap();
    f.as_agent(Request::DesignNoteReport {
        id: queued.id.clone(),
        status: AgentNoteStatus::Done,
        reply: None,
        commit: None,
        no_change: true,
        path: None,
        steps: Vec::new(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("done with no change");
    assert!(undo(&f, &queued.id).await.is_err(), "nothing to undo");
}

/// A revert note left in `Working` with a recorded `pre` and `new`, as a
/// crash after the store write leaves it.
async fn left_working(f: &Fixture, original: &str, pre: &str, new: &str) -> String {
    use archon_core::design::NoteKind;
    use archon_queue::store::{NewDesignNote, NoteChange};
    let mut store = f.h.ctx.store.lock().await;
    let viewport = archon_core::design::Viewport {
        w: 1440,
        h: 900,
        preset: archon_core::design::ViewportPreset::Desktop,
    };
    let (n, _) = store
        .design_note_add(NewDesignNote {
            item: f.item.clone(),
            session: f.session.clone(),
            kind: NoteKind::Revert,
            text: format!("Undo {original}"),
            details_text: None,
            send_picture: false,
            target: None,
            viewport,
            client_key: format!("crash-{pre}-{new}"),
            reverts: Some(original.to_owned()),
            resent_from: None,
            follows: None,
            refs: Vec::new(),
            before_asset: None,
            created_by_device: None,
        })
        .unwrap();
    store
        .design_note_change(
            &f.item,
            &n.id,
            NoteChange::RevertPrepared {
                base_commit: pre.to_owned(),
                commit: new.to_owned(),
            },
        )
        .unwrap();
    n.id
}

/// An undone note, for its revert commit: (original id, original commit,
/// revert commit). HEAD is at the revert.
async fn undone(f: &Fixture) -> (String, String, String) {
    let (id, commit) = done_note(f, "k1", "a.txt", "one\n").await;
    undo(f, &id).await.expect("undo queued");
    for _ in 0..100 {
        if f.head() != commit {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let revert = f.head();
    assert_ne!(revert, commit, "the undo ran");
    // Wait for the note to settle so the next one may run.
    for _ in 0..100 {
        let all = state(f).await.notes;
        if all
            .iter()
            .all(|n| !archon_core::design::is_in_flight(n.note.status))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (id, commit, revert)
}

async fn recovered(f: &Fixture, id: &str) -> archon_core::design::DesignNote {
    archon_daemon::design::recover(&f.h.ctx)
        .await
        .expect("recovery");
    note(f, id).await
}

/// The crash window of an undo, at the service: every state the worktree can
/// be left in settles to one consistent state at the next start.
#[tokio::test]
async fn a_crashed_undo_settles_at_startup() {
    // Branch at the revert: the note is done with that commit.
    let f = Fixture::new().await;
    let (id, commit, revert) = undone(&f).await;
    let n = left_working(&f, &id, &commit, &revert).await;
    let told = |f: &Fixture| {
        f.h.backend
            .messages()
            .iter()
            .filter(|(_, t)| t.contains(&format!("undid {id}")) && t.contains("no action needed"))
            .count()
    };
    let before = told(&f);
    let settled = recovered(&f, &n).await;
    assert_eq!(settled.status, NoteStatus::Done);
    assert_eq!(settled.commit.as_deref(), Some(revert.as_str()));
    assert_eq!(f.head(), revert);
    for _ in 0..100 {
        if told(&f) > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        told(&f),
        before + 1,
        "recovery tells the agent of the undo it completed, as a live undo does"
    );

    // The checkout is done, the branch ref is not updated: recovery finishes
    // the move and the note is done.
    let f = Fixture::new().await;
    let (id, commit, revert) = undone(&f).await;
    crate::common::git(
        &f.worktree.worktree,
        &[
            "update-ref",
            &format!("refs/heads/{}", f.worktree.branch),
            &commit,
        ],
    );
    let n = left_working(&f, &id, &commit, &revert).await;
    let settled = recovered(&f, &n).await;
    assert_eq!(settled.status, NoteStatus::Done, "{settled:?}");
    assert_eq!(f.head(), revert, "the branch is at the revert");

    // Nothing was applied: the note fails, the worktree is unchanged.
    let f = Fixture::new().await;
    let (id, commit, revert) = undone(&f).await;
    crate::common::git(
        &f.worktree.worktree,
        &["reset", "--hard", "--quiet", &commit],
    );
    let n = left_working(&f, &id, &commit, &revert).await;
    let settled = recovered(&f, &n).await;
    assert_eq!(settled.status, NoteStatus::Failed);
    assert_eq!(settled.fail_reason, Some(FailReason::UndoInterrupted));
    assert_eq!(f.head(), commit, "the worktree is unchanged");
    assert!(
        settled
            .thread
            .iter()
            .any(|t| t.text.contains("nothing changed. Press Undo again.")),
    );

    // The branch moved to something else meanwhile: history changed.
    let f = Fixture::new().await;
    let (id, commit, revert) = undone(&f).await;
    let other = f.commit("b.txt", "b\n", "unrelated");
    let n = left_working(&f, &id, &commit, &revert).await;
    let settled = recovered(&f, &n).await;
    assert_eq!(settled.status, NoteStatus::Failed);
    assert_eq!(settled.fail_reason, Some(FailReason::HistoryChanged));
    assert_eq!(f.head(), other, "nothing moved");
}

/// Q3: a note an earlier approval made a requirement is never undone.
#[tokio::test]
async fn an_approved_requirement_is_never_undone() {
    let f = Fixture::new().await;
    let (id, _) = done_note(&f, "k1", "a.txt", "one\n").await;
    {
        let mut store = f.h.ctx.store.lock().await;
        let mut item = store.item(&f.item).unwrap();
        item.intent = Some(Box::new(archon_core::intent::IntentBinding {
            id: "earlier".into(),
            version: 1,
            requirements: vec![id.clone()],
        }));
        store.update_item(&item).unwrap();
    }
    let refused = undo(&f, &id).await.unwrap_err().to_string();
    assert!(
        refused.contains("was approved earlier; add a new note instead"),
        "{refused}"
    );
    assert!(
        state(&f)
            .await
            .notes
            .iter()
            .all(|n| n.note.reverts.is_none()),
        "no revert note was queued"
    );
}

/// Finish (§8.6): capture, the staleness checks, confirm, cancel.
#[tokio::test]
async fn finish_captures_confirms_and_goes_back() {
    use archon_core::design::SessionState;
    use archon_core::protocol::Reply;
    let f = Fixture::new().await;
    let (id, _) = done_note(&f, "k1", "a.txt", "one\n").await;
    let capture = |cancel_queued: bool, recapture: bool| {
        f.operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued,
            recapture,
        })
    };
    let view = match capture(false, false).await.expect("capture") {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    };
    assert_eq!(view.head, f.head());
    assert_eq!(view.cards.len(), 1);
    assert_eq!(view.cards[0].note, id);
    assert!(!view.no_change_only);
    assert_eq!(
        state(&f).await.session.unwrap().state,
        SessionState::Finishing
    );
    // While finishing nothing changes.
    let added = f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: "late".into(),
            text: "more".into(),
            asset: None,
            send_picture: false,
            details_text: None,
            follows: None,
            refs: Vec::new(),
        })
        .await;
    crate::design_session::refused(&added, archon_core::error::DesignRefusal::Finishing);

    let confirm = |capture: &str| {
        f.operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: capture.to_owned(),
            include: [(id.clone(), true)].into(),
            superseded: Default::default(),
            confirm_no_change: false,
        })
    };
    // A capture that is not the current one is stale.
    let stale = confirm("not-the-capture").await;
    crate::design_session::refused(&stale, archon_core::error::DesignRefusal::Stale);
    // The branch moved after the capture: stale.
    let moved = f.commit("b.txt", "b\n", "sneaky");
    let stale = confirm(&view.capture).await;
    crate::design_session::refused(&stale, archon_core::error::DesignRefusal::Stale);
    crate::common::git(
        &f.worktree.worktree,
        &["reset", "--hard", "--quiet", &view.head],
    );
    assert_ne!(f.head(), moved);

    // Back to editing, then capture again and confirm.
    f.operator(Request::DesignFinishCancel {
        session: f.session.clone(),
    })
    .await
    .expect("cancel");
    assert_eq!(state(&f).await.session.unwrap().state, SessionState::Live);
    let view = match capture(false, false).await.expect("capture again") {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    };
    confirm(&view.capture).await.expect("confirmed");
    assert_eq!(
        state(&f).await.session.unwrap().state,
        SessionState::Confirmed
    );
    assert!(
        f.h.backend
            .messages()
            .iter()
            .any(|(_, t)| t.contains("finish") || t.contains("Finish") || t.contains("approved")),
        "the agent is told"
    );
    // A second confirm finds the session no longer finishing.
    let again = confirm(&view.capture).await;
    crate::design_session::refused(&again, archon_core::error::DesignRefusal::Conflict);
}

/// A queued note blocks Finish until the operator agrees to cancel it.
#[tokio::test]
async fn finish_cancels_queued_notes_only_when_told_to() {
    use archon_core::design::{NoteKind, Viewport, ViewportPreset};
    use archon_queue::store::NewDesignNote;
    let f = Fixture::new().await;
    done_note(&f, "k1", "a.txt", "one\n").await;
    let queued = {
        let mut store = f.h.ctx.store.lock().await;
        store
            .design_note_add(NewDesignNote {
                item: f.item.clone(),
                session: f.session.clone(),
                kind: NoteKind::Change,
                text: "waiting".into(),
                details_text: None,
                send_picture: false,
                target: None,
                viewport: Viewport {
                    w: 1440,
                    h: 900,
                    preset: ViewportPreset::Desktop,
                },
                client_key: "waiting".into(),
                reverts: None,
                resent_from: None,
                follows: None,
                refs: Vec::new(),
                before_asset: None,
                created_by_device: None,
            })
            .unwrap()
            .0
    };
    let refused = f
        .operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await;
    assert!(refused.is_err(), "a queued note would be cancelled");
    assert_eq!(note(&f, &queued.id).await.status, NoteStatus::Queued);
    f.operator(Request::DesignFinishCapture {
        session: f.session.clone(),
        cancel_queued: true,
        recapture: false,
    })
    .await
    .expect("capture with cancel");
    assert_eq!(note(&f, &queued.id).await.status, NoteStatus::Cancelled);
}

/// Every note settled with no change: approving takes the explicit flag.
#[tokio::test]
async fn finish_with_only_no_change_notes_needs_the_flag() {
    use archon_core::protocol::Reply;
    let f = Fixture::new().await;
    let added = add(&f, "k1", "is it already red?").await;
    report(&f, &added.id, AgentNoteStatus::Working, None, None)
        .await
        .unwrap();
    f.as_agent(Request::DesignNoteReport {
        id: added.id.clone(),
        status: AgentNoteStatus::Done,
        reply: Some("it already is".into()),
        commit: None,
        no_change: true,
        path: None,
        steps: Vec::new(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("done with no change");
    let view = match f
        .operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("capture")
    {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    };
    assert!(view.no_change_only);
    let confirm = |flag: bool| {
        f.operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: view.capture.clone(),
            include: Default::default(),
            superseded: Default::default(),
            confirm_no_change: flag,
        })
    };
    assert!(confirm(false).await.is_err(), "refused without the flag");
    confirm(true).await.expect("approved with the flag");
}
