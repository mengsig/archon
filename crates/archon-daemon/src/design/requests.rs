//! The design requests as `handlers.rs` routes them.

use archon_core::design::{ComponentRef, MAX_INPUT_EVENTS, Rect, Target};
use archon_core::error::ArchonError;
use archon_core::protocol::{
    ClientKind, DesignResolvedView, InputEvent, KeyAction, MouseAction, MouseButton, Reply,
    Request, TouchAction,
};
use archon_design_browser as adb;

use super::DesignService;
use super::assets::NewAsset;
use super::browser::{PreviewBrowser, to_adb_viewport, within_rate};
use crate::context::DaemonContext;

/// Most hover and select resolves a second, and input requests a second.
const RESOLVES_PER_SECOND: u32 = 10;
const INPUTS_PER_SECOND: u32 = 60;
/// Input requests run in separate tasks. Each is ready by this deadline and the
/// browser bounds its turn (10 s with releases owed), so one ends within 20 s,
/// well before the web app's 60 s wait sends the next.
const READY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// `ready` bounded by [`READY_DEADLINE`]: past it nothing has been dispatched,
/// and the caller hears that its input did not reach the app.
async fn within_ready_deadline<T>(
    ready: impl std::future::Future<Output = Result<T, ArchonError>>,
) -> Result<T, ArchonError> {
    match tokio::time::timeout(READY_DEADLINE, ready).await {
        Ok(done) => done,
        Err(_) => Err(ArchonError::Unavailable(format!(
            "the preview was not ready for input within {} s; it did not reach the app",
            READY_DEADLINE.as_secs()
        ))),
    }
}

/// Tells the session's browser its input was lost when dropped before `done`:
/// on an error, and when the request is cancelled because its connection closed.
struct InputOutcome<'a> {
    service: &'a DesignService,
    session: &'a archon_core::design::DesignSessionId,
    done: bool,
}

impl Drop for InputOutcome<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.service.input_lost(self.session);
        }
    }
}

fn validate_input(event: &InputEvent, width: u32, height: u32) -> Result<(), ArchonError> {
    let point =
        |x, y| (0.0..=f64::from(width)).contains(&x) && (0.0..=f64::from(height)).contains(&y);
    let valid = match event {
        InputEvent::Mouse {
            x,
            y,
            click_count,
            modifiers,
            ..
        } => point(*x, *y) && *click_count <= 32 && *modifiers <= 15,
        InputEvent::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => {
            point(*x, *y) && dx.abs() <= 1_000_000.0 && dy.abs() <= 1_000_000.0 && *modifiers <= 15
        }
        InputEvent::Text { text } => text.len() <= 16 * 1024,
        InputEvent::Key {
            key,
            code,
            text,
            modifiers,
            ..
        } => {
            !key.is_empty()
                && key.len() <= 128
                && code.len() <= 128
                && text.as_ref().is_none_or(|v| v.len() <= 128)
                && *modifiers <= 15
        }
        InputEvent::Touch { points, .. } => {
            points.len() <= 10
                && points.iter().enumerate().all(|(index, p)| {
                    point(p.x, p.y) && points[..index].iter().all(|earlier| earlier.id != p.id)
                })
        }
    };
    if !valid {
        return Err(ArchonError::Invalid(
            "preview input has an invalid coordinate, modifier, size or touch identifier".into(),
        ));
    }
    Ok(())
}

