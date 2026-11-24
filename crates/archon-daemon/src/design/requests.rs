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
    match request {
        Request::DesignGet { item } => service.state_view(ctx, &item).await,
        Request::SubscribeDesign { .. } => Err(ArchonError::Invalid(
            "subscribe_design is served by the connection".into(),
        )),
        Request::DesignAsset { item, id } => service.asset_view(ctx, &item, &id).await,
        Request::DesignFrameAck { sub, seq } => {
            service.frame_ack(sub, seq);
            Ok(Reply::DesignDone)
        }
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
            let row = service.current(ctx, &session).await?;
            if let Some(browser) = service.ensure_browser(ctx, &row.id).await? {
                browser.reload().await.map_err(unavailable)?;
            }
            Ok(Reply::DesignDone)
        }
        Request::DesignTellAgent { session } => service.tell_agent(ctx, &session).await,
        Request::DesignDialogAnswer {
            session,
            accept,
            text,
        } => {
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

    async fn input(
        &self,
        ctx: &DaemonContext,
        session: &archon_core::design::DesignSessionId,
        events: Vec<InputEvent>,
    ) -> Result<Reply, ArchonError> {
        let row = self.current(ctx, session).await?;
        if events.len() > MAX_INPUT_EVENTS {
            return Err(ArchonError::Invalid(format!(
                "at most {MAX_INPUT_EVENTS} input events a request"
            )));
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
        let browser = self.browser_for_input(ctx, session).await?;
        let mapped: Vec<adb::Input> = events.into_iter().map(input_of).collect();
        browser.input(&mapped).await.map_err(unavailable)?;
        Ok(Reply::DesignDone)
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
