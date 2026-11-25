//! Operator requests on notes: add, edit, cancel, reply, drop, undo, send
//! again and tell the agent again.

use archon_core::design::{
    DesignAssetId, DesignNote, DesignSession, DesignSessionId, MAX_DETAILS_CHARS, MAX_NOTE_CHARS,
    MAX_REPLY_CHARS, NoteKind, NoteStatus, ReplyAuthor, SessionState, ThreadEntry, Viewport,
    clean_text, is_in_flight, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::id::QueueItemId;
use archon_core::protocol::Reply;
use archon_queue::store::{NewDesignNote, NoteChange, Store};
use sha2::{Digest, Sha256};

use super::delivery::{Owed, agent_key, note_message, pending_note_delivery, queued_payload};
use super::{DesignService, note_view, refuse, status_change};
use crate::context::DaemonContext;

pub(super) fn validate_client_key(key: &str) -> Result<(), ArchonError> {
    if key.trim().is_empty()
        || key.len() > 256
        || key.chars().any(char::is_control)
        || ["continue:", "revise:", "undo:", "resend:"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
    {
        return Err(ArchonError::Invalid("use a unique client key of up to 256 bytes without control characters or a reserved prefix".into()));
    }
    Ok(())
}

pub(super) fn request_fingerprint(input: impl serde::Serialize) -> Result<String, ArchonError> {
    let bytes = serde_json::to_vec(&input)
        .map_err(|error| ArchonError::Corrupt(format!("encode design note request: {error}")))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn require_matching_request(
    store: &Store,
    session: &DesignSession,
    note: &DesignNote,
    request: &str,
    legacy_matches: impl FnOnce() -> bool,
) -> Result<(), ArchonError> {
    assert_eq!(note.item, session.item);
    assert!(!note.client_key.is_empty());
    let matches = note.session == session.id
        && match store.design_note_input(&note.item, &note.id)? {
            Some(saved) => saved == request,
            None => legacy_matches(),
        };
    if !matches {
        return Err(ArchonError::Invalid(
            "this client key already belongs to a different design request".into(),
        ));
    }
    Ok(())
}

struct AddInput {
    client_key: String,
    text: String,
    asset: Option<DesignAssetId>,
    send_picture: bool,
    details: Option<String>,
    follows: Option<String>,
    refs: Vec<String>,
}

impl AddInput {
    fn store(
        mut self,
        store: &mut Store,
        session: &DesignSession,
    ) -> Result<(DesignNote, bool), ArchonError> {
        let existing = store.design_notes(&session.item)?;
        self.refs = super::iterations::check_refs(std::mem::take(&mut self.refs), &existing)?;
        let request = request_fingerprint((
            &self.text,
            &self.asset,
            self.send_picture,
            &self.details,
            &self.follows,
            &self.refs,
        ))?;
        if let Some(found) = existing.iter().find(|n| n.client_key == self.client_key) {
            require_matching_request(store, session, found, &request, || {
                found.text == self.text
                    && found.details_text == self.details
                    && found.follows == self.follows
                    && found.refs == self.refs
                    && (self.follows.is_some() && self.asset.is_none()
                        || found.before_asset == self.asset
                            && found.send_picture == (self.send_picture && self.asset.is_some()))
            })?;
            return Ok((found.clone(), false));
        }
        let parent = self
            .follows
            .as_deref()
            .map(|id| super::iterations::check_follows(id, &existing, &[]))
            .transpose()?;
        let new = self.into_note(store, session, parent)?;
        store.design_note_add_requested(new, &request)
    }

    fn into_note(
        self,
        store: &Store,
        session: &DesignSession,
        parent: Option<&DesignNote>,
    ) -> Result<NewDesignNote, ArchonError> {
        assert_eq!(parent.map(|n| n.id.as_str()), self.follows.as_deref());
        assert!(parent.is_none_or(|n| n.item == session.item));
        let device = super::current_device().map(|d| d.id);
        match (parent, self.asset) {
            (Some(parent), None) => {
                let mut new = super::iterations::follow_up(
                    &session.item,
                    &session.id,
                    parent,
                    self.text,
                    self.client_key,
                    self.refs,
                    device,
                );
                new.details_text = self.details;
                Ok(new)
            }
            (parent, asset) => new_note(
                store,
                session,
                NewDesignNote {
                    item: session.item.clone(),
                    session: session.id.clone(),
                    kind: NoteKind::Page,
                    text: self.text,
                    details_text: self.details,
                    send_picture: self.send_picture && asset.is_some(),
                    target: None,
                    viewport: session_viewport(session),
                    client_key: self.client_key,
                    reverts: None,
                    resent_from: None,
                    follows: parent.map(|p| p.id.clone()),
                    refs: self.refs,
                    before_asset: asset,
                    created_by_device: device,
                },
            ),
        }
    }
}

fn session_viewport(session: &DesignSession) -> Viewport {
    let (w, h) = session.viewport_preset.size();
    Viewport {
        w,
        h,
        preset: session.viewport_preset,
    }
}

fn new_note(
    store: &Store,
    session: &DesignSession,
    mut new: NewDesignNote,
) -> Result<NewDesignNote, ArchonError> {
    assert_eq!(new.session, session.id);
    assert_eq!(new.item, session.item);
    if let Some(id) = &new.before_asset {
        let asset = store.design_asset(&session.item, id)?.ok_or_else(|| {
            ArchonError::Invalid("that picture was not stored by Archon for this item".into())
        })?;
        if let Some(target) = asset.target {
            new.kind = NoteKind::Change;
            new.viewport = target.viewport;
            new.target = Some(target);
        }
    }
    Ok(new)
}

fn queue_undo(
    store: &mut Store,
    session: &DesignSession,
    id: &str,
) -> Result<DesignNote, ArchonError> {
    let device = super::current_device().map(|d| d.id);
    let item = &session.item;
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
    let (revert, created) = store.design_note_add(NewDesignNote {
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
    assert!(created, "a new undo has a fresh generated key");
    assert_eq!(revert.reverts.as_deref(), Some(id));
    Ok(revert)
}

impl DesignService {
    pub(super) async fn exact_notes_session(
        &self,
        ctx: &DaemonContext,
        expected: &DesignSession,
    ) -> Result<DesignSession, ArchonError> {
        let current = self.current(ctx, &expected.id).await?;
        assert_eq!(current.item, expected.item);
        if matches!(
            current.state,
            SessionState::Finishing | SessionState::Confirmed
        ) {
            return Err(super::finishing());
        }
        Ok(current)
    }

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
        let expected = self.current(ctx, session).await?;
        let text = clean_text(&text, MAX_NOTE_CHARS);
        if text.is_empty() {
            return Err(ArchonError::Invalid("a note needs words".into()));
        }
        validate_client_key(&client_key)?;
        let input = AddInput {
            client_key,
            text,
            asset,
            send_picture,
            follows,
            refs,
            details: details_text
                .map(|d| clean_text(&d, MAX_DETAILS_CHARS))
                .filter(|d| !d.is_empty()),
        };
        let lock = self.item_lock(&expected.item);
        let held = lock.lock().await;
        let session = self.exact_notes_session(ctx, &expected).await?;
        let (note, created) = input.store(&mut *ctx.store.lock().await, &session)?;
        if created {
            tracing::info!(note = %note.id, item = %note.item, device = ?super::current_device_label(), "design: note added");
            self.emit_note(ctx, &note.item, &note.id).await;
        }
        drop(held);
        self.pump(ctx, &note.item).await?;
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
        let lock = self.item_lock(item);
        let _held = lock.lock().await;
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
        {
            let mut store = ctx.store.lock().await;
            let raw = queued_payload(
                &store,
                &session,
                Owed {
                    key,
                    text: format!("Operator's answer on {id}: \"{text}\""),
                    asset: None,
                    item: item.clone(),
                    more: Vec::new(),
                },
            )?;
            store.design_note_transition(
                item,
                id,
                status_change(NoteStatus::Working, None),
                Some(ThreadEntry {
                    at: archon_core::now(),
                    from: ReplyAuthor::Operator,
                    text,
                }),
                Some(&raw),
            )?;
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
        let lock = self.item_lock(item);
        let _held = lock.lock().await;
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
        let key = session
            .agent
            .as_ref()
            .map_or(key.clone(), |agent| agent_key(&key, agent));
        self.owe_locked(
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
        let expected = self.current_for_notes(ctx, item).await?;
        let lock = self.item_lock(item);
        let held = lock.lock().await;
        let session = self.exact_notes_session(ctx, &expected).await?;
        let revert = queue_undo(&mut *ctx.store.lock().await, &session, id)?;
        tracing::info!(note = id, undo = %revert.id, %item, device = ?super::current_device_label(), "design: undo queued");
        self.emit_note(ctx, item, &revert.id).await;
        drop(held);
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
        let expected = self.current_for_notes(ctx, item).await?;
        let lock = self.item_lock(item);
        let held = lock.lock().await;
        let session = self.exact_notes_session(ctx, &expected).await?;
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
        drop(held);
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
        let expected = self.current(ctx, session_id).await?;
        let lock = self.item_lock(&expected.item);
        let _held = lock.lock().await;
        let session = self.exact_notes_session(ctx, &expected).await?;
        let notes = ctx.store.lock().await.design_notes(&session.item)?;
        let sent = notes.iter().find(|n| {
            n.session == session.id && n.status == NoteStatus::Sent && n.kind != NoteKind::Revert
        });
        if let Some(note) = sent {
            if !pending_note_delivery(&self.owed_list(ctx, &session.id).await?, &session, note) {
                self.redeliver(ctx, &session, note, &notes).await?;
            }
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
            self.owe_locked(
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
    async fn redeliver(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        notes: &[DesignNote],
    ) -> Result<(), ArchonError> {
        assert_eq!(note.session, session.id);
        assert_eq!(note.status, NoteStatus::Sent);
        let delivery = note.delivery.checked_add(1).ok_or_else(|| {
            ArchonError::Corrupt(format!("{} exhausted its delivery sequence", note.id))
        })?;
        let base = note.base_commit.as_deref().ok_or_else(|| {
            ArchonError::Corrupt(format!("{} was sent without a base commit", note.id))
        })?;
        let (text, more) = note_message(session, note, base, notes);
        let mut store = ctx.store.lock().await;
        let raw = queued_payload(
            &store,
            session,
            Owed {
                key: format!("design:{}:{}:{delivery}", session.id, note.id),
                text,
                asset: note
                    .send_picture
                    .then(|| note.before_asset.clone())
                    .flatten(),
                item: note.item.clone(),
                more,
            },
        )?;
        store.design_note_transition(
            &note.item,
            &note.id,
            NoteChange::Redelivered { delivery },
            None,
            Some(&raw),
        )?;
        drop(store);
        self.emit_note(ctx, &note.item, &note.id).await;
        Ok(())
    }
}