fn input_of(event: InputEvent) -> adb::Input {
    match event {
        InputEvent::Mouse {
            action,
            x,
            y,
            button,
            click_count,
            modifiers,
        } => adb::Input::Mouse {
            action: match action {
                MouseAction::Move => adb::MouseAction::Move,
                MouseAction::Down => adb::MouseAction::Down,
                MouseAction::Up => adb::MouseAction::Up,
            },
            x,
            y,
            button: match button {
                MouseButton::None => adb::MouseButton::None,
                MouseButton::Left => adb::MouseButton::Left,
                MouseButton::Middle => adb::MouseButton::Middle,
                MouseButton::Right => adb::MouseButton::Right,
            },
            click_count,
            modifiers,
        },
        InputEvent::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        } => adb::Input::Wheel {
            x,
            y,
            dx,
            dy,
            modifiers,
        },
        InputEvent::Key {
            action,
            key,
            code,
            text,
            modifiers,
        } => adb::Input::Key {
            action: match action {
                KeyAction::Down => adb::KeyAction::Down,
                KeyAction::Up => adb::KeyAction::Up,
            },
            key,
            code,
            text,
            modifiers,
        },
        InputEvent::Text { text } => adb::Input::Text { text },
        InputEvent::Touch { action, points } => adb::Input::Touch {
            action: match action {
                TouchAction::Start => adb::TouchAction::Start,
                TouchAction::Move => adb::TouchAction::Move,
                TouchAction::End => adb::TouchAction::End,
                TouchAction::Cancel => adb::TouchAction::Cancel,
            },
            points: points
                .into_iter()
                .map(|p| adb::TouchPoint {
                    x: p.x,
                    y: p.y,
                    id: p.id,
                })
                .collect(),
        },
    }
}

fn unavailable(e: adb::BrowserError) -> ArchonError {
    ArchonError::Unavailable(format!("the preview browser: {e}"))
}

