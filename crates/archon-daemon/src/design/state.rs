//! The pictures of a note's change: taken on a page that has finished
//! reloading, in the UI state the agent named (a dialog a button opens, a
//! hovered menu), and compared with the before picture so two identical
//! pictures are never offered as a comparison.

use std::time::Duration;

use archon_core::design::{
    AssetSource, DesignNote, DesignSession, MAX_STEP_WAIT_MS, Target, UiStep,
};
use archon_core::error::ArchonError;
use archon_design_browser as adb;
use archon_queue::store::NoteChange;

use super::assets::NewAsset;
use super::browser::PreviewBrowser;
use super::finish::{origin_of, recorded_box};
use super::reports::page_target;
use super::{DesignService, WorktreeRef};
use crate::context::DaemonContext;

/// Longest a page gets to load and stop moving.
const SETTLE_MAX: Duration = Duration::from_secs(10);
/// Longest one whole picture (load, steps, capture) takes.
const CAPTURE_MAX: Duration = Duration::from_secs(40);
/// Longest a page gets to answer one step.
const STEP_SETTLE: Duration = Duration::from_secs(3);

/// Where and how the after picture is taken.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AfterShot<'a> {
    /// The page a page note's change shows on.
    pub path: Option<&'a str>,
    /// The steps to the state that shows the change.
    pub steps: &'a [UiStep],
}

/// What taking the after picture came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterOutcome {
    /// A picture that differs from the before one is kept.
    Kept,
    /// The picture matched the before picture: nothing was kept.
    Matched,
    /// No picture could be taken (no browser, a failed capture).
    Skipped,
    /// A step could not be replayed; why, in plain words.
    StepFailed(String),
}

/// Replay `steps` from the page loaded at `origin`. `Err` says which step
/// failed and why, in words for the agent.
pub(crate) async fn replay_steps(
    browser: &dyn PreviewBrowser,
    origin: &str,
    steps: &[UiStep],
) -> Result<(), String> {
    for (n, step) in steps.iter().enumerate() {
        let at = n + 1;
        let failed = |why: adb::BrowserError| format!("step {at}: {why}");
        match step {
            UiStep::Open { path } => {
                let url = format!("{origin}{path}");
                browser
                    .navigate_settled(&url, SETTLE_MAX)
                    .await
                    .map_err(failed)?;
            }
            UiStep::Click { selector } | UiStep::Hover { selector } => {
                let Some((x, y)) = browser.center_of(selector).await.map_err(failed)? else {
                    return Err(format!(
                        "step {at}: no visible element matches `{selector}`"
                    ));
                };
                let click = matches!(step, UiStep::Click { .. });
                browser.input(&pointer(x, y, click)).await.map_err(failed)?;
                browser.wait_settled(STEP_SETTLE).await.map_err(failed)?;
            }
            UiStep::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(u64::from(
                    (*ms).min(MAX_STEP_WAIT_MS),
                )))
                .await;
            }
        }
    }
    Ok(())
}

/// A move over a point, then, for a click, the press and release.
fn pointer(x: f64, y: f64, click: bool) -> Vec<adb::Input> {
    let mouse = |action, button, click_count| adb::Input::Mouse {
        action,
        x,
        y,
        button,
        click_count,
        modifiers: 0,
    };
    let mut events = vec![mouse(adb::MouseAction::Move, adb::MouseButton::None, 0)];
    if click {
        events.push(mouse(adb::MouseAction::Down, adb::MouseButton::Left, 1));
        events.push(mouse(adb::MouseAction::Up, adb::MouseButton::Left, 1));
    }
    events
}

