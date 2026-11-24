//! Finish (design.md §8.6): freeze the session, take final pictures at HEAD,
//! confirm them, and tell the agent to wrap up. The proposal gate's ledger is
//! built later from archond's records.

use std::collections::BTreeMap;
use std::time::Duration;

use archon_core::design::{
    AssetSource, DesignNote, DesignSession, DesignSessionId, FinishChoices,
    MAX_SERIALIZED_FRAME_BYTES, NoteKind, NoteStatus, SessionState, Target, ViewportPreset,
    is_in_flight, undone_by,
};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{
    DesignFinishView, DesignStateView, FinishCard, FinishHistoryEntry, Reply,
};
use archon_design_browser as adb;
use archon_queue::store::{FinishRecord, NoteChange};

use super::assets::{NewAsset, caption_of};
use super::browser::{PreviewBrowser, to_adb_viewport};
use super::delivery::{Owed, queued_payload};
use super::state::{navigate_fresh, reload_fresh, replay_steps};
use super::{Approval, DesignService, WorktreeRef, note_view, refuse};
use crate::context::DaemonContext;

/// Margin around an element's final picture, in CSS pixels.
const MARGIN: f64 = 24.0;
const FINISH_BUDGET: Duration = Duration::from_secs(45);
const MANUAL_BUDGET: Duration = Duration::from_secs(30);
const RESTORE_BUDGET: Duration = Duration::from_secs(10);
/// The finish message to the agent.
const FINISH_TEXT: &str = "Finish: the operator confirmed the design. Stop the dev server without touching the worktree, then complete with a one-line summary.";

fn conflict() -> ArchonError {
    refuse(
        DesignRefusal::Conflict,
        "another device already confirmed or cancelled; reload",
    )
}

fn capture_error(why: impl Into<String>) -> ArchonError {
    ArchonError::Unavailable(format!(
        "the final picture could not be taken: {}",
        why.into()
    ))
}

fn capture_timeout(budget: Duration) -> ArchonError {
    capture_error(format!("the capture exceeded {} seconds", budget.as_secs()))
}