pub(crate) async fn handle(
    ctx: &DaemonContext,
    client: ClientKind,
    request: Request,
) -> Result<Reply, ArchonError> {
    let service = &ctx.design;
    // Observer devices see a one-line status and nothing else.
    if client == ClientKind::ArchonWeb {
        return match request {
            Request::DesignGet { item } => {
                let store = ctx.store.lock().await;
                let state = store.design_current_session(&item)?.map(|s| s.state);
                let note_count = store.design_notes(&item)?.len() as u32;
                Ok(Reply::DesignSummary { state, note_count })
            }
            _ => Err(ArchonError::Forbidden(
                "an observer device cannot use design sessions; pair it as an operator".into(),
            )),
        };
    }
    if !service.enabled() && !matches!(request, Request::DesignGet { .. }) {
        return Err(ArchonError::Invalid(
            "design sessions are turned off".into(),
        ));
    }
    let preview_session = match &request {
        Request::DesignInput { session, .. }
        | Request::DesignReload { session }
        | Request::DesignDialogAnswer { session, .. }
        | Request::DesignResolve { session, .. }
        | Request::DesignRegion { session, .. } => Some(session),
        _ => None,
    };
    let _preview_guard = if let Some(id) = preview_session {
        let row = service.current(ctx, id).await?;
        let held = service.item_lock(&row.item).lock_owned().await;
        let current = service.current(ctx, id).await?;
        if current.state == archon_core::design::SessionState::Confirmed {
            return Err(super::finishing());
        }
        Some(held)
    } else {
        None
    };
    match request {
        Request::DesignGet { item } => service.state_view(ctx, &item).await,
        Request::SubscribeDesign { .. } | Request::DesignUnsubscribe { .. } => Err(
            ArchonError::Invalid("design subscriptions are managed by their connection".into()),
        ),
        Request::DesignAsset { item, id } => service.asset_view(ctx, &item, &id).await,
        Request::DesignFrameAck { .. } => Err(ArchonError::Invalid(
            "design_frame_ack is served by the design subscription's connection".into(),
        )),
        Request::DesignInput { session, events } => service.input(ctx, &session, events).await,
        Request::DesignResolve {
            session,
            x,
            y,
            depth,
            select,
        } => service.resolve(ctx, &session, x, y, depth, select).await,
        Request::DesignRegion { session, rect } => service.region(ctx, &session, rect).await,
        Request::DesignUpload {
            session,
            media_type,
            data,
            name,
            sketch,
            ..
        } => {
            let row = service.current(ctx, &session).await?;
            service
                .upload(ctx, &row, &media_type, &data, name, sketch)
                .await
        }
        Request::DesignViewport { session, preset } => {
            service.set_viewport(ctx, &session, preset).await
        }
        Request::DesignReload { session } => {
            service.reload_preview(ctx, &session).await?;
            Ok(Reply::DesignDone)
        }
        Request::DesignTellAgent { session } => service.tell_agent(ctx, &session).await,
        Request::DesignDialogAnswer {
            session,
            accept,
            text,
        } => {
            if text.as_ref().is_some_and(|value| value.len() > 16 * 1024) {
                return Err(ArchonError::Invalid("dialog text exceeds 16 KiB".into()));
            }
            let row = service.current(ctx, &session).await?;
            let browser = service
                .existing_runtime(&row.id)
                .and_then(|rt| rt.browser())
                .ok_or_else(|| ArchonError::Unavailable("there is no preview browser".into()))?;
            browser
                .answer_dialog(accept, text)
                .await
                .map_err(unavailable)?;
            Ok(Reply::DesignDone)
        }
        Request::DesignNoteAdd {
            session,
            client_key,
            text,
            asset,
            send_picture,
            details_text,
            follows,
            refs,
        } => {
            service
                .note_add(
                    ctx,
                    &session,
                    client_key,
                    text,
                    asset,
                    send_picture,
                    details_text,
                    follows,
                    refs,
                )
                .await
        }
        Request::DesignNoteContinueFrom {
            item,
            id,
            text,
            client_key,
            refs,
        } => {
            service
                .note_continue_from(ctx, &item, &id, text, client_key, refs)
                .await
        }
        Request::DesignNoteEdit { item, id, text } => {
            service.note_edit(ctx, &item, &id, text).await
        }
        Request::DesignNoteCancel { item, id } => service.note_cancel(ctx, &item, &id).await,
        Request::DesignNoteReply { item, id, text } => {
            service.note_answer(ctx, &item, &id, text).await
        }
        Request::DesignNoteDrop { item, id } => service.note_drop(ctx, &item, &id).await,
        Request::DesignNoteUndo { item, id } => service.note_undo(ctx, &item, &id).await,
        Request::DesignNoteResend { item, id } => service.note_resend(ctx, &item, &id).await,
        Request::DesignFinishCapture {
            session,
            cancel_queued,
            recapture,
        } => {
            service
                .finish_capture(ctx, &session, cancel_queued, recapture)
                .await
        }
        Request::DesignFinalCapture {
            session,
            capture,
            note,
        } => service.final_capture(ctx, &session, &capture, &note).await,
        Request::DesignFinishConfirm {
            session,
            capture,
            include,
            superseded,
            confirm_no_change,
        } => {
            service
                .finish_confirm(
                    ctx,
                    &session,
                    &capture,
                    include,
                    superseded,
                    confirm_no_change,
                )
                .await
        }
        Request::DesignFinishCancel { session } => service.finish_cancel(ctx, &session).await,
        Request::DesignPreview { url, caller } => {
            service.design_preview(ctx, &url, caller.as_ref()).await
        }
        Request::DesignNoteReport {
            id,
            status,
            reply,
            commit,
            no_change,
            path,
            steps,
            caller,
        } => {
            let report = super::reports::NoteReport {
                id,
                status,
                reply,
                commit,
                no_change,
                path,
                steps,
            };
            service.note_report(ctx, report, caller.as_ref()).await
        }
        Request::DesignHistory { id, caller } => {
            service.history(ctx, id.as_deref(), caller.as_ref()).await
        }
        other => Err(ArchonError::Invalid(format!(
            "not a design request: {other:?}"
        ))),
    }
}

impl DesignService {
    async fn browser_for_input(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
    ) -> Result<std::sync::Arc<dyn PreviewBrowser>, ArchonError> {
        let row = self.current(ctx, session).await?;
        if let Some(b) = self.existing_runtime(&row.id).and_then(|rt| rt.browser()) {
            return Ok(b);
        }
        self.ensure_browser(ctx, &row.id)
            .await?
            .ok_or_else(|| ArchonError::Unavailable("there is no preview to use yet".into()))
    }

