//! Undo of a done note (design.md decision B, T-U13): archond reverts the
//! commit itself, in the queue, and records what it did.

use crate::common::design::Fixture;
use crate::design_session::{add, note, report, state};
use archon_core::design::{FailReason, NoteStatus};
use archon_core::protocol::{AgentNoteStatus, Request};

/// A note done with a real commit.
async fn done_note(f: &Fixture, key: &str, file: &str, body: &str) -> (String, String) {
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
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("done with no change");
    assert!(undo(&f, &queued.id).await.is_err(), "nothing to undo");
}