fn restored<T>(
    capture: Result<T, ArchonError>,
    restore: Result<(), ArchonError>,
) -> Result<T, ArchonError> {
    match (capture, restore) {
        (Ok(picture), Ok(())) => Ok(picture),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => {
            tracing::warn!(%restore, "design: restoring the preview after capture failed");
            Err(error)
        }
    }
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
        let mut row = self.current(ctx, session_id).await?;
        let wt = self.worktree_of(ctx, &row).await?;
        let recapturing = row.state == SessionState::Finishing;
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
            SessionState::Live => {}
            _ => {
                return Err(ArchonError::Invalid(
                    "the design session is not live".into(),
                ));
            }
        }
        let captured = self.capture_review(ctx, &row, &wt, cancel_queued).await;
        if recapturing || captured.is_ok() {
            self.emit_session(ctx, &row.id).await;
        }
        self.finish_view(ctx, &captured?, &wt)
            .await
            .map(Reply::DesignFinish)
    }

    async fn capture_review(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        wt: &WorktreeRef,
        cancel_queued: bool,
    ) -> Result<DesignSession, ArchonError> {
        assert_eq!(row.state, SessionState::Live);
        let head = self.head_of(wt)?;
        if let Err(why) = self.worktree_at(wt, &head)? {
            return Err(ArchonError::Invalid(why));
        }
        let (active, notes, queued) = self
            .finish_capture_notes(ctx, row, wt, &head, cancel_queued)
            .await?;
        self.clear_old_finals(ctx, row, &notes).await?;
        self.capture_finals(ctx, row, &active, &head).await?;
        if self.worktree_at(wt, &head)?.is_err() {
            return Err(stale());
        }
        let row = {
            let mut store = ctx.store.lock().await;
            let current = store.design_session(&row.id)?.ok_or_else(super::gone)?;
            let note_version = current.note_version + queued;
            store.design_session_transition(
                &row.id,
                SessionState::Live,
                SessionState::Finishing,
                Some(FinishRecord {
                    capture: capture_id(note_version),
                    head,
                    note_version,
                    choices: None,
                }),
                cancel_queued,
            )?
        };
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: finish capture");
        assert_eq!(row.state, SessionState::Finishing);
        Ok(row)
    }

    async fn finish_capture_notes(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        wt: &WorktreeRef,
        head: &str,
        cancel_queued: bool,
    ) -> Result<(Vec<DesignNote>, Vec<DesignNote>, u64), ArchonError> {
        let owed = self.has_owed(ctx, &row.id).await?;
        let store = ctx.store.lock().await;
        let notes = store.design_notes(&row.item)?;
        if let Some(why) = Self::blocker(row, &notes, owed) {
            return Err(ArchonError::Invalid(why));
        }
        let queued = notes
            .iter()
            .filter(|n| n.session == row.id && n.status == NoteStatus::Queued)
            .count() as u64;
        if queued > 0 && !cancel_queued {
            return Err(ArchonError::Invalid(format!(
                "{queued} queued note{} would be cancelled; confirm to cancel and finish",
                if queued == 1 { "" } else { "s" }
            )));
        }
        let carried = Self::review_choices(&store, &row.item, &row.id, None)?;
        let active = self.active_notes(&store, wt, &row.item, head, carried.as_ref())?;
        let approved = super::approved_design(&store, &row.item)?;
        if super::approval(&active, &notes, &row.id, &approved) == Approval::Nothing {
            return Err(ArchonError::Invalid(super::NOTHING_TO_APPROVE.into()));
        }
        Ok((active, notes, queued))
    }

    async fn clear_old_finals(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        notes: &[DesignNote],
    ) -> Result<(), ArchonError> {
        let mut store = ctx.store.lock().await;
        for note in notes
            .iter()
            .filter(|n| n.session == row.id && n.final_asset.is_some())
        {
            let captured_commit = match &note.after_asset {
                Some(id) => {
                    store
                        .design_asset(&row.item, id)?
                        .ok_or_else(|| ArchonError::Corrupt(format!("missing after picture {id}")))?
                        .commit
                }
                None => None,
            };
            store.design_note_change(
                &row.item,
                &note.id,
                NoteChange::Final {
                    asset: None,
                    included: None,
                    captured_commit,
                },
            )?;
        }
        Ok(())
    }

    async fn capture_finals(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        active: &[DesignNote],
        head: &str,
    ) -> Result<(), ArchonError> {
        let Some(url) = row.preview_url.as_deref() else {
            return Ok(());
        };
        let notes: Vec<_> = active
            .iter()
            .filter(|note| note.session == row.id)
            .collect();
        if notes.is_empty() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + FINISH_BUDGET;
        let Some(browser) = tokio::time::timeout_at(deadline, self.ensure_browser(ctx, &row.id))
            .await
            .map_err(|_| capture_timeout(FINISH_BUDGET))??
        else {
            return Ok(());
        };
        let origin = origin_of(url);
        let start = tokio::time::timeout_at(deadline, browser.current_location())
            .await
            .map_err(|_| capture_timeout(FINISH_BUDGET))?
            .map_err(|error| {
                capture_error(format!("could not read the current preview page: {error}"))
            })?;
        let mut moved = false;
        let result = tokio::time::timeout_at(deadline, async {
            for note in notes {
                self.capture_final(ctx, &*browser, (&origin, &start), note, head, &mut moved)
                    .await?;
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(capture_timeout(FINISH_BUDGET)));
        let restore = Self::restore_preview(
            &*browser,
            row.viewport_preset,
            moved.then_some((&origin, &start)),
        )
        .await;
        restored(result, restore)
    }

    async fn restore_preview(
        browser: &(dyn PreviewBrowser + 'static),
        preset: ViewportPreset,
        page: Option<(&str, &str)>,
    ) -> Result<(), ArchonError> {
        tokio::time::timeout(RESTORE_BUDGET, async {
            let (_, viewport) = to_adb_viewport(preset);
            let viewport = browser.set_viewport(viewport).await;
            let page = match page {
                Some((origin, path)) => browser.navigate(&format!("{origin}{path}")).await,
                None => Ok(()),
            };
            viewport.and(page)
        })
        .await
        .map_err(|_| capture_error("restoring the preview timed out"))?
        .map_err(|error| capture_error(format!("the preview could not be restored: {error}")))
    }

    async fn capture_final(
        &self,
        ctx: &DaemonContext,
        browser: &(dyn PreviewBrowser + 'static),
        page_at_finish: (&str, &str),
        note: &DesignNote,
        head: &str,
        moved: &mut bool,
    ) -> Result<(), ArchonError> {
        let (origin, start) = page_at_finish;
        let failed = |why: String| capture_error(format!("{}: {why}", note.id));
        let preset = note
            .target
            .as_ref()
            .map_or(note.viewport.preset, |t| t.viewport.preset);
        let (_, viewport) = to_adb_viewport(preset);
        browser
            .set_viewport(viewport)
            .await
            .map_err(|error| failed(format!("could not set the picture's viewport: {error}")))?;
        let shown = self.after_target(ctx, note).await?;
        let steps = shown.as_ref().map_or(&[][..], |target| &target.steps[..]);
        let page = shown
            .as_ref()
            .or(note.target.as_ref())
            .map_or(start, |target| target.path.as_str());
        if *moved || page != start {
            *moved = true;
            navigate_fresh(browser, origin, page)
                .await
                .map_err(failed)?;
        } else {
            reload_fresh(browser).await.map_err(failed)?;
        }
        *moved |= !steps.is_empty();
        replay_steps(browser, origin, steps).await.map_err(failed)?;
        let Some(clip) = final_clip(browser, note, !steps.is_empty())
            .await
            .map_err(|error| failed(format!("could not locate the element: {error}")))?
        else {
            return Ok(());
        };
        let picture = browser
            .capture(clip, 2.0)
            .await
            .map_err(|error| failed(format!("the screenshot failed: {error}")))?;
        self.note_unchanged_final(ctx, browser, note, &picture)
            .await?;
        self.keep_final(ctx, note, &picture, head).await
    }

    async fn after_target(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
    ) -> Result<Option<Target>, ArchonError> {
        let Some(after) = &note.after_asset else {
            return Ok(None);
        };
        let asset = ctx
            .store
            .lock()
            .await
            .design_asset(&note.item, after)?
            .ok_or_else(|| ArchonError::Corrupt(format!("missing after picture {after}")))?;
        Ok(asset.target)
    }

    /// A done note with no after picture whose final picture matches its
    /// before picture shows no change: say so on the note, once, so no screen
    /// offers the two as a comparison.
    async fn note_unchanged_final(
        &self,
        ctx: &DaemonContext,
        browser: &(dyn PreviewBrowser + 'static),
        note: &DesignNote,
        picture: &adb::Picture,
    ) -> Result<(), ArchonError> {
        let changed_code =
            note.kind != NoteKind::Revert && note.commit.is_some() && !note.no_change;
        if !changed_code
            || note.after_asset.is_some()
            || archon_core::design::no_visible_change(note).is_some()
            || !self.matches_before(ctx, browser, note, picture).await
        {
            return Ok(());
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
        ctx.store.lock().await.design_note_change(
            &note.item,
            &note.id,
            NoteChange::Thread(entry),
        )?;
        self.emit_note(ctx, &note.item, &note.id).await;
        Ok(())
    }

    async fn keep_final(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        picture: &adb::Picture,
        head: &str,
    ) -> Result<(), ArchonError> {
        let asset = self.store_final(ctx, note, picture, head).await?;
        ctx.store.lock().await.design_note_change(
            &note.item,
            &note.id,
            NoteChange::Final {
                asset: Some(asset),
                included: None,
                captured_commit: Some(head.to_owned()),
            },
        )?;
        Ok(())
    }

    async fn store_final(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        picture: &adb::Picture,
        head: &str,
    ) -> Result<archon_core::design::DesignAssetId, ArchonError> {
        assert!(!head.is_empty());
        assert_eq!(note.status, NoteStatus::Done);
        self.put_asset(
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
        .await?
        .map(|asset| asset.id)
        .ok_or_else(|| capture_error("the final picture was not stored"))
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
        let capture = row
            .finish_capture
            .clone()
            .ok_or_else(|| ArchonError::Corrupt("a finishing session has a capture ID".into()))?;
        let (active, notes, choices, approved) = {
            let store = ctx.store.lock().await;
            let choices =
                Self::review_choices(&store, &row.item, &row.id, row.finish_choices.as_ref())?;
            (
                self.active_notes(&store, wt, &row.item, &head, choices.as_ref())?,
                store.design_notes(&row.item)?,
                choices,
                super::approved_design(&store, &row.item)?,
            )
        };
        // Auto supersession is shown on the cards of the notes it covers.
        let all_active = {
            let store = ctx.store.lock().await;
            self.review_candidates(&store, wt, row, &head, &notes)?
        };
        let left_history =
            self.notes_left_history(wt, &head, row, &notes, &approved.ids, &all_active)?;
        let mut choices = choices;
        if let Some(choices) = choices.as_mut() {
            choices.superseded.retain(|_, by| {
                by.as_ref()
                    .is_none_or(|by| left_history.iter().all(|left| &left.note != by))
            });
        }
        let mut cards = self
            .review_cards(ctx, row, &notes, &all_active, choices.as_ref())
            .await?;
        cards.extend(left_history);
        let no_change_only =
            super::approval(&active, &notes, &row.id, &approved) == Approval::NoChange;
        let mut view = DesignFinishView {
            note_version: capture_version(&capture).unwrap_or(row.note_version),
            capture,
            head,
            cards,
            no_change_only,
        };
        self.attach_review_images(ctx, row, &mut view).await?;
        Ok(view)
    }

    async fn review_cards(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        notes: &[DesignNote],
        candidates: &[DesignNote],
        choices: Option<&FinishChoices>,
    ) -> Result<Vec<FinishCard>, ArchonError> {
        let refs: Vec<_> = candidates.iter().collect();
        let auto = archon_core::design::auto_supersession(&refs);
        let history = chain_history(notes, candidates);
        let browser_available = self
            .existing_runtime(&row.id)
            .and_then(|rt| rt.browser())
            .is_some();
        let store = ctx.store.lock().await;
        candidates
            .iter()
            .map(|note| {
                let superseded_by = choices
                    .and_then(|choices| choices.superseded.get(&note.id).cloned())
                    .unwrap_or_else(|| auto.get(&note.id).cloned());
                let asset = match &note.final_asset {
                    Some(id) => Some(store.design_asset(&row.item, id)?.ok_or_else(|| {
                        ArchonError::Corrupt(format!("missing final picture {id}"))
                    })?),
                    None => None,
                };
                let earlier = note.session != row.id;
                let not_found = !earlier
                    && browser_available
                    && asset.is_none()
                    && note
                        .target
                        .as_ref()
                        .is_some_and(|target| target.selector.is_some())
                    && superseded_by.is_none();
                let caption = match (earlier, asset.as_ref()) {
                    (true, Some(_)) => {
                        Some("Reviewed in an earlier session; kept as it was".into())
                    }
                    (true, None) => Some("From an earlier session, with no picture".into()),
                    (false, None) if !browser_available && superseded_by.is_none() => {
                        Some("No preview browser was available for a final picture".into())
                    }
                    (false, asset) => asset.and_then(caption_of),
                };
                Ok(FinishCard {
                    note: note.id.clone(),
                    text: note.text.clone(),
                    asset: asset.map(|asset| asset.id),
                    data_url: None,
                    caption,
                    include: note.final_included.unwrap_or(true) && note.final_asset.is_some(),
                    superseded_by,
                    not_found,
                    left_history: false,
                    earlier,
                    history: history.get(note.id.as_str()).cloned().unwrap_or_default(),
                })
            })
            .collect()
    }

    fn notes_left_history(
        &self,
        wt: &WorktreeRef,
        head: &str,
        row: &DesignSession,
        notes: &[DesignNote],
        approved: &[String],
        candidates: &[DesignNote],
    ) -> Result<Vec<FinishCard>, ArchonError> {
        let undone = undone_by(notes);
        let listed: std::collections::BTreeSet<_> =
            candidates.iter().map(|note| note.id.as_str()).collect();
        let mut cards = Vec::new();
        for note in notes {
            if listed.contains(note.id.as_str())
                || note.kind == NoteKind::Revert
                || note.status != NoteStatus::Done
                || undone.contains_key(&note.id)
                || approved.contains(&note.id)
                || !self.left_history(wt, note, head)?
            {
                continue;
            }
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
        Ok(cards)
    }

    async fn attach_review_images(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        view: &mut DesignFinishView,
    ) -> Result<(), ArchonError> {
        let text_bytes = serde_json::to_vec(view)
            .map_err(|error| ArchonError::Corrupt(format!("could not encode the review: {error}")))?
            .len();
        let mut remaining_bytes = MAX_SERIALIZED_FRAME_BYTES.saturating_sub(text_bytes);
        let store = ctx.store.lock().await;
        for card in &mut view.cards {
            let Some(id) = &card.asset else { continue };
            let asset = store
                .design_asset(&row.item, id)?
                .ok_or_else(|| ArchonError::Corrupt(format!("missing final picture {id}")))?;
            let encoded_bytes = asset.bytes.div_ceil(3) * 4 + 64;
            if encoded_bytes <= remaining_bytes as u64 {
                remaining_bytes -= encoded_bytes as usize;
                card.data_url = Some(self.data_url(&asset)?);
            }
        }
        Ok(())
    }

    fn left_history(
        &self,
        wt: &WorktreeRef,
        note: &DesignNote,
        head: &str,
    ) -> Result<bool, ArchonError> {
        use crate::worktree::design_git::{self, NoteGit};
        let Some(commit) = note.commit.as_deref() else {
            return Ok(false);
        };
        let shadow = self.shadow(wt)?;
        let g = NoteGit {
            shadow: &shadow,
            repo: &wt.repo,
            worktree: &wt.worktree,
            branch: &wt.branch,
        };
        design_git::contains(&g, commit, head).map(|present| !present)
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
        let lock = self.item_lock(&row.item);
        let _held = lock.lock().await;
        let row = self.current(ctx, session_id).await?;
        let wt = self.worktree_of(ctx, &row).await?;
        let head = self.check_review(&row, &wt, capture)?;
        let note = {
            let store = ctx.store.lock().await;
            let note = store
                .design_note(&row.item, note_id)?
                .ok_or_else(|| ArchonError::NotFound(format!("note {note_id}")))?;
            if note.session != row.id {
                return Err(refuse(
                    DesignRefusal::ReadOnly,
                    format!("{note_id} is from an earlier session; its picture was reviewed then"),
                ));
            }
            let notes = store.design_notes(&row.item)?;
            if !self
                .review_candidates(&store, &wt, &row, head, &notes)?
                .iter()
                .any(|eligible| eligible.id == note_id)
            {
                return Err(ArchonError::Invalid(format!(
                    "{note_id} is not part of this review"
                )));
            }
            note
        };
        let picture = self.capture_operator_frame(ctx, &row, &note).await?;
        self.check_review(&row, &wt, capture)?;
        let asset = self.store_final(ctx, &note, &picture, head).await?;
        let row = ctx.store.lock().await.design_finish_restamp(
            &row,
            &capture_id(row.note_version + 1),
            note_id,
            asset,
        )?;
        self.emit_session(ctx, &row.id).await;
        self.finish_view(ctx, &row, &wt)
            .await
            .map(Reply::DesignFinish)
    }

    async fn capture_operator_frame(
        &self,
        ctx: &DaemonContext,
        row: &DesignSession,
        note: &DesignNote,
    ) -> Result<adb::Picture, ArchonError> {
        let deadline = tokio::time::Instant::now() + MANUAL_BUDGET;
        let browser = tokio::time::timeout_at(deadline, self.ensure_browser(ctx, &row.id))
            .await
            .map_err(|_| capture_timeout(MANUAL_BUDGET))??
            .ok_or_else(|| capture_error("there is no preview browser available"))?;
        let preset = note
            .target
            .as_ref()
            .map_or(note.viewport.preset, |t| t.viewport.preset);
        let switched = row.viewport_preset != preset;
        let result = tokio::time::timeout_at(deadline, async {
            if switched {
                let (_, viewport) = to_adb_viewport(preset);
                browser.set_viewport(viewport).await?;
            }
            let clip = match note
                .target
                .as_ref()
                .and_then(|target| target.selector.as_deref())
            {
                Some(selector) => browser
                    .resolve_selector(selector, note.target.as_ref().map(recorded_box))
                    .await?
                    .map(|rect| grow(rect, MARGIN)),
                None => None,
            };
            browser.capture(clip, 2.0).await
        })
        .await
        .map_err(|_| capture_timeout(MANUAL_BUDGET))
        .and_then(|res| res.map_err(|error| capture_error(error.to_string())));
        let restore = if switched {
            Self::restore_preview(&*browser, row.viewport_preset, None).await
        } else {
            Ok(())
        };
        restored(result, restore)
    }

    fn check_review<'a>(
        &self,
        row: &'a DesignSession,
        wt: &WorktreeRef,
        capture: &str,
    ) -> Result<&'a str, ArchonError> {
        if row.state != SessionState::Finishing {
            return Err(conflict());
        }
        if row.finish_capture.as_deref() != Some(capture)
            || capture_version(capture) != Some(row.note_version)
        {
            return Err(stale());
        }
        let head = row
            .finish_head
            .as_deref()
            .filter(|head| !head.is_empty())
            .ok_or_else(|| ArchonError::Corrupt("a finishing session has a head".into()))?;
        if self.worktree_at(wt, head)?.is_err() {
            return Err(stale());
        }
        Ok(head)
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
        let row = self.current(ctx, session_id).await?;
        let lock = self.item_lock(&row.item);
        let _held = lock.lock().await;
        let row = self.current(ctx, session_id).await?;
        let wt = self.worktree_of(ctx, &row).await?;
        let head = self.check_review(&row, &wt, capture)?;
        let row = {
            let mut store = ctx.store.lock().await;
            let choices = FinishChoices {
                include,
                superseded,
            };
            let (choices, inclusion) =
                self.confirm_choices(&store, &row, &wt, head, choices, confirm_no_change)?;
            let owed = queued_payload(
                &store,
                &row,
                Owed {
                    key: format!("design:{}:finish", row.id),
                    text: FINISH_TEXT.to_owned(),
                    asset: None,
                    item: row.item.clone(),
                    more: Vec::new(),
                },
            )?;
            store.design_finish_confirm(
                &row.id,
                FinishRecord {
                    capture: capture.to_owned(),
                    head: head.to_owned(),
                    note_version: row.note_version,
                    choices: Some(choices),
                },
                &inclusion,
                &owed,
            )?
        };
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: finish confirmed");
        self.park_browser(&row.id, "confirmed");
        self.emit_session(ctx, &row.id).await;
        if let Err(error) = self.flush_owed(ctx, &row).await {
            tracing::warn!(%error, session = %row.id, "design: Finish delivery is owed and will retry");
        }
        Ok(Reply::DesignDone)
    }

    fn confirm_choices(
        &self,
        store: &archon_queue::store::Store,
        row: &DesignSession,
        wt: &WorktreeRef,
        head: &str,
        mut choices: FinishChoices,
        confirm_no_change: bool,
    ) -> Result<(FinishChoices, Vec<(String, bool)>), ArchonError> {
        let notes = store.design_notes(&row.item)?;
        let candidates = self.review_candidates(store, wt, row, head, &notes)?;
        let approved = super::approved_design(store, &row.item)?;
        let carried = Self::review_choices(store, &row.item, &row.id, None)?;
        self.validate_choices(
            (wt, head),
            row,
            (&notes, &candidates),
            &approved.ids,
            carried.as_ref(),
            &mut choices,
        )?;
        let in_force = Self::review_choices(store, &row.item, &row.id, Some(&choices))?;
        let active = self.active_notes(store, wt, &row.item, head, in_force.as_ref())?;
        match super::approval(&active, &notes, &row.id, &approved) {
            Approval::Nothing => {
                return Err(ArchonError::Invalid(super::NOTHING_TO_APPROVE.into()));
            }
            Approval::NoChange if !confirm_no_change => {
                return Err(ArchonError::Invalid(NO_CHANGE_UNCONFIRMED.into()));
            }
            _ => {}
        }
        let inclusion = active
            .iter()
            .filter(|note| note.session == row.id)
            .map(|note| {
                let include = choices.include.get(&note.id).copied().unwrap_or(true)
                    && note.final_asset.is_some();
                (note.id.clone(), include)
            })
            .collect();
        Ok((choices, inclusion))
    }

    fn validate_choices(
        &self,
        (wt, head): (&WorktreeRef, &str),
        row: &DesignSession,
        (notes, candidates): (&[DesignNote], &[DesignNote]),
        approved: &[String],
        carried: Option<&FinishChoices>,
        choices: &mut FinishChoices,
    ) -> Result<(), ArchonError> {
        let undone = undone_by(notes);
        for (id, by) in &choices.superseded {
            if by.is_some() && approved.contains(id) {
                return Err(ArchonError::Invalid(format!(
                    "{id} was approved earlier; add a new note instead"
                )));
            }
            let carried_by = carried.and_then(|choices| choices.superseded.get(id));
            self.validate_replacement(
                wt,
                head,
                (notes, &undone),
                candidates,
                id,
                by.as_deref(),
                carried_by == Some(by),
                approved,
            )?;
        }
        choices
            .superseded
            .retain(|id, _| candidates.iter().any(|note| &note.id == id));
        for id in choices.include.keys() {
            if !candidates.iter().any(|note| &note.id == id) {
                return Err(ArchonError::Invalid(format!(
                    "{id} is not part of this review"
                )));
            }
        }
        choices.include.retain(|id, _| {
            candidates
                .iter()
                .any(|note| &note.id == id && note.session == row.id && note.final_asset.is_some())
        });
        Ok(())
    }

    fn validate_replacement(
        &self,
        wt: &WorktreeRef,
        head: &str,
        (notes, undone): (&[DesignNote], &BTreeMap<String, String>),
        candidates: &[DesignNote],
        id: &str,
        by: Option<&str>,
        carried: bool,
        approved: &[String],
    ) -> Result<(), ArchonError> {
        let Some(source) = candidates.iter().find(|note| note.id == id) else {
            // Older clients send an empty choice for every card, including changes that left history.
            if by.is_none()
                && let Some(note) = notes.iter().find(|note| note.id == id)
                && note.kind != NoteKind::Revert
                && note.status == NoteStatus::Done
                && !undone.contains_key(id)
                && self.left_history(wt, note, head)?
            {
                return Ok(());
            }
            return Err(ArchonError::Invalid(format!(
                "{id} is not part of this review"
            )));
        };
        let Some(by) = by else { return Ok(()) };
        if candidates
            .iter()
            .any(|note| note.id == by && note.number > source.number)
        {
            return Ok(());
        }
        if carried
            && let Some(replacement) = notes
                .iter()
                .find(|note| note.id == by && note.number > source.number)
            && (approved.iter().any(|id| id == by) || !self.left_history(wt, replacement, head)?)
        {
            return Ok(());
        }
        Err(ArchonError::Invalid(format!(
            "{id} can only be superseded by a later note in this review"
        )))
    }

    fn review_candidates(
        &self,
        store: &archon_queue::store::Store,
        wt: &WorktreeRef,
        row: &DesignSession,
        head: &str,
        notes: &[DesignNote],
    ) -> Result<Vec<DesignNote>, ArchonError> {
        let choices = FinishChoices {
            include: BTreeMap::new(),
            superseded: notes.iter().map(|n| (n.id.clone(), None)).collect(),
        };
        self.active_notes(store, wt, &row.item, head, Some(&choices))
    }

    pub(crate) async fn finish_cancel(
        &self,
        ctx: &DaemonContext,
        session_id: &DesignSessionId,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session_id).await?;
        let lock = self.item_lock(&row.item);
        let held = lock.lock().await;
        let row = self.current(ctx, session_id).await?;
        if row.state != SessionState::Finishing {
            return Err(conflict());
        }
        let row = ctx.store.lock().await.design_session_transition(
            &row.id,
            SessionState::Finishing,
            SessionState::Live,
            None,
            false,
        )?;
        tracing::info!(session = %row.id, device = ?super::current_device_label(), "design: back to editing");
        self.emit_session(ctx, &row.id).await;
        drop(held);
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
        let lock = self.item_lock(&row.item);
        let _held = lock.lock().await;
        let row = self.current(ctx, session_id).await?;
        if row.state == SessionState::Confirmed {
            return Err(super::finishing());
        }
        if let Some(browser) = self.existing_runtime(&row.id).and_then(|rt| rt.browser()) {
            let (_, v) = to_adb_viewport(preset);
            browser
                .set_viewport(v)
                .await
                .map_err(|e| ArchonError::Unavailable(e.to_string()))?;
        }
        ctx.store.lock().await.design_session_patch(
            &row.id,
            archon_queue::store::SessionPatch {
                viewport_preset: Some(preset),
                ..Default::default()
            },
        )?;
        tracing::info!(session = %row.id, ?preset, device = ?super::current_device_label(), "design: viewport changed");
        self.emit_session(ctx, &row.id).await;
        Ok(Reply::DesignDone)
    }
}

fn chain_history<'a>(
    notes: &'a [DesignNote],
    candidates: &'a [DesignNote],
) -> BTreeMap<&'a str, Vec<FinishHistoryEntry>> {
    let chains = archon_core::design::chains(notes);
    let undone = undone_by(notes);
    let representatives: BTreeMap<_, _> = candidates
        .iter()
        .filter_map(|note| {
            chains
                .get(&note.id)
                .map(|chain| (chain.root.as_str(), note))
        })
        .collect();
    let mut history = BTreeMap::<&str, Vec<_>>::new();
    for note in notes.iter().filter(|note| note.status == NoteStatus::Done) {
        let Some(card) = chains
            .get(&note.id)
            .and_then(|chain| representatives.get(chain.root.as_str()))
            .filter(|card| card.number > note.number)
        else {
            continue;
        };
        history
            .entry(card.id.as_str())
            .or_default()
            .push(FinishHistoryEntry {
                note: note.id.clone(),
                text: note.text.clone(),
                set_aside: undone.contains_key(&note.id),
            });
    }
    history
}

pub(crate) fn origin_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("http://{authority}")
}

/// What the final picture covers: nothing is `None` (the note's element is
/// gone), `Some(None)` the whole view (a state reached by steps, or no target).
async fn final_clip(
    browser: &(dyn PreviewBrowser + 'static),
    note: &DesignNote,
    stepped: bool,
) -> Result<Option<Option<adb::ClipRect>>, adb::BrowserError> {
    let target = match note.target.as_ref() {
        _ if stepped => return Ok(Some(None)),
        None => return Ok(Some(None)),
        Some(target) => target,
    };
    if target.region {
        return Ok(Some(Some(clip_of(target, 0.0))));
    }
    let Some(selector) = target.selector.as_deref() else {
        return Ok(None);
    };
    let rect = browser
        .resolve_selector(selector, Some(recorded_box(target)))
        .await?;
    Ok(rect.map(|rect| Some(grow(rect, MARGIN))))
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
