//! Start-up: reap browsers a dead archond left, sweep their profiles, and
//! settle the undos it was in the middle of.

use archon_core::design::{
    DesignNote, FailReason, NoteKind, NoteStatus, ReplyAuthor, SessionState, ThreadEntry,
};
use archon_core::error::ArchonError;
use archon_design_browser as adb;
use archon_queue::store::design::{NoteChange, SessionPatch};

use crate::context::DaemonContext;
use crate::worktree::design_git::{self, NoteGit, Reconciled};

pub(crate) async fn recover(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let service = &ctx.design;
    let sessions = {
        let store = ctx.store.lock().await;
        store.design_sessions_in(&[
            SessionState::Starting,
            SessionState::Live,
            SessionState::Finishing,
            SessionState::Confirmed,
        ])?
    };
    for session in &sessions {
        if let (Some(pid), Some(started)) = (session.browser_pid, session.browser_started_at) {
            let killed = adb::kill_if_matches(pid, started);
            tracing::info!(session = %session.id, pid, killed, "design: recorded browser reaped");
            let mut store = ctx.store.lock().await;
            store.design_session_patch(
                &session.id,
                SessionPatch {
                    browser: Some(None),
                    ..Default::default()
                },
            )?;
        }
    }
    let root = service.data_dir.join("design").join("profiles");
    if root.is_dir() {
        match adb::sweep_profiles(&root, &[]) {
            Ok(n) => tracing::info!(removed = n, "design: stale profiles swept"),
            Err(error) => tracing::warn!(%error, "design: sweeping profiles failed"),
        }
    }
    for session in &sessions {
        let notes = ctx.store.lock().await.design_notes(&session.item)?;
        for note in notes.iter().filter(|n| {
            n.session == session.id && n.kind == NoteKind::Revert && n.status == NoteStatus::Working
        }) {
            if let Err(error) = settle_working_revert(ctx, session, note).await {
                tracing::warn!(%error, note = %note.id, "design: an interrupted undo was not settled");
            }
        }
    }
    Ok(())
}

async fn settle_working_revert(
    ctx: &DaemonContext,
    session: &archon_core::design::DesignSession,
    revert: &DesignNote,
) -> Result<(), ArchonError> {
    let service = &ctx.design;
    let original = revert.reverts.clone().unwrap_or_default();
    let (Some(pre), Some(new)) = (revert.base_commit.clone(), revert.commit.clone()) else {
        return fail(
            ctx,
            revert,
            FailReason::UndoInterrupted,
            format!("Undo of {original} was interrupted; nothing changed. Press Undo again."),
        )
        .await;
    };
    let wt = service.worktree_of(ctx, session).await?;
    let shadow = service.shadow(&wt)?;
    let g = NoteGit {
        shadow: &shadow,
        repo: &wt.repo,
        worktree: &wt.worktree,
        branch: &wt.branch,
    };
    match design_git::reconcile_revert(&g, &pre, &new)? {
        Reconciled::Done | Reconciled::Completed => {
            ctx.store.lock().await.design_note_change(
                &revert.item,
                &revert.id,
                NoteChange::Status {
                    to: NoteStatus::Done,
                    fail_reason: None,
                    commit: Some(new),
                    no_change: false,
                },
            )?;
            Ok(())
        }
        Reconciled::NotApplied => {
            fail(
                ctx,
                revert,
                FailReason::UndoInterrupted,
                format!("Undo of {original} was interrupted; nothing changed. Press Undo again."),
            )
            .await
        }
        Reconciled::HistoryChanged(_) => {
            fail(
                ctx,
                revert,
                FailReason::HistoryChanged,
                format!("Undo of {original} was interrupted; check the preview"),
            )
            .await
        }
    }
}

async fn fail(
    ctx: &DaemonContext,
    revert: &DesignNote,
    reason: FailReason,
    text: String,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    store.design_note_change(
        &revert.item,
        &revert.id,
        NoteChange::Thread(ThreadEntry {
            at: archon_core::now(),
            from: ReplyAuthor::Archon,
            text,
        }),
    )?;
    store.design_note_change(
        &revert.item,
        &revert.id,
        super::status_change(NoteStatus::Failed, Some(reason)),
    )?;
    Ok(())
}