/// A fresh load of `page` (a reload when the preview is already there, so the
/// query and fragment stay), settled. Returns the path the page is at.
async fn load_fresh(
    browser: &dyn PreviewBrowser,
    session: &DesignSession,
    page: Option<&str>,
) -> Option<String> {
    let current = browser.current_path().await.ok();
    let loaded = match (page, session.preview_url.as_deref()) {
        (Some(page), Some(url)) if current.as_deref() != Some(page) => {
            let to = format!("{}{page}", origin_of(url));
            browser.navigate_settled(&to, SETTLE_MAX).await
        }
        _ => browser.reload_settled(SETTLE_MAX).await,
    };
    match loaded {
        Ok(true) => {}
        Ok(false) => tracing::info!("design: the page had not settled when its picture was taken"),
        Err(error) => tracing::info!(%error, "design: the preview did not reload for the picture"),
    }
    page.map(str::to_owned).or(current)
}

impl DesignService {
    /// The operator-only picture after a change: the page freshly loaded and
    /// settled, the agent's steps replayed, the same clip at the element's new
    /// box (the whole view when steps open a state). A page note done with a
    /// `path` is pictured whole at that path, and its after picture records
    /// it for the final picture. A picture that matches the before picture is
    /// not kept.
    pub(crate) async fn after_picture(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        wt: &WorktreeRef,
        shot: AfterShot<'_>,
    ) -> AfterOutcome {
        match tokio::time::timeout(CAPTURE_MAX, self.take_after(ctx, session, note, wt, shot)).await
        {
            Ok(outcome) => outcome,
            Err(_) => AfterOutcome::StepFailed(format!(
                "the page did not finish loading and answering within {} seconds",
                CAPTURE_MAX.as_secs()
            )),
        }
    }

    async fn take_after(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        wt: &WorktreeRef,
        shot: AfterShot<'_>,
    ) -> AfterOutcome {
        let Some(browser) = self
            .existing_runtime(&session.id)
            .and_then(|rt| rt.browser())
        else {
            return AfterOutcome::Skipped;
        };
        let page = shot
            .path
            .or_else(|| note.target.as_ref().map(|t| t.path.as_str()));
        let base = load_fresh(&*browser, session, page).await;
        if let Some(url) = session.preview_url.as_deref()
            && let Err(why) = replay_steps(&*browser, &origin_of(url), shot.steps).await
        {
            return AfterOutcome::StepFailed(why);
        }
        let clip = match (shot.steps.is_empty(), note.target.as_ref()) {
            (true, Some(target)) => match target.selector.as_deref() {
                Some(selector) => browser
                    .resolve_selector(selector, Some(recorded_box(target)))
                    .await
                    .ok()
                    .flatten(),
                None => None,
            },
            _ => None,
        };
        let picture = match browser.capture(clip, 2.0).await {
            Ok(picture) => picture,
            Err(error) => {
                tracing::info!(%error, "design: the after picture was not taken");
                return AfterOutcome::Skipped;
            }
        };
        if self.matches_before(ctx, &*browser, note, &picture).await {
            return AfterOutcome::Matched;
        }
        let target = recorded_state(note, shot, base.as_deref());
        let head = self.head_of(wt).unwrap_or_default();
        match self.keep_after(ctx, note, target, &picture, &head).await {
            Ok(()) => AfterOutcome::Kept,
            Err(error) => {
                tracing::info!(%error, "design: the after picture was not kept");
                AfterOutcome::Skipped
            }
        }
    }

