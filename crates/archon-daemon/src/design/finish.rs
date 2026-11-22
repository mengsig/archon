//! Finish (design.md §8.6): freeze the session, take final pictures at HEAD,
//! confirm them, and tell the agent to wrap up. The proposal gate's ledger is
//! built later from archond's records.

use std::collections::BTreeMap;

use archon_core::design::{
    AssetSource, DesignNote, DesignSession, DesignSessionId, FinishChoices, NoteKind, NoteStatus,
    SessionState, Target, ViewportPreset, is_in_flight, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{DesignFinishView, DesignStateView, FinishCard, Reply};
use archon_design_browser as adb;
use archon_queue::store::{FinishRecord, NoteChange};

use super::assets::{NewAsset, caption_of};
use super::browser::to_adb_viewport;
use super::{DesignService, WorktreeRef, note_view, refuse};
use crate::context::DaemonContext;

/// Margin around an element's final picture, in CSS pixels.
const MARGIN: f64 = 24.0;
/// The finish message to the agent.
const FINISH_TEXT: &str = "Finish: the operator confirmed the design. Stop the dev server without touching the worktree, then complete with a one-line summary.";

fn conflict() -> ArchonError {
    refuse(
        DesignRefusal::Conflict,
        "another device already confirmed or cancelled; reload",
    )
}

fn stale() -> ArchonError {
    refuse(
        DesignRefusal::Stale,
        "the code or the notes changed; capture again",
    )
}

/// A capture id carries the note version it was taken at.
fn capture_id(note_version: u64) -> String {
    format!("{}.{note_version}", ulid::Ulid::generate())
}

fn capture_version(capture: &str) -> Option<u64> {
    capture.rsplit_once('.').and_then(|(_, v)| v.parse().ok())
}

impl DesignService {
    /// Why Finish is not possible now, in plain words.
    pub(crate) fn blocker(
        session: &DesignSession,
        notes: &[DesignNote],
        owed: bool,
    ) -> Option<String> {
        let mine = notes.iter().filter(|n| n.session == session.id);
        for note in mine.clone() {
            if is_in_flight(note.status)
                || (note.kind == NoteKind::Revert && note.status == NoteStatus::Working)
            {
                return Some(match note.status {
                    NoteStatus::NeedsReply => format!("{} needs your answer", note.id),
                    _ => format!("Finish after {} is done", note.id),
                });
            }
        }
        if owed {
            return Some("Finish after the agent has the latest message".into());
        }
        None
    }

    pub(crate) async fn state_view(
        &self,
        ctx: &DaemonContext,
        item: &archon_core::id::QueueItemId,
    ) -> Result<Reply, ArchonError> {
        // With no session open, the item's last one is shown as it ended
        // (approved, revised, rejected...), so a screen opened after the
        // gate's answer says what happened instead of "no session yet".
        // Its notes are all read-only: only an open session's rows change.
        let (current, session, notes, approved) = {
            let store = ctx.store.lock().await;
            let current = store.design_current_session(item)?;
            let session = match &current {
                Some(open) => Some(open.clone()),
                None => store.design_sessions(item)?.pop(),
            };
            (
                current,
                session,
                store.design_notes(item)?,
                super::approved_ids(&store, item)?,
            )
        };
        let owed = match &session {
            Some(s) => self.has_owed(ctx, &s.id).await?,
            None => false,
        };
        let blocker = session
            .as_ref()
            .filter(|s| s.state == SessionState::Live)
            .and_then(|s| Self::blocker(s, &notes, owed));
        let browser = session
            .as_ref()
            .and_then(|s| self.existing_runtime(&s.id))
            .map(|rt| rt.st().status.clone())
            .unwrap_or(archon_core::protocol::BrowserStatus::Idle);
        let browser = match (&browser, session.as_ref()) {
            (archon_core::protocol::BrowserStatus::Idle, Some(s)) if s.sandbox_off => {
                archon_core::protocol::BrowserStatus::Unsandboxed
            }
            _ => browser,
        };
        let views = notes
            .iter()
            .map(|n| note_view(n, &notes, current.as_ref(), &approved))
            .collect::<Vec<_>>();
        let used = views.iter().filter(|v| v.active).count() as u32;
        let stage_deadline = session
            .as_ref()
            .filter(|s| s.state != SessionState::Ended)
            .map(|s| {
                s.created_at
                    + chrono::Duration::minutes(i64::from(
                        archon_core::design::STAGE_TIMEOUT_MINUTES,
                    ))
            });
        Ok(Reply::DesignState(DesignStateView {
            item: item.clone(),
            session,
            notes: views,
            browser,
            finish_blocker: blocker,
            images_used: used,
            images_max: 128,
            stage_deadline,
        }))
    }

    pub(crate) async fn finish_capture(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
        cancel_queued: bool,
        recapture: bool,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session_id).await?;
        let lock = self.item_lock(&row.item);
        let _held = lock.lock().await;
        let wt = self.worktree_of(ctx, &row).await?;
        let mut row = row;
        match row.state {
            SessionState::Confirmed => return Err(super::finishing()),
            SessionState::Finishing if !recapture => {
                return self
                    .finish_view(ctx, &row, &wt)
                    .await
                    .map(Reply::DesignFinish);
            }
            SessionState::Finishing => {
                row = ctx.store.lock().await.design_session_transition(
                    &row.id,
                    SessionState::Finishing,
                    SessionState::Live,
                    None,
                    false,
                )?;
            }
            _ => {}
        }
        // Preconditions.
        let notes = ctx.store.lock().await.design_notes(&row.item)?;
        let owed = self.has_owed(ctx, &row.id).await?;
        if let Some(why) = Self::blocker(&row, &notes, owed) {
            return Err(ArchonError::Invalid(why));
        }
        let queued = notes
            .iter()
            .filter(|n| n.session == row.id && n.status == NoteStatus::Queued)
            .count();
        if queued > 0 && !cancel_queued {
            return Err(ArchonError::Invalid(format!(
                "{queued} queued note{} would be cancelled; confirm to cancel and finish",
                if queued == 1 { "" } else { "s" }
            )));
        }
        let head = self.head_of(&wt)?;
        if let Err(why) = self.worktree_at(&wt, &head)? {
            return Err(ArchonError::Invalid(why));
        }
        let active = {
            let store = ctx.store.lock().await;
            let carried = Self::review_choices(&store, &row.item, &row.id, None)?;
            self.active_notes(&store, &wt, &row.item, &head, carried.as_ref())?
        };
        let no_change_only = active.is_empty()
            && notes.iter().any(|n| {
                n.session == row.id
                    && n.kind != NoteKind::Revert
                    && n.status == NoteStatus::Done
                    && n.no_change
            });
        if active.is_empty() && !no_change_only {
            return Err(ArchonError::Invalid(
                "nothing to approve; add a note or cancel".into(),
            ));
        }
        // Cancelled under the item's lock, which sending also needs, so the
        // note version the capture records is the one the freeze sees.
        if cancel_queued && queued > 0 {
            let mut store = ctx.store.lock().await;
            for note in notes
                .iter()
                .filter(|n| n.session == row.id && n.status == NoteStatus::Queued)
            {
                store.design_note_change(
                    &note.item,
                    &note.id,
                    super::status_change(NoteStatus::Cancelled, None),
                )?;
            }
            row = store.design_session(&row.id)?.ok_or_else(super::gone)?;
        }
        // Every stored picture counts as a change of the notes, so they are
        // taken first and the capture names the version they leave. An
        // earlier session's note keeps the picture its own review confirmed:
        // its row is never written again (design.md §8.6, Revise).
        for note in active.iter().filter(|n| n.session == row.id) {
            self.capture_final(ctx, &row, note, &head, &wt).await;
        }
        self.restore_viewport(ctx, &row).await;
        row = ctx
            .store
            .lock()
            .await
            .design_session(&row.id)?
            .ok_or_else(super::gone)?;
        let capture = capture_id(row.note_version);
        let moved = ctx.store.lock().await.design_session_transition(
            &row.id,
            SessionState::Live,
            SessionState::Finishing,
            Some(FinishRecord {
                capture: capture.clone(),
                head: head.clone(),
                note_version: row.note_version,
                choices: None,
            }),
            false,
        );
        let row = moved.map_err(|_| conflict())?;
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: finish capture");
        self.emit_session(ctx, &row.id).await;
        self.finish_view(ctx, &row, &wt)
            .await
            .map(Reply::DesignFinish)
    }

    async fn restore_viewport(&self, ctx: &DaemonContext, row: &DesignSession) {
        let _ = ctx;
        if let Some(browser) = self.existing_runtime(&row.id).and_then(|rt| rt.browser()) {
            let (_, v) = to_adb_viewport(row.viewport_preset);
            let _ = browser.set_viewport(v).await;
        }
    }

    /// The final picture of one note on a fresh load at its page and viewport.
    async fn capture_final(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        note: &DesignNote,
        head: &str,
        _wt: &WorktreeRef,
    ) {
        let Ok(Some(browser)) = self.ensure_browser(ctx, &row.id).await else {
            return;
        };
        let preset = note
            .target
            .as_ref()
            .map_or(note.viewport.preset, |t| t.viewport.preset);
        let (_, viewport) = to_adb_viewport(preset);
        let _ = browser.set_viewport(viewport).await;
        if let (Some(url), Some(target)) = (row.preview_url.as_deref(), note.target.as_ref()) {
            let origin = origin_of(url);
            let _ = browser.navigate(&format!("{origin}{}", target.path)).await;
            let _ = browser
                .wait_network_idle(
                    std::time::Duration::from_millis(500),
                    std::time::Duration::from_secs(8),
                )
                .await;
        }
        let clip = match note.target.as_ref() {
            Some(target) if target.region => Some(clip_of(target, 0.0)),
            Some(target) => match target.selector.as_deref() {
                Some(selector) => match browser
                    .resolve_selector(selector, Some(recorded_box(target)))
                    .await
                    .ok()
                    .flatten()
                {
                    Some(rect) => Some(grow(rect, MARGIN)),
                    None => return,
                },
                None => return,
            },
            None => None,
        };
        let Ok(picture) = browser.capture(clip, 2.0).await else {
            return;
        };
        if let Err(error) = self.keep_final(ctx, note, &picture, head).await {
            tracing::info!(%error, "design: a final picture was not kept");
        }
    }

    async fn keep_final(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        picture: &adb::Picture,
        head: &str,
    ) -> Result<(), ArchonError> {
        let asset = self
            .put_asset(
                ctx,
                &note.item,
                NewAsset {
                    source: AssetSource::Final,
                    target: note.target.clone(),
                    note: Some(note.id.clone()),
                    commit: Some(head.to_owned()),
                    mime: picture.mime,
                    bytes: &picture.bytes,
                    width: picture.width,
                    height: picture.height,
                    sketch: None,
                },
            )
            .await?;
        if let Some(asset) = asset {
            ctx.store.lock().await.design_note_change(
                &note.item,
                &note.id,
                NoteChange::Final {
                    asset: Some(asset.id),
                    included: None,
                    captured_commit: Some(head.to_owned()),
                },
            )?;
        }
        Ok(())
    }

    /// The review screen, from what is stored.
    async fn finish_view(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        wt: &WorktreeRef,
    ) -> Result<DesignFinishView, ArchonError> {
        let head = row
            .finish_head
            .clone()
            .ok_or_else(|| ArchonError::Corrupt("a finishing session has a head".into()))?;
        let capture = row.finish_capture.clone().unwrap_or_default();
        let (active, notes, choices) = {
            let store = ctx.store.lock().await;
            let choices =
                Self::review_choices(&store, &row.item, &row.id, row.finish_choices.as_ref())?;
            (
                self.active_notes(&store, wt, &row.item, &head, choices.as_ref())?,
                store.design_notes(&row.item)?,
                choices,
            )
        };
        // Auto supersession is shown on the cards of the notes it covers.
        let all_active = {
            let store = ctx.store.lock().await;
            self.active_notes(
                &store,
                wt,
                &row.item,
                &head,
                Some(&FinishChoices {
                    include: BTreeMap::new(),
                    superseded: notes.iter().map(|n| (n.id.clone(), None)).collect(),
                }),
            )?
        };
        let refs: Vec<&DesignNote> = all_active.iter().collect();
        let auto = archon_core::design::auto_supersession(&refs);
        let mut cards = Vec::new();
        for note in &all_active {
            let superseded_by = choices
                .as_ref()
                .and_then(|c| c.superseded.get(&note.id).cloned())
                .unwrap_or_else(|| auto.get(&note.id).cloned());
            let asset = match &note.final_asset {
                Some(id) => ctx.store.lock().await.design_asset(&row.item, id)?,
                None => None,
            };
            let earlier = note.session != row.id;
            // A note another one supersedes takes no final picture, so its
            // missing picture is no sign its element is gone. An earlier
            // session's note is never captured again, so nothing is missing.
            let not_found = !earlier
                && asset.is_none()
                && note.final_asset.is_none()
                && superseded_by.is_none();
            let caption = match (earlier, asset.as_ref()) {
                (true, Some(_)) => Some("Reviewed in an earlier session; kept as it was".into()),
                (true, None) => Some("From an earlier session, with no picture".into()),
                (false, asset) => asset.and_then(caption_of),
            };
            cards.push(FinishCard {
                note: note.id.clone(),
                text: note.text.clone(),
                asset: asset.as_ref().map(|a| a.id.clone()),
                data_url: asset.as_ref().and_then(|a| self.data_url(a).ok()),
                caption,
                include: note.final_included.unwrap_or(true) && asset.is_some(),
                superseded_by,
                not_found,
                left_history: false,
                earlier,
            });
        }
        // Done notes whose change left the history.
        let undone = undone_by(&notes);
        for note in &notes {
            let listed = all_active.iter().any(|a| a.id == note.id);
            if note.kind != NoteKind::Revert
                && note.status == NoteStatus::Done
                && note.commit.is_some()
                && !undone.contains_key(&note.id)
                && !listed
                && !active.iter().any(|a| a.id == note.id)
                && self.left_history(wt, note, &head)
            {
                cards.push(FinishCard {
                    note: note.id.clone(),
                    text: note.text.clone(),
                    asset: None,
                    data_url: None,
                    caption: Some("Its change is no longer in the code; not included".into()),
                    include: false,
                    superseded_by: None,
                    not_found: false,
                    left_history: true,
                    earlier: note.session != row.id,
                });
            }
        }
        let no_change_only = all_active.is_empty()
            && notes.iter().any(|n| {
                n.session == row.id
                    && n.kind != NoteKind::Revert
                    && n.status == NoteStatus::Done
                    && n.no_change
            });
        Ok(DesignFinishView {
            capture,
            head,
            note_version: capture_version(row.finish_capture.as_deref().unwrap_or_default())
                .unwrap_or(row.note_version),
            cards,
            no_change_only,
        })
    }

    fn left_history(&self, wt: &WorktreeRef, note: &DesignNote, head: &str) -> bool {
        use crate::worktree::design_git::{self, NoteGit};
        let (Ok(shadow), Some(commit)) = (self.shadow(wt), note.commit.as_deref()) else {
            return false;
        };
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        matches!(design_git::contains(&g, commit, head), Ok(false))
    }

    /// `design_final_capture`: the current frame for one note.
    pub(crate) async fn final_capture(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
        capture: &str,
        note_id: &str,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session_id).await?;
        // Under the item lock, as Finish and Confirm: the picture kept here
        // and the capture re-stamped below are one change of the review.
        let lock = self.item_lock(&row.item);
        let _held = lock.lock().await;
        let row = ctx
            .store
            .lock()
            .await
            .design_session(&row.id)?
            .ok_or_else(super::gone)?;
        if row.state != SessionState::Finishing {
            return Err(conflict());
        }
        if row.finish_capture.as_deref() != Some(capture) {
            return Err(stale());
        }
        let wt = self.worktree_of(ctx, &row).await?;
        let note = ctx
            .store
            .lock()
            .await
            .design_note(&row.item, note_id)?
            .ok_or_else(|| ArchonError::NotFound(format!("note {note_id}")))?;
        if note.session != row.id {
            return Err(refuse(
                DesignRefusal::ReadOnly,
                format!("{note_id} is from an earlier session; its picture was reviewed then"),
            ));
        }
        // The approved reference records the note's preset size, so the
        // picture is taken at that preset whatever the preview shows now.
        // A metrics change keeps the page as the operator left it.
        let preset = note
            .target
            .as_ref()
            .map_or(note.viewport.preset, |t| t.viewport.preset);
        let head = row.finish_head.clone().unwrap_or_default();
        if let Some(browser) = self.ensure_browser(ctx, &row.id).await? {
            let switched = row.viewport_preset != preset;
            if switched {
                let (_, viewport) = to_adb_viewport(preset);
                if let Err(e) = browser.set_viewport(viewport).await {
                    self.restore_viewport(ctx, &row).await;
                    return Err(ArchonError::Unavailable(format!("the capture failed: {e}")));
                }
            }
            let clip = match note.target.as_ref().and_then(|t| t.selector.as_deref()) {
                Some(selector) => browser
                    .resolve_selector(selector, note.target.as_ref().map(recorded_box))
                    .await
                    .ok()
                    .flatten()
                    .map(|r| grow(r, MARGIN)),
                None => None,
            };
            let picture = browser.capture(clip, 2.0).await;
            if switched {
                self.restore_viewport(ctx, &row).await;
            }
            let picture = picture
                .map_err(|e| ArchonError::Unavailable(format!("the capture failed: {e}")))?;
            self.keep_final(ctx, &note, &picture, &head).await?;
        }
        let row = self.restamp_capture(ctx, &row.id, &head).await?;
        self.finish_view(ctx, &row, &wt)
            .await
            .map(Reply::DesignFinish)
    }

    /// The review with a capture id at the notes' version now. A picture the
    /// operator took during the review changes the notes' version; without a
    /// new id Confirm would read that as the code changing under the review.
    /// The head stays: only the operator's own picture changed.
    async fn restamp_capture(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        head: &str,
    ) -> Result<DesignSession, ArchonError> {
        let restamped = {
            let mut store = ctx.store.lock().await;
            let current = store.design_session(session)?.ok_or_else(super::gone)?;
            if current.finish_capture.as_deref().and_then(capture_version)
                == Some(current.note_version)
            {
                return Ok(current);
            }
            store
                .design_finish_restamp(session, &capture_id(current.note_version), head)
                .map_err(|_| conflict())?
        };
        // Other devices reviewing the same session take the new id too.
        self.emit_session(ctx, session).await;
        Ok(restamped)
    }

    pub(crate) async fn finish_confirm(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
        capture: &str,
        include: BTreeMap<String, bool>,
        superseded: BTreeMap<String, Option<String>>,
        confirm_no_change: bool,
    ) -> Result<Reply, ArchonError> {
        let item = {
            let store = ctx.store.lock().await;
            store
                .design_session(session_id)?
                .ok_or_else(super::gone)?
                .item
        };
        let lock = self.item_lock(&item);
        let _held = lock.lock().await;
        // Read the session under the item lock: a Back to editing, a new
        // capture or the session's end may have landed while this waited.
        let row = {
            let store = ctx.store.lock().await;
            let row = store.design_session(session_id)?.ok_or_else(super::gone)?;
            if row.state == SessionState::Ended {
                return Err(super::gone());
            }
            row
        };
        if row.state != SessionState::Finishing {
            return Err(conflict());
        }
        if row.finish_capture.as_deref() != Some(capture) {
            return Err(stale());
        }
        let head = row.finish_head.clone().unwrap_or_default();
        let wt = self.worktree_of(ctx, &row).await?;
        if self.head_of(&wt)? != head || self.worktree_at(&wt, &head)?.is_err() {
            return Err(stale());
        }
        if capture_version(capture) != Some(row.note_version) {
            return Err(stale());
        }
        // Q3: an approved requirement is never superseded.
        let approved = ctx
            .store
            .lock()
            .await
            .item(&row.item)?
            .intent
            .map(|b| b.requirements)
            .unwrap_or_default();
        for (id, by) in &superseded {
            if by.is_some() && approved.contains(id) {
                return Err(ArchonError::Invalid(format!(
                    "{id} was approved earlier; add a new note instead"
                )));
            }
        }
        // Only this session's pictures are chosen here: an earlier session's
        // note keeps the inclusion its own review recorded.
        let notes_now = ctx.store.lock().await.design_notes(&row.item)?;
        let include: BTreeMap<String, bool> = include
            .into_iter()
            .filter(|(id, _)| notes_now.iter().any(|n| &n.id == id && n.session == row.id))
            .collect();
        let choices = FinishChoices {
            include: include.clone(),
            superseded,
        };
        let (active, notes) = {
            let store = ctx.store.lock().await;
            let in_force = Self::review_choices(&store, &row.item, &row.id, Some(&choices))?;
            (
                self.active_notes(&store, &wt, &row.item, &head, in_force.as_ref())?,
                store.design_notes(&row.item)?,
            )
        };
        let no_change_only = active.is_empty()
            && notes.iter().any(|n| {
                n.session == row.id
                    && n.kind != NoteKind::Revert
                    && n.status == NoteStatus::Done
                    && n.no_change
            });
        if active.is_empty() && !no_change_only {
            return Err(ArchonError::Invalid(
                "nothing to approve; add a note or cancel".into(),
            ));
        }
        if no_change_only && !confirm_no_change {
            return Err(ArchonError::Invalid(
                "no note changed anything; confirm that you want to approve with no change".into(),
            ));
        }
        let moved = ctx.store.lock().await.design_session_transition(
            &row.id,
            SessionState::Finishing,
            SessionState::Confirmed,
            Some(FinishRecord {
                capture: capture.to_owned(),
                head: head.clone(),
                note_version: row.note_version,
                choices: Some(choices),
            }),
            false,
        );
        let row = moved.map_err(|_| conflict())?;
        {
            let mut store = ctx.store.lock().await;
            for note in active.iter().filter(|n| n.session == row.id) {
                let included =
                    include.get(&note.id).copied().unwrap_or(true) && note.final_asset.is_some();
                store.design_note_change(
                    &note.item,
                    &note.id,
                    NoteChange::Final {
                        asset: note.final_asset.clone(),
                        included: Some(included),
                        captured_commit: note.captured_commit.clone(),
                    },
                )?;
            }
        }
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: finish confirmed");
        // The preview is done: the gate reads the recorded pictures, so the
        // browser and its profile go now, not when the gate is answered.
        self.park_browser(&row.id, "confirmed");
        self.emit_session(ctx, &row.id).await;
        self.owe(
            ctx,
            &row,
            format!("design:{}:finish", row.id),
            FINISH_TEXT.to_owned(),
            None,
        )
        .await?;
        Ok(Reply::DesignDone)
    }

    pub(crate) async fn finish_cancel(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session_id).await?;
        if row.state != SessionState::Finishing {
            return Err(conflict());
        }
        let moved = ctx.store.lock().await.design_session_transition(
            &row.id,
            SessionState::Finishing,
            SessionState::Live,
            None,
            false,
        );
        let row = moved.map_err(|_| conflict())?;
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: back to editing");
        self.emit_session(ctx, &row.id).await;
        self.pump(ctx, &row.item).await?;
        Ok(Reply::DesignDone)
    }

    pub(crate) async fn set_viewport(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
        preset: ViewportPreset,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session_id).await?;
        ctx.store.lock().await.design_session_patch(
            &row.id,
            archon_queue::store::SessionPatch {
                viewport_preset: Some(preset),
                ..Default::default()
            },
        )?;
        if let Some(browser) = self.existing_runtime(&row.id).and_then(|rt| rt.browser()) {
            let (_, v) = to_adb_viewport(preset);
            browser
                .set_viewport(v)
                .await
                .map_err(|e| ArchonError::Unavailable(e.to_string()))?;
        }
        tracing::info!(session = %row.id, ?preset, device = ?super::current_device_label(), "design: viewport changed");
        self.emit_session(ctx, &row.id).await;
        Ok(Reply::DesignDone)
    }
}

fn origin_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or_default();
    format!("http://{authority}")
}

/// The element's box as the note recorded it, in page CSS pixels at the
/// note's preset: how a selector matching several elements finds the one meant.
pub(crate) fn recorded_box(target: &Target) -> adb::ClipRect {
    clip_of(target, 0.0)
}

fn clip_of(target: &Target, margin: f64) -> adb::ClipRect {
    grow(
        adb::ClipRect {
            x: target.rect.x,
            y: target.rect.y,
            w: target.rect.w,
            h: target.rect.h,
        },
        margin,
    )
}

fn grow(rect: adb::ClipRect, margin: f64) -> adb::ClipRect {
    let x = (rect.x - margin).max(0.0);
    let y = (rect.y - margin).max(0.0);
    adb::ClipRect {
        x,
        y,
        w: rect.w + (rect.x - x) + margin,
        h: rect.h + (rect.y - y) + margin,
    }
}
