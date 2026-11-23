//! Undo (decision B): archond reverts a note's commit itself, through the
//! shadow and sealed git, when the agent is idle. The agent never does.

use archon_core::design::{
    DesignNote, DesignSession, FailReason, NoteStatus, ReplyAuthor, ThreadEntry,
};
use archon_core::error::ArchonError;
use archon_queue::store::NoteChange;

use super::{DesignService, WorktreeRef};
use crate::context::DaemonContext;
use crate::executor::repo_identity;
use crate::worktree::design_git::{self, NoteGit, RevertApplied, RevertPrep};

pub(crate) const CONFLICT_TEXT: &str = "Can't undo automatically, later changes touch the same code; add a note describing what to change back.";

impl DesignService {
    /// The commit identity archond's own merges use in the session's repository.
    async fn merger(
        &self,
        ctx: &DaemonContext,
        wt: &WorktreeRef,
    ) -> Result<crate::worktree::stack::Identity, ArchonError> {
        let repo = {
            let store = ctx.store.lock().await;
            store.repos()?.into_iter().find(|r| r.path == wt.repo)
        };
        let identity = match repo {
            Some(repo) => repo_identity::of(ctx, &repo).await?,
            None => None,
        };
        Ok(repo_identity::RepoIdentity::merger(identity.as_ref()))
    }

    async fn fail_revert(
        &self,
        ctx: &DaemonContext,
        revert: &DesignNote,
        reason: FailReason,
        text: String,
    ) -> Result<(), ArchonError> {
        {
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
        }
        self.emit_note(ctx, &revert.item, &revert.id).await;
        Ok(())
    }

    /// Run the revert note at the head of the queue.
    pub(crate) async fn run_undo(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert: &DesignNote,
    ) -> Result<(), ArchonError> {
        let original_id = revert
            .reverts
            .clone()
            .ok_or_else(|| ArchonError::Corrupt("a revert note names its original".into()))?;
        let original = ctx
            .store
            .lock()
            .await
            .design_note(&revert.item, &original_id)?
            .ok_or_else(|| ArchonError::Corrupt(format!("{original_id} is gone")))?;
        let Some(commit) = original.commit.clone() else {
            return self
                .fail_revert(
                    ctx,
                    revert,
                    FailReason::UndoRefused,
                    format!("{original_id} has no commit to undo"),
                )
                .await;
        };
        let wt = self.worktree_of(ctx, session).await?;
        let identity = self.merger(ctx, &wt).await?;
        let shadow = self.shadow(&wt)?;
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        let prep = design_git::prepare_revert(&g, &commit, &revert.id, &identity);
        let (pre, new) = match prep {
            Ok(RevertPrep::Conflict(files)) => {
                tracing::info!(note = %original_id, ?files, "design: undo conflicts");
                return self
                    .fail_revert(ctx, revert, FailReason::UndoConflict, CONFLICT_TEXT.into())
                    .await;
            }
            Ok(RevertPrep::Refused(why)) => {
                return self
                    .fail_revert(ctx, revert, FailReason::UndoRefused, why)
                    .await;
            }
            Err(error) => {
                return self
                    .fail_revert(
                        ctx,
                        revert,
                        FailReason::UndoRefused,
                        format!("Undo was not possible: {error}"),
                    )
                    .await;
            }
            Ok(RevertPrep::Ready { pre, new }) => (pre, new),
        };
        // Stored before the branch moves, so a restart can settle it.
        ctx.store.lock().await.design_note_change(
            &revert.item,
            &revert.id,
            NoteChange::RevertPrepared {
                base_commit: pre.clone(),
                commit: new.clone(),
            },
        )?;
        self.emit_note(ctx, &revert.item, &revert.id).await;
        let applied = match design_git::apply_revert(&g, &pre, &new) {
            Ok(applied) => applied,
            Err(error) => {
                // `apply_revert` puts the files back when it fails before the
                // branch moved: nothing changed, so the undo fails visibly
                // and the session is editable again. A branch that did move
                // is left for startup's reconcile, as before.
                if crate::worktree::stack::sha(&wt.repo, &wt.branch)
                    .ok()
                    .as_deref()
                    != Some(pre.as_str())
                {
                    return Err(error);
                }
                // "Nothing changed" only when the files really are back at
                // the commit the undo started from; a restore that failed
                // too is said plainly, so the operator checks the preview.
                if matches!(self.worktree_at(&wt, &pre), Ok(Ok(()))) {
                    tracing::warn!(note = %original_id, %error, "design: undo failed; nothing changed");
                    return self
                        .fail_revert(
                            ctx,
                            revert,
                            FailReason::UndoRefused,
                            format!("Undo did not finish, so nothing changed: {error}"),
                        )
                        .await;
                }
                tracing::warn!(note = %original_id, %error, "design: undo failed and its files were not all restored");
                return self
                    .fail_revert(
                        ctx,
                        revert,
                        FailReason::UndoInterrupted,
                        format!(
                            "Undo did not finish and its files could not all be put back; check \
                             the preview and add a note if something looks wrong: {error}"
                        ),
                    )
                    .await;
            }
        };
        match applied {
            RevertApplied::HistoryChanged(why) => {
                tracing::info!(note = %original_id, why, "design: undo found the history changed");
                self.fail_revert(
                    ctx,
                    revert,
                    FailReason::HistoryChanged,
                    "The code history changed while undoing; check the preview.".into(),
                )
                .await
            }
            RevertApplied::Applied => {
                self.settle_revert_done(ctx, session, revert, &original_id, &new, &wt)
                    .await
            }
        }
    }

    /// The revert is in the code: the note is done, the agent is told.
    pub(crate) async fn settle_revert_done(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert: &DesignNote,
        original_id: &str,
        new: &str,
        wt: &WorktreeRef,
    ) -> Result<(), ArchonError> {
        ctx.store.lock().await.design_note_change(
            &revert.item,
            &revert.id,
            NoteChange::Status {
                to: NoteStatus::Done,
                fail_reason: None,
                commit: Some(new.to_owned()),
                no_change: false,
            },
        )?;
        tracing::info!(note = original_id, undo = %revert.id, commit = new, device = ?super::current_device_label(), "design: undone");
        self.emit_note(ctx, &revert.item, &revert.id).await;
        let mut for_picture = revert.clone();
        for_picture.target = ctx
            .store
            .lock()
            .await
            .design_note(&revert.item, original_id)?
            .and_then(|n| n.target);
        self.after_picture(ctx, session, &for_picture, wt, Default::default())
            .await;
        self.emit_note(ctx, &revert.item, &revert.id).await;
        self.tell_undone(ctx, session, &revert.id, original_id, new)
            .await;
        Ok(())
    }

    /// Tell the agent an undo is in the code, keyed by the revert note so a
    /// live undo and startup's settling of the same one send it once.
    pub(crate) async fn tell_undone(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert_id: &str,
        original_id: &str,
        new: &str,
    ) {
        let key = format!("design:{}:{revert_id}:told", session.id);
        let text = format!(
            "Archon undid {original_id} with commit {new}; HEAD is now {new}; no action needed"
        );
        if let Err(error) = self.owe(ctx, session, key, text, None).await {
            tracing::warn!(%error, "the undo notice was not queued");
        }
    }
}
