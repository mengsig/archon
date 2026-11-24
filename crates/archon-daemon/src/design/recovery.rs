//! Start-up: reap browsers a dead archond left, sweep their profiles, and
//! settle the undos it was in the middle of.

use archon_core::design::{
    DesignNote, FailReason, NoteKind, NoteStatus, ReplyAuthor, SessionState, ThreadEntry,
};
use archon_core::error::ArchonError;
use archon_design_browser as adb;
use archon_queue::store::{NoteChange, SessionPatch};

use crate::context::DaemonContext;
use crate::worktree::design_git::{self, NoteGit, Reconciled};

pub async fn recover(ctx: &DaemonContext) -> Result<(), ArchonError> {
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
    reap_recorded_browsers(ctx).await?;
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
    // A session whose run already finished or was cancelled has nothing left
    // to wait for, including a Confirmed one a cancel left behind.
    for session in &sessions {
        let terminal = {
            let store = ctx.store.lock().await;
            match store.run(&session.run) {
                Ok(run) => run.state.is_terminal(),
                Err(ArchonError::NotFound(_)) => true,
                Err(error) => return Err(error),
            }
        };
        if terminal && let Err(error) = service.end_session(ctx, session, "run ended").await {
            tracing::warn!(%error, session = %session.id, "design: a session of a finished run was not ended");
        }
    }
    Ok(())
}

async fn reap_recorded_browsers(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let sessions = ctx.store.lock().await.design_sessions_with_process()?;
    for session in sessions {
        let (pid, started) = session
            .browser_pid
            .zip(session.browser_started_at)
            .ok_or_else(|| {
                ArchonError::Unavailable(format!(
                    "incomplete browser process identity for {}",
                    session.id
                ))
            })?;
        let killed = adb::kill_if_matches(pid, started)
            .map_err(|error| ArchonError::io("reap recorded preview browser", error))?;
        tracing::info!(session = %session.id, pid, killed, "design: checked recorded browser");
        ctx.store.lock().await.design_session_patch(
            &session.id,
            SessionPatch {
                browser: Some(None),
                ..Default::default()
            },
        )?;
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
                    commit: Some(new.clone()),
                    no_change: false,
                },
            )?;
            // As a live undo does: the agent learns HEAD moved under it.
            service
                .tell_undone(ctx, session, &revert.id, &original, &new)
                .await;
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
    drop(store);
    // As a live failed undo does: the rest of its continue stops.
    ctx.design.cancel_continue(ctx, revert).await;
    Ok(())
}