    /// Whether `picture` shows what the note's before picture shows. Only a
    /// page capture is a baseline: a drawing or an upload is not the page.
    pub(crate) async fn matches_before(
        &self,
        ctx: &DaemonContext,
        browser: &dyn PreviewBrowser,
        note: &DesignNote,
        picture: &adb::Picture,
    ) -> bool {
        let Some(id) = note.before_asset.as_ref() else {
            return false;
        };
        let before = match ctx.store.lock().await.design_asset(&note.item, id) {
            Ok(Some(asset)) if asset.source == AssetSource::Capture => asset,
            Ok(_) => return false,
            Err(error) => {
                tracing::warn!(%error, note = %note.id, "design: the before picture could not be read to compare");
                return false;
            }
        };
        let bytes = match self.asset_bytes(&before) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, note = %note.id, "design: the before picture file could not be read to compare");
                return false;
            }
        };
        match browser
            .pictures_match((&before.mime, &bytes), (picture.mime, &picture.bytes))
            .await
        {
            Ok(same) => same,
            Err(error) => {
                tracing::info!(%error, "design: the pictures could not be compared");
                false
            }
        }
    }

    /// Retake the note's before picture in the state `steps` reach, on the
    /// code as it is now (the agent has not changed it yet). `Err` says why
    /// it could not, in words for the agent.
    pub(crate) async fn before_in_state(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        steps: &[UiStep],
    ) -> Result<(), String> {
        tokio::time::timeout(CAPTURE_MAX, self.take_before(ctx, session, note, steps))
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "the page did not finish loading and answering within {} seconds",
                    CAPTURE_MAX.as_secs()
                ))
            })
    }

    async fn take_before(
        &self,
        ctx: &DaemonContext,
        session: &DesignSession,
        note: &DesignNote,
        steps: &[UiStep],
    ) -> Result<(), String> {
        if !self.before_is_replaceable(ctx, note).await {
            tracing::info!(note = %note.id, "design: the operator's own picture stays the before picture");
            return Ok(());
        }
        let browser = match self.ensure_browser(ctx, &session.id).await {
            Ok(Some(browser)) => browser,
            Ok(None) | Err(_) => return Err("the preview browser is not running".into()),
        };
        let page = note.target.as_ref().map(|t| t.path.as_str());
        let base = load_fresh(&*browser, session, page).await;
        if let Some(url) = session.preview_url.as_deref() {
            replay_steps(&*browser, &origin_of(url), steps).await?;
        }
        let picture = browser
            .capture(None, 2.0)
            .await
            .map_err(|e| format!("the before picture was not taken: {e}"))?;
        let shot = AfterShot { path: None, steps };
        let target = recorded_state(note, shot, base.as_deref());
        self.keep_before(ctx, note, target, &picture)
            .await
            .map_err(|e| format!("the before picture was not kept: {e}"))
    }

    /// Whether the note's before picture may be retaken: a page capture, or
    /// none yet. A sketch or an upload is the operator's own and stays.
    async fn before_is_replaceable(&self, ctx: &DaemonContext, note: &DesignNote) -> bool {
        let Some(id) = note.before_asset.as_ref() else {
            return true;
        };
        match ctx.store.lock().await.design_asset(&note.item, id) {
            Ok(Some(asset)) => asset.source == AssetSource::Capture,
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(%error, note = %note.id, "design: the before picture could not be read");
                false
            }
        }
    }

    async fn keep_before(
        &self,
        ctx: &DaemonContext,
        note: &DesignNote,
        target: Option<Target>,
        picture: &adb::Picture,
    ) -> Result<(), ArchonError> {
        let asset = self
            .put_asset(
                ctx,
                &note.item,
                NewAsset {
                    source: AssetSource::Capture,
                    target,
                    note: Some(note.id.clone()),
                    commit: note.base_commit.clone(),
                    mime: picture.mime,
                    bytes: &picture.bytes,
                    width: picture.width,
                    height: picture.height,
                    sketch: None,
                },
            )
            .await?
            .ok_or_else(|| ArchonError::Invalid("the picture was not stored".into()))?;
        ctx.store.lock().await.design_note_change(
            &note.item,
            &note.id,
            NoteChange::Before { asset: asset.id },
        )?;
        Ok(())
    }
}

/// What a picture records of where it was taken: the page note's own page, or
/// the note's target, with the steps that reached the state. A note with no
/// target is recorded only for a page (its `path`) or for steps (the page they
/// started from).
fn recorded_state(note: &DesignNote, shot: AfterShot<'_>, base: Option<&str>) -> Option<Target> {
    let mut target = match (shot.path, note.target.as_ref(), base) {
        (Some(path), _, _) => Some(page_target(path, note)),
        (None, Some(target), _) => Some(target.clone()),
        (None, None, Some(base)) if !shot.steps.is_empty() => Some(page_target(base, note)),
        _ => None,
    };
    if let Some(target) = target.as_mut() {
        target.steps = shot.steps.into();
    }
    target
}
