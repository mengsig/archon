//! Undo (decision B): archond reverts a note's commit itself, through the
//! shadow and sealed git, when the agent is idle. The agent never does.

use archon_core::design::{
    DesignNote, DesignSession, FailReason, NoteStatus, ReplyAuthor, ThreadEntry,
};
use archon_core::error::ArchonError;
use archon_queue::store::NoteChange;

use super::delivery::{Owed, queued_payload};
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

    pub(super) async fn fail_revert(
        &self,
        ctx: &DaemonContext,
        revert: &DesignNote,
        reason: FailReason,
        text: String,
    ) -> Result<(), ArchonError> {
        assert_eq!(revert.kind, archon_core::design::NoteKind::Revert);
        assert!(!text.is_empty(), "every refusal explains what happened");
        ctx.store.lock().await.design_note_transition(
            &revert.item,
            &revert.id,
            super::status_change(NoteStatus::Failed, Some(reason)),
            Some(ThreadEntry {
                at: archon_core::now(),
                from: ReplyAuthor::Archon,
                text,
            }),
            None,
        )?;
        self.emit_note(ctx, &revert.item, &revert.id).await;
        // A continue stops at its first failed revert: nothing after it runs.
        self.cancel_continue(ctx, revert).await;
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
            .as_deref()
            .ok_or_else(|| ArchonError::Corrupt("a revert note names its original".into()))?;
        let original = ctx
            .store
            .lock()
            .await
            .design_note(&revert.item, original_id)?
            .ok_or_else(|| ArchonError::Corrupt(format!("{original_id} is gone")))?;
        let Some(commit) = original.commit else {
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
        let change = match prepared_change(prep, original_id) {
            Ok(change) => change,
            Err((reason, why)) => return self.fail_revert(ctx, revert, reason, why).await,
        };
        let prepared =
            ctx.store
                .lock()
                .await
                .design_note_change(&revert.item, &revert.id, change)?;
        self.emit_note(ctx, &revert.item, &revert.id).await;
        self.apply_prepared(ctx, session, &prepared, &wt, &g).await
    }

    async fn apply_prepared(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert: &DesignNote,
        wt: &WorktreeRef,
        git: &NoteGit<'_>,
    ) -> Result<(), ArchonError> {
        assert_eq!(revert.status, NoteStatus::Working);
        assert_eq!(revert.session, session.id);
        let original = revert
            .reverts
            .as_deref()
            .expect("a prepared undo names its original");
        let pre = revert
            .base_commit
            .as_deref()
            .expect("stored before applying");
        let new = revert.commit.as_deref().expect("stored before applying");
        match design_git::apply_revert(git, pre, new) {
            Ok(RevertApplied::Applied) => {
                self.settle_revert_done(ctx, session, revert, original, new, wt)
                    .await
            }
            Ok(RevertApplied::HistoryChanged(why)) => {
                tracing::info!(
                    note = original,
                    why,
                    "design: undo found the history changed"
                );
                self.fail_revert(
                    ctx,
                    revert,
                    FailReason::HistoryChanged,
                    "The code history changed while undoing; check the preview.".into(),
                )
                .await
            }
            Err(error) => {
                tracing::warn!(note = original, %error, "design: settling an unsuccessful undo");
                let head = crate::worktree::stack::sha(&wt.repo, &wt.branch)?;
                if head != pre {
                    return super::recovery::settle_working_revert(ctx, session, revert).await;
                }
                let (reason, message) = match self.worktree_at(wt, pre)? {
                    Ok(()) => (
                        FailReason::UndoRefused,
                        format!("Undo did not finish, so nothing changed: {error}"),
                    ),
                    Err(why) => (
                        FailReason::UndoInterrupted,
                        format!(
                            "Undo did not finish and its files could not all be put back; check the preview: {why} ({error})"
                        ),
                    ),
                };
                self.fail_revert(ctx, revert, reason, message).await
            }
        }
    }

    /// The revert is in the code: the note is done, the agent is told.
    async fn settle_revert_done(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert: &DesignNote,
        original_id: &str,
        new: &str,
        wt: &WorktreeRef,
    ) -> Result<(), ArchonError> {
        self.record_revert_done(ctx, session, revert, original_id, new)
            .await?;
        tracing::info!(note = original_id, undo = %revert.id, commit = new, device = ?super::current_device_label(), "design: undone");
        self.emit_note(ctx, &revert.item, &revert.id).await;
        let mut for_picture = revert.clone();
        for_picture.target = ctx
            .store
            .lock()
            .await
            .design_note(&revert.item, original_id)?
            .and_then(|n| n.target);
        if let super::state::AfterOutcome::StepFailed(why) = self
            .after_picture(ctx, session, &for_picture, wt, Default::default())
            .await
        {
            tracing::warn!(note = %revert.id, why, "design: the undo picture could not be taken");
        }
        self.emit_note(ctx, &revert.item, &revert.id).await;
        Ok(())
    }

    pub(super) async fn record_revert_done(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        revert: &DesignNote,
        original_id: &str,
        new: &str,
    ) -> Result<(), ArchonError> {
        assert_eq!(revert.session, session.id);
        assert_eq!(revert.reverts.as_deref(), Some(original_id));
        let message = Owed {
            key: format!("design:{}:{}:told", session.id, revert.id),
            text: format!(
                "Archon undid {original_id} with commit {new}; HEAD is now {new}; no action needed"
            ),
            asset: None,
            item: session.item.clone(),
            more: Vec::new(),
        };
        let mut store = ctx.store.lock().await;
        let owed = queued_payload(&store, session, message)?;
        store.design_revert_done(session, &revert.id, new, &owed)
    }
}

fn prepared_change(
    prep: Result<RevertPrep, ArchonError>,
    original: &str,
) -> Result<NoteChange, (FailReason, String)> {
    match prep {
        Ok(RevertPrep::Ready { pre, new }) => Ok(NoteChange::RevertPrepared {
            base_commit: pre,
            commit: new,
        }),
        Ok(RevertPrep::Conflict(files)) => {
            tracing::info!(note = original, ?files, "design: undo conflicts");
            Err((FailReason::UndoConflict, CONFLICT_TEXT.into()))
        }
        Ok(RevertPrep::Refused(why)) => Err((FailReason::UndoRefused, why)),
        Err(error) => Err((
            FailReason::UndoRefused,
            format!("Undo was not possible: {error}"),
        )),
    }
}