    /// One input batch. Any that fails, times out, is refused or is cancelled
    /// may have carried a release, so the session's browser lets go of what it holds.
    async fn input(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        events: Vec<InputEvent>,
    ) -> Result<Reply, ArchonError> {
        let mut outcome = InputOutcome {
            service: self,
            session,
            done: false,
        };
        let reply = self.dispatch_input(ctx, session, events).await?;
        outcome.done = true;
        Ok(reply)
    }

    /// An input batch for `session` failed or was refused before it ran.
    pub(crate) fn input_lost(&self, session: &archon_core::design::DesignSessionId) {
        if let Some(browser) = self.existing_runtime(session).and_then(|rt| rt.browser()) {
            browser.input_lost();
        }
    }

    async fn dispatch_input(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        events: Vec<InputEvent>,
    ) -> Result<Reply, ArchonError> {
        if events.len() > MAX_INPUT_EVENTS {
            return Err(ArchonError::Invalid(format!(
                "at most {MAX_INPUT_EVENTS} input events a request"
            )));
        }
        let browser = within_ready_deadline(self.ready_for_input(ctx, session, &events)).await?;
        let mapped: Vec<adb::Input> = events.into_iter().map(input_of).collect();
        browser.input(&mapped).await.map_err(unavailable)?;
        Ok(Reply::DesignDone)
    }

    /// Checks `events` and the rate, then the browser to send them to.
    async fn ready_for_input(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        events: &[InputEvent],
    ) -> Result<std::sync::Arc<dyn PreviewBrowser>, ArchonError> {
        let row = self.current(ctx, session).await?;
        let (width, height) = row.viewport_preset.size();
        for event in events {
            validate_input(event, width, height)?;
        }
        let rt = self.runtime(&row.id);
        {
            let mut st = rt.st();
            if !within_rate(&mut st.input_window, INPUTS_PER_SECOND) {
                return Err(ArchonError::Policy(
                    "too many input requests; slow down".into(),
                ));
            }
            st.counters.inputs += events.len() as u64;
            st.pending_input = Some(std::time::Instant::now());
        }
        self.browser_for_input(ctx, session).await
    }

