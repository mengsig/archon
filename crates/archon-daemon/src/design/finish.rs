//! Finish (design.md §8.6): freeze the session, take final pictures at HEAD,
//! confirm them, and tell the agent to wrap up. The proposal gate's ledger is
//! built later from archond's records.

use std::collections::BTreeMap;

use archon_core::design::{
    AssetSource, DesignNote, DesignSession, DesignSessionId, FinishChoices, NoteKind, NoteStatus,
    SessionState, Target, ViewportPreset, is_in_flight, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{
    DesignFinishView, DesignStateView, FinishCard, FinishHistoryEntry, Reply,
};
use archon_design_browser as adb;
use archon_queue::store::{FinishRecord, NoteChange};

use super::assets::{NewAsset, caption_of};
use super::browser::to_adb_viewport;
use super::{Approval, DesignService, WorktreeRef, note_view, refuse};
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
        let (active, approved) = {
            let store = ctx.store.lock().await;
            let carried = Self::review_choices(&store, &row.item, &row.id, None)?;
            (
                self.active_notes(&store, &wt, &row.item, &head, carried.as_ref())?,
                super::approved_design(&store, &row.item)?,
            )
        };
        if super::approval(&active, &notes, &row.id, &approved) == Approval::Nothing {
            return Err(ArchonError::Invalid(super::NOTHING_TO_APPROVE.into()));
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
        // The page the preview shows at Finish. A page note with no page of
        // its own is pictured as shown; once a note before it took the
        // preview elsewhere, it is pictured back at this page, not that
        // one, and the preview is taken back here afterwards.
        let start = match self.existing_runtime(&row.id).and_then(|rt| rt.browser()) {
            Some(browser) => browser.current_path().await.ok(),
            None => None,
        };
        let mut moved = false;
        for note in active.iter().filter(|n| n.session == row.id) {
            self.capture_final(ctx, &row, note, &head, start.as_deref(), &mut moved)
                .await;
        }
        self.restore_viewport(ctx, &row).await;
        if moved
            && let (Some(start), Some(url)) = (start.as_deref(), row.preview_url.as_deref())
            && let Some(browser) = self.existing_runtime(&row.id).and_then(|rt| rt.browser())
        {
            let _ = browser
                .navigate(&format!("{}{start}", origin_of(url)))
                .await;
        }
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

    /// The final picture of one note on a fresh load at its page and viewport;
    /// `start` is the page the preview showed at Finish, and `moved` says an
    /// earlier picture took the preview away from it.
    async fn capture_final(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        note: &DesignNote,
        head: &str,
        start: Option<&str>,
        moved: &mut bool,
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
        // What the after picture recorded: a page note done with its own page
        // (a new screen) is pictured there, not on whatever page the preview
        // shows at Finish, and a state the agent reached with steps is
        // reached again.
        let shown = self.after_target(ctx, note).await;
        let steps = shown.as_ref().map(|t| t.steps.clone()).unwrap_or_default();
        // A page note with no page of its own stays on the page as shown
        // (its query and fragment too) unless an earlier picture moved it.
        let page = note
            .target
            .as_ref()
            .or(shown.as_ref())
            .map(|t| t.path.as_str());
        let path = match page {
            Some(page) => {
                *moved = true;
                Some(page)
            }
            None => start.filter(|_| *moved),
        };
        if let (Some(url), Some(path)) = (row.preview_url.as_deref(), path) {
            let origin = origin_of(url);
            let _ = browser
                .navigate_settled(
                    &format!("{origin}{path}"),
                    std::time::Duration::from_secs(10),
                )
                .await;
        }
        if let Some(url) = row.preview_url.as_deref()
            && let Err(why) = super::state::replay_steps(&*browser, &origin_of(url), &steps).await
        {
            tracing::info!(note = %note.id, why, "design: the final picture's state was not reached");
        }
        let Some(clip) = final_clip(&*browser, note, !steps.is_empty()).await else {
            return;
        };
        let Ok(picture) = browser.capture(clip, 2.0).await else {
            return;
        };
        self.note_unchanged_final(ctx, &*browser, note, &picture)
            .await;
        if let Err(error) = self.keep_final(ctx, note, &picture, head).await {
            tracing::info!(%error, "design: a final picture was not kept");
        }
    }

    /// The target the note's after picture recorded: its page and steps.
    async fn after_target(&self, ctx: &DaemonContext, note: &DesignNote) -> Option<Target> {
        let after = note.after_asset.as_ref()?;
        ctx.store
            .lock()
            .await
            .design_asset(&note.item, after)
            .ok()
            .flatten()
            .and_then(|a| a.target)
    }

    /// A done note with no after picture whose final picture matches its
    /// before picture shows no change: say so on the note, once, so no screen
    /// offers the two as a comparison.
    async fn note_unchanged_final(
        &self,
        ctx: &DaemonContext,
        browser: &dyn super::browser::PreviewBrowser,
        note: &DesignNote,
        picture: &adb::Picture,
    ) {
        let changed_code =
            note.kind != NoteKind::Revert && note.commit.is_some() && !note.no_change;
        if !changed_code
            || note.after_asset.is_some()
            || archon_core::design::no_visible_change(note).is_some()
            || !self.matches_before(ctx, browser, note, picture).await
        {
            return;
        }
        let said = note
            .thread
            .iter()
            .rev()
            .find(|e| e.from == archon_core::design::ReplyAuthor::Agent)
            .map_or("the agent gave no explanation", |e| e.text.as_str());
        let entry = archon_core::design::ThreadEntry {
            at: archon_core::now(),
            from: archon_core::design::ReplyAuthor::Archon,
            text: format!("{}: {said}", archon_core::design::NO_VISIBLE_CHANGE),
        };
        let noted = ctx.store.lock().await.design_note_change(
            &note.item,
            &note.id,
            NoteChange::Thread(entry),
        );
        if let Err(error) = noted {
            tracing::info!(%error, "design: the no-visible-change notice was not kept");
        }
        self.emit_note(ctx, &note.item, &note.id).await;
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
        let undone = undone_by(&notes);
        let chains = archon_core::design::chains(&notes);
        // A card stands for its chain: the chain's earlier done versions,
        // oldest first, are its history.
        let history = |note: &DesignNote| -> Vec<FinishHistoryEntry> {
            let Some(root) = chains.get(&note.id).map(|p| &p.root) else {
                return Vec::new();
            };
            notes
                .iter()
                .filter(|n| {
                    n.number < note.number
                        && n.status == NoteStatus::Done
                        && chains.get(&n.id).is_some_and(|p| &p.root == root)
                })
                .map(|n| FinishHistoryEntry {
                    note: n.id.clone(),
                    text: n.text.clone(),
                    set_aside: undone.contains_key(&n.id),
                })
                .collect()
        };
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
                history: history(note),
            });
        }
        // Done notes whose change left the history.
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
                    history: Vec::new(),
                });
            }
        }
        // The rule Confirm applies, on the same notes: the review asks
        // "approve as it is?" exactly when Confirm needs that answer.
        let approved = super::approved_design(&*ctx.store.lock().await, &row.item)?;
        let no_change_only =
            super::approval(&active, &notes, &row.id, &approved) == Approval::NoChange;
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
        let approved = super::approved_design(&*ctx.store.lock().await, &row.item)?;
        for (id, by) in &superseded {
            if by.is_some() && approved.ids.contains(id) {
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
        match super::approval(&active, &notes, &row.id, &approved) {
            Approval::Notes => {}
            Approval::Nothing => {
                return Err(ArchonError::Invalid(super::NOTHING_TO_APPROVE.into()));
            }
            Approval::NoChange if !confirm_no_change => {
                return Err(ArchonError::Invalid(NO_CHANGE_UNCONFIRMED.into()));
            }
            Approval::NoChange => {}
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

pub(crate) fn origin_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or_default();
    format!("http://{authority}")
}

/// What the final picture covers: nothing is `None` (the note's element is
/// gone), `Some(None)` the whole view (a state reached by steps, or no target).
async fn final_clip(
    browser: &dyn super::browser::PreviewBrowser,
    note: &DesignNote,
    stepped: bool,
) -> Option<Option<adb::ClipRect>> {
    let target = match note.target.as_ref() {
        _ if stepped => return Some(None),
        None => return Some(None),
        Some(target) => target,
    };
    if target.region {
        return Some(Some(clip_of(target, 0.0)));
    }
    let selector = target.selector.as_deref()?;
    let rect = browser
        .resolve_selector(selector, Some(recorded_box(target)))
        .await
        .ok()
        .flatten()?;
    Some(Some(grow(rect, MARGIN)))
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

/// Confirm's refusal when nothing changed and the operator did not say to
/// approve as it is. The review screen recognises it and asks.
pub(crate) const NO_CHANGE_UNCONFIRMED: &str =
    "nothing changed in this session; confirm that you want to approve the design as it is";
