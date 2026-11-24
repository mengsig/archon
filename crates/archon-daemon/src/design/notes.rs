//! Operator requests on notes: add, edit, cancel, reply, drop, undo, send
//! again and tell the agent again.

use archon_core::design::{
    DesignAssetId, DesignNote, DesignSessionId, MAX_DETAILS_CHARS, MAX_NOTE_CHARS,
    MAX_NOTES_PER_SESSION, MAX_REPLY_CHARS, NoteKind, NoteStatus, ReplyAuthor, ThreadEntry,
    Viewport, clean_text, is_in_flight, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::id::QueueItemId;
use archon_core::protocol::Reply;
use archon_queue::store::{NewDesignNote, NoteChange};

use super::delivery::note_message;
use super::{DesignService, note_view, refuse, status_change};
use crate::context::DaemonContext;

impl DesignService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn note_add(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        client_key: String,
        text: String,
        asset: Option<DesignAssetId>,
        send_picture: bool,
        details_text: Option<String>,
        follows: Option<String>,
        refs: Vec<String>,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session).await?;
        let row = self.current_for_notes(ctx, &row.item).await.map(|_| row)?;
        let text = clean_text(&text, MAX_NOTE_CHARS);
        if text.is_empty() {
            return Err(ArchonError::Invalid("a note needs words".into()));
        }
        if client_key.trim().is_empty()
            || client_key.len() > 256
            || client_key.chars().any(char::is_control)
        {
            return Err(ArchonError::Invalid(
                "a note needs a client key of up to 256 bytes without control characters".into(),
            ));
        }
        let details = details_text
            .map(|d| clean_text(&d, MAX_DETAILS_CHARS))
            .filter(|d| !d.is_empty());
        let device = super::current_device().map(|d| d.id);
        let (note, created) = {
            let mut store = ctx.store.lock().await;
            let existing = store.design_notes(&row.item)?;
            // A repeat of a note already added is answered as it stands,
            // whatever happened to its chain since.
            if let Some(found) = existing.iter().find(|n| n.client_key == client_key) {
                let id = found.id.clone();
                drop(store);
                return self.note_reply(ctx, &row.item, &id).await;
            }
            if existing.iter().filter(|n| n.session == row.id).count() >= MAX_NOTES_PER_SESSION {
                return Err(ArchonError::Invalid(format!(
                    "a session holds at most {MAX_NOTES_PER_SESSION} notes"
                )));
            }
            let refs = super::iterations::check_refs(refs, &existing)?;
            let parent = match follows.as_deref() {
                Some(follows) => {
                    Some(super::iterations::check_follows(follows, &existing, &[])?.clone())
                }
                None => None,
            };
            if let (Some(parent), None) = (&parent, &asset) {
                // A follow-up with no picture of its own points where its
                // version did and starts from that version's picture.
                let mut new = super::iterations::follow_up(
                    &row.item, &row.id, parent, text, client_key, refs, device,
                );
                new.details_text = details;
                store.design_note_add(new)?
            } else {
                // Only a picture this server stored for this item is accepted.
                let stored = match &asset {
                    Some(id) => Some(store.design_asset(&row.item, id)?.ok_or_else(|| {
                        ArchonError::Invalid(
                            "that picture was not stored by Archon for this item".into(),
                        )
                    })?),
                    None => None,
                };
                let target = stored.as_ref().and_then(|a| a.target.clone());
                let kind = if target.is_some() {
                    NoteKind::Change
                } else {
                    NoteKind::Page
                };
                let viewport = target.as_ref().map(|t| t.viewport).unwrap_or_else(|| {
                    let (w, h) = row.viewport_preset.size();
                    Viewport {
                        w,
                        h,
                        preset: row.viewport_preset,
                    }
                });
                store.design_note_add(NewDesignNote {
                    item: row.item.clone(),
                    session: row.id.clone(),
                    kind,
                    text,
                    details_text: details,
                    send_picture: send_picture && stored.is_some(),
                    target,
                    viewport,
                    client_key,
                    reverts: None,
                    resent_from: None,
                    follows: parent.map(|p| p.id),
                    refs,
                    before_asset: stored.map(|a| a.id),
                    created_by_device: device,
                })?
            }
        };
        if created {
            tracing::info!(note = %note.id, item = %note.item, device = ?super::current_device_label(), "design: note added");
            self.emit_note(ctx, &note.item, &note.id).await;
            self.pump(ctx, &note.item).await?;
        }
        self.note_reply(ctx, &note.item, &note.id).await
    }

    pub(crate) async fn note_reply(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Reply, ArchonError> {
        let store = ctx.store.lock().await;
        let note = store
            .design_note(item, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("note {id}")))?;
        let all = store.design_notes(item)?;
        let current = store.design_current_session(item)?;
        let approved = super::approved_ids(&store, item)?;
        Ok(Reply::DesignNote(Box::new(note_view(
            &note,
            &all,
            current.as_ref(),
            &approved,
        ))))
    }

    /// A note of the item's current session, for a change request.
    async fn own_note(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<(archon_core::design::DesignSession, DesignNote), ArchonError> {
        let session = self.current_for_notes(ctx, item).await?;
        let note = ctx
            .store
            .lock()
            .await
            .design_note(item, id)?
            .ok_or_else(|| ArchonError::NotFound(format!("note {id}")))?;
        if note.session != session.id {
            return Err(refuse(
                DesignRefusal::ReadOnly,
                format!("{id} belongs to an earlier session and cannot change"),
            ));
        }
        Ok((session, note))
    }

    pub(crate) async fn note_edit(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
        text: String,
    ) -> Result<Reply, ArchonError> {
        // The delivery pump holds this lock from copying a queued note to
        // marking it sent, so an edit cannot land in between.
        let lock = self.item_lock(item);
        let _held = lock.lock().await;
        let (_, note) = self.own_note(ctx, item, id).await?;
        if note.status != NoteStatus::Queued || note.kind == NoteKind::Revert {
            return Err(ArchonError::Invalid(
                "only a queued note can be edited".into(),
            ));
        }
        let text = clean_text(&text, MAX_NOTE_CHARS);
        if text.is_empty() {
            return Err(ArchonError::Invalid("a note needs words".into()));
        }
        ctx.store
            .lock()
            .await
            .design_note_change(item, id, NoteChange::Edit { text })?;
        tracing::info!(note = id, %item, device = ?super::current_device_label(), "design: note edited");
        self.emit_note(ctx, item, id).await;
        self.note_reply(ctx, item, id).await
    }

    pub(crate) async fn note_cancel(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Reply, ArchonError> {
        let lock = self.item_lock(item);
        let _held = lock.lock().await;
        let (_, note) = self.own_note(ctx, item, id).await?;
        if note.status != NoteStatus::Queued {
            return Err(ArchonError::Invalid(
                "only a queued note can be cancelled".into(),
            ));
        }
        ctx.store.lock().await.design_note_change(
            item,
            id,
            status_change(NoteStatus::Cancelled, None),
        )?;
        tracing::info!(note = id, %item, device = ?super::current_device_label(), "design: note cancelled");
        self.emit_note(ctx, item, id).await;
        drop(_held);
        self.note_reply(ctx, item, id).await
    }

    pub(crate) async fn note_answer(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
        text: String,
    ) -> Result<Reply, ArchonError> {
        let (session, note) = self.own_note(ctx, item, id).await?;
        if note.status != NoteStatus::NeedsReply {
            return Err(ArchonError::Invalid(
                "that note is not waiting for an answer".into(),
            ));
        }
        let text = clean_text(&text, MAX_REPLY_CHARS);
        if text.is_empty() {
            return Err(ArchonError::Invalid("an answer needs words".into()));
        }
        let key = format!(
            "design:{}:{}:{}:reply:{}",
            session.id,
            id,
            note.delivery,
            note.thread.len()
        );
        self.owe_queue(
            ctx,
            &session,
            key,
            format!("Operator's answer on {id}: \"{text}\""),
            None,
        )
        .await?;
        {
            let mut store = ctx.store.lock().await;
            store.design_note_change(
                item,
                id,
                NoteChange::Thread(ThreadEntry {
                    at: archon_core::now(),
                    from: ReplyAuthor::Operator,
                    text,
                }),
            )?;
            store.design_note_change(item, id, status_change(NoteStatus::Working, None))?;
        }
        self.emit_note(ctx, item, id).await;
        self.flush_owed(ctx, &session).await?;
        self.note_reply(ctx, item, id).await
    }

    pub(crate) async fn note_drop(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Reply, ArchonError> {
        let (session, note) = self.own_note(ctx, item, id).await?;
        if !is_in_flight(note.status) || note.kind == NoteKind::Revert {
            return Err(ArchonError::Invalid(
                "that note is not with the agent".into(),
            ));
        }
        let waited = archon_core::now() - note.updated_at;
        let offered = note.status == NoteStatus::NeedsReply
            || waited >= chrono::Duration::minutes(archon_core::design::DROP_OFFERED_AFTER_MINUTES);
        if !offered {
            return Err(ArchonError::Invalid(
                "a note can be dropped once it needs an answer or has worked 15 minutes".into(),
            ));
        }
        let key = format!("design:{}:{}:{}:drop", session.id, id, note.delivery);
        self.owe(
            ctx,
            &session,
            key,
            format!("Drop {id}: discard your edits and report cancelled"),
            None,
        )
        .await?;
        tracing::info!(note = id, %item, device = ?super::current_device_label(), "design: note dropped");
        self.note_reply(ctx, item, id).await
    }

    /// `design_note_undo`: queue a revert note in the current session.
    pub(crate) async fn note_undo(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Reply, ArchonError> {
        let session = self.current_for_notes(ctx, item).await?;
        let device = super::current_device().map(|d| d.id);
        let revert = {
            let mut store = ctx.store.lock().await;
            let notes = store.design_notes(item)?;
            let note = notes
                .iter()
                .find(|n| n.id == id)
                .ok_or_else(|| ArchonError::NotFound(format!("note {id}")))?;
            if note.kind == NoteKind::Revert {
                return Err(ArchonError::Invalid("an undo cannot be undone".into()));
            }
            if note.status != NoteStatus::Done || note.commit.is_none() || note.no_change {
                return Err(ArchonError::Invalid(format!("{id} has no change to undo")));
            }
            if undone_by(&notes).contains_key(id) {
                return Err(ArchonError::Invalid(format!("{id} is already undone")));
            }
            let pending = notes.iter().any(|n| {
                n.kind == NoteKind::Revert
                    && n.reverts.as_deref() == Some(id)
                    && matches!(n.status, NoteStatus::Queued | NoteStatus::Working)
            });
            if pending {
                return Err(ArchonError::Invalid(format!(
                    "an undo of {id} is already waiting"
                )));
            }
            let approved = store
                .item(item)?
                .intent
                .is_some_and(|binding| binding.requirements.iter().any(|r| r == id));
            if approved {
                return Err(ArchonError::Invalid(format!(
                    "{id} was approved earlier; add a new note instead"
                )));
            }
            let (_, created) = store.design_note_add(NewDesignNote {
                item: item.clone(),
                session: session.id.clone(),
                kind: NoteKind::Revert,
                text: format!("Undo {id}"),
                details_text: None,
                send_picture: false,
                target: None,
                viewport: note.viewport,
                client_key: format!("undo:{id}:{}", notes.len()),
                reverts: Some(id.to_owned()),
                resent_from: None,
                follows: None,
                refs: Vec::new(),
                before_asset: None,
                created_by_device: device,
            })?;
            debug_assert!(created);
            store
                .design_notes(item)?
                .into_iter()
                .rfind(|n| n.reverts.as_deref() == Some(id))
                .ok_or_else(|| ArchonError::Corrupt("the undo note was not stored".into()))?
        };
        tracing::info!(note = id, undo = %revert.id, %item, device = ?super::current_device_label(), "design: undo queued");
        self.emit_note(ctx, item, &revert.id).await;
        self.pump(ctx, item).await?;
        self.note_reply(ctx, item, &revert.id).await
    }

    /// `design_note_resend`: copy an undelivered note of an earlier session.
    pub(crate) async fn note_resend(
        &self,
        ctx: &DaemonContext,
        item: &QueueItemId,
        id: &str,
    ) -> Result<Reply, ArchonError> {
        let session = self.current_for_notes(ctx, item).await?;
        let device = super::current_device().map(|d| d.id);
        let created = {
            let mut store = ctx.store.lock().await;
            let note = store
                .design_note(item, id)?
                .ok_or_else(|| ArchonError::NotFound(format!("note {id}")))?;
            if note.fail_reason != Some(archon_core::design::FailReason::NotDelivered)
                || note.kind == NoteKind::Revert
            {
                return Err(ArchonError::Invalid(
                    "only an undelivered note can be sent again".into(),
                ));
            }
            // The copy keeps its place in the chain and its references
            // while they still hold.
            let all = store.design_notes(item)?;
            let follows = note.follows.as_deref().and_then(|parent| {
                super::iterations::check_follows(parent, &all, &[])
                    .ok()
                    .map(|p| p.id.clone())
            });
            let refs = super::iterations::check_refs(note.refs.clone(), &all).unwrap_or_default();
            let (created, _) = store.design_note_add(NewDesignNote {
                item: item.clone(),
                session: session.id.clone(),
                kind: note.kind,
                text: format!("{} (Re-sent from {id})", note.text),
                details_text: note.details_text.clone(),
                send_picture: note.send_picture,
                target: note.target.clone(),
                viewport: note.viewport,
                client_key: format!("resend:{id}:{}", session.id),
                reverts: None,
                resent_from: Some(id.to_owned()),
                follows,
                refs,
                before_asset: note.before_asset.clone(),
                created_by_device: device,
            })?;
            created
        };
        self.emit_note(ctx, item, &created.id).await;
        self.pump(ctx, item).await?;
        self.note_reply(ctx, item, &created.id).await
    }

    /// `design_tell_agent`: send the note in flight again (delivery + 1), or
    /// say the preview is unreachable when it is.
    pub(crate) async fn tell_agent(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
    ) -> Result<Reply, ArchonError> {
        let session = self.current(ctx, session_id).await?;
        let session = self
            .current_for_notes(ctx, &session.item)
            .await
            .map(|_| session)?;
        let notes = ctx.store.lock().await.design_notes(&session.item)?;
        let sent = notes.iter().find(|n| {
            n.session == session.id && n.status == NoteStatus::Sent && n.kind != NoteKind::Revert
        });
        if let Some(note) = sent {
            let delivery = note.delivery + 1;
            let wt = self.worktree_of(ctx, &session).await?;
            let head = self.head_of(&wt)?;
            let key = format!("design:{}:{}:{}", session.id, note.id, delivery);
            let (text, more) = note_message(&session, note, &head, &notes);
            let asset = note
                .send_picture
                .then(|| note.before_asset.clone())
                .flatten();
            self.owe_queue_with(ctx, &session, key, text, asset, more)
                .await?;
            ctx.store.lock().await.design_note_change(
                &session.item,
                &note.id,
                NoteChange::Redelivered { delivery },
            )?;
            self.flush_owed(ctx, &session).await?;
            return Ok(Reply::DesignAgent {
                message: format!("{} sent to the agent again", note.id),
            });
        }
        if session.degraded {
            let url = session.preview_url.clone().unwrap_or_default();
            let key = format!(
                "design:{}:preview:again:{}",
                session.id,
                archon_core::now().timestamp_millis()
            );
            self.owe(
                ctx,
                &session,
                key,
                format!("preview unreachable at {url}"),
                None,
            )
            .await?;
            return Ok(Reply::DesignAgent {
                message: "told the agent the preview is unreachable".into(),
            });
        }
        Err(ArchonError::Invalid(
            "there is nothing to tell the agent".into(),
        ))
    }
}