    async fn resolve(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        x: f64,
        y: f64,
        depth: i32,
        select: bool,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session).await?;
        let (viewport, _) = to_adb_viewport(row.viewport_preset);
        if !(0.0..=viewport.w as f64).contains(&x) || !(0.0..=viewport.h as f64).contains(&y) {
            return Err(ArchonError::Invalid(
                "that point is outside the preview".into(),
            ));
        }
        if !(-64..=64).contains(&depth) {
            return Err(ArchonError::Invalid(
                "selection depth must be between -64 and 64".into(),
            ));
        }
        let rt = self.runtime(&row.id);
        if !within_rate(&mut rt.st().resolve_window, RESOLVES_PER_SECOND) {
            return Err(ArchonError::Policy("too many resolves; slow down".into()));
        }
        let browser = self.browser_for_input(ctx, session).await?;
        let found = browser.resolve(x, y, depth).await.map_err(unavailable)?;
        let target = Target {
            path: found.path.clone(),
            selector: found.selector.clone(),
            role_name: found.role_name.clone(),
            component: found
                .component
                .clone()
                .map(|(name, framework)| ComponentRef { name, framework }),
            rect: Rect {
                x: found.x,
                y: found.y,
                w: found.w,
                h: found.h,
            },
            viewport,
            styles: found.styles.clone(),
            region: false,
            steps: Box::default(),
        };
        self.emit(
            &row.id,
            archon_core::protocol::DesignEvent::Selection {
                rect: target.rect,
                label: found.label.clone(),
            },
        );
        if !select {
            return Ok(Reply::DesignResolved(DesignResolvedView {
                target,
                label: found.label,
                asset: None,
                data_url: None,
            }));
        }
        self.crop(ctx, &row.item, browser, target, found.label)
            .await
    }

    async fn region(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        rect: Rect,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session).await?;
        let (viewport, _) = to_adb_viewport(row.viewport_preset);
        let inside = rect.w > 0.0
            && rect.h > 0.0
            && rect.x >= 0.0
            && rect.y >= 0.0
            && rect.x + rect.w <= viewport.w as f64 + 1.0
            && rect.y + rect.h <= viewport.h as f64 + 1.0;
        if !inside {
            return Err(ArchonError::Invalid(
                "that region is outside the preview".into(),
            ));
        }
        let browser = self.browser_for_input(ctx, session).await?;
        let path = browser.current_path().await.map_err(unavailable)?;
        let target = Target {
            path,
            selector: None,
            role_name: None,
            component: None,
            rect,
            viewport,
            styles: Default::default(),
            region: true,
            steps: Box::default(),
        };
        self.crop(ctx, &row.item, browser, target, "region".into())
            .await
    }

    /// Capture the crop now, at DPR 2, as a pending asset with its target.
    async fn crop(
        &self,
        ctx: &DaemonContext,
        item: &archon_core::id::QueueItemId,
        browser: std::sync::Arc<dyn PreviewBrowser>,
        target: Target,
        label: String,
    ) -> Result<Reply, ArchonError> {
        let clip = adb::ClipRect {
            x: target.rect.x,
            y: target.rect.y,
            w: target.rect.w,
            h: target.rect.h,
        };
        let picture = browser
            .capture(Some(clip), 2.0)
            .await
            .map_err(unavailable)?;
        let asset = self
            .put_asset(
                ctx,
                item,
                NewAsset {
                    source: archon_core::design::AssetSource::Capture,
                    target: Some(target.clone()),
                    note: None,
                    commit: None,
                    mime: picture.mime,
                    bytes: &picture.bytes,
                    width: picture.width,
                    height: picture.height,
                    sketch: None,
                    capture: Some(super::assets::capture_of(&picture)),
                },
            )
            .await?
            .ok_or_else(|| ArchonError::Invalid("the picture was not kept".into()))?;
        let data_url = self.data_url(&asset)?;
        Ok(Reply::DesignResolved(DesignResolvedView {
            target,
            label,
            asset: Some(asset.id),
            data_url: Some(data_url),
        }))
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use archon_core::protocol::TouchPoint;

    #[test]
    fn reject_duplicate_fingers_and_points_outside_the_preview() {
        let touch = |points| InputEvent::Touch {
            action: TouchAction::Move,
            points,
        };
        let finger = TouchPoint {
            x: 5.0,
            y: 6.0,
            id: 1,
        };
        assert!(validate_input(&touch(vec![finger]), 40, 60).is_ok());
        assert!(validate_input(&touch(vec![finger, finger]), 40, 60).is_err());
        assert!(validate_input(&touch(vec![finger]), 1, 1).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn input_whose_preview_is_not_ready_ends_by_its_deadline_and_says_so() {
        let started = tokio::time::Instant::now();
        let hung = within_ready_deadline::<()>(std::future::pending()).await;
        assert!(
            matches!(&hung, Err(ArchonError::Unavailable(m)) if m.contains("did not reach the app"))
        );
        assert_eq!(started.elapsed(), READY_DEADLINE);
        assert_eq!(within_ready_deadline(async { Ok(7) }).await.ok(), Some(7));
        let failed =
            within_ready_deadline::<()>(async { Err(ArchonError::Invalid("x".into())) }).await;
        assert!(matches!(failed, Err(ArchonError::Invalid(_))));
    }

    #[test]
    fn refuse_oversized_paste_before_starting_input() {
        let text = |length| InputEvent::Text {
            text: "x".repeat(length),
        };
        assert!(validate_input(&text(16 * 1024), 20, 30).is_ok());
        assert!(validate_input(&text(16 * 1024 + 1), 20, 30).is_err());
    }
}
