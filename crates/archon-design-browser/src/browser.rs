//! The browser handle: launch, page control, guards, screencast and capture.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cdp::{CLOSED_EVENT, Cdp, CdpEvent, MAX_FRAME};
use crate::containment::Containment;
use crate::input::{Held, command};
use crate::launch::{
    SHELL_SCRIPT, classify_stderr, kill_group, launch_args, launch_env, process_start_time,
    write_profile_preferences,
};
use crate::origin::parse_url;
use crate::proxy::{Policy, Proxy};
use crate::relay::{FrameRelay, RelayShared};
use crate::sanitize::clean;
use crate::{
    BrowserError, BrowserEvent, ClipRect, Input, LaunchSpec, MAX_IMAGE_BYTES, Picture,
    PictureCapture, Viewport, jpeg_dims,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// One input request's dispatch, then letting go of what it left held: both
/// well inside the 60 s the web app waits, so its next batch never overlaps.
pub(crate) const INPUT_DEADLINE: Duration = Duration::from_secs(8);
const RELEASE_DEADLINE: Duration = Duration::from_secs(2);
/// Stream frames: four a second at rest, ten (the relay's most) while the
/// operator scrolls or types, so a scroll is followed and its end shown soon.
const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(250);
const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVE_AFTER_INPUT: Duration = Duration::from_millis(1500);

/// How long the polling loop waits before its next frame.
pub(crate) fn frame_interval(since_input: Option<Duration>) -> Duration {
    match since_input {
        Some(elapsed) if elapsed < ACTIVE_AFTER_INPUT => ACTIVE_FRAME_INTERVAL,
        _ => IDLE_FRAME_INTERVAL,
    }
}
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

struct LaunchGuard {
    pid: Option<u32>,
    profile: Option<PathBuf>,
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_group(pid);
        }
        if let Some(profile) = &self.profile
            && let Err(error) = fs::remove_dir_all(profile)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to remove browser profile {}: {error}",
                profile.display()
            );
        }
    }
}

struct OwnedSession {
    policy: Arc<Policy>,
    process: (u32, u64),
}

#[derive(Debug)]
struct State {
    url: String,
    main_frame: Option<String>,
    reqs: HashMap<String, String>,
    last_net: Instant,
    blocked: HashSet<String>,
    viewport: Viewport,
    poll: Option<JoinHandle<()>>,
    /// When input was last dispatched: frames come faster for a while after it.
    last_input: Option<Instant>,
    /// Keys, buttons and fingers sent down and not yet up.
    held: Held,
}

#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) cdp: Cdp,
    pub(crate) session: String,
    containment: Option<Arc<Containment>>,
    events: broadcast::Sender<BrowserEvent>,
    pub(crate) relay: Arc<RelayShared>,
    st: Mutex<State>,
    /// Held for a whole `capture`: Chrome draws a screenshot whose clip scale
    /// differs from one already in flight into a tiled copy (the page at 1x
    /// repeated across the 2x canvas), so two never overlap on this target.
    shot: tokio::sync::Mutex<()>,
}

/// A headless Chromium driven over CDP.
#[derive(Debug)]
pub struct Browser {
    pub(crate) sh: Arc<Shared>,
    child: Option<Child>,
    profile: Option<PathBuf>,
    pid: Option<u32>,
    started_at: Option<u64>,
    event_task: JoinHandle<()>,
    proxy: Option<Proxy>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.proxy.take();
        self.event_task.abort();
        self.sh.relay.close();
        self.sh.cdp.abort();
        if let (Some(pid), Some(_)) = (self.pid, self.child.as_ref()) {
            kill_group(pid);
        }
        if let Some(h) = self.sh.st.lock().unwrap().poll.take() {
            h.abort();
        }
        if let Some(profile) = self.profile.take()
            && let Err(error) = fs::remove_dir_all(&profile)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to remove browser profile {}: {error}",
                profile.display()
            );
        }
    }
}

fn b64(s: &str) -> Result<Vec<u8>, BrowserError> {
    B64.decode(s)
        .map_err(|e| BrowserError::Protocol(format!("bad base64: {e}")))
}

fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return None;
    }
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let host = if let Some(r) = auth.strip_prefix('[') {
        format!("[{}]", r.split(']').next()?)
    } else {
        auth.split(':').next()?.to_string()
    };
    (!host.is_empty()).then_some(host)
}

pub(crate) fn path_of(url: &str) -> String {
    match url.split_once("://") {
        Some((_, rest)) => {
            let p = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
            p.split(['?', '#']).next().unwrap_or("/").to_string()
        }
        None => url.to_string(),
    }
}

/// Encode to the picture budget: q70, then q40, then halve the scale.
pub(crate) async fn fit_to_budget<F, Fut>(
    mut shoot: F,
    dpr: f64,
) -> Result<(Vec<u8>, f64), BrowserError>
where
    F: FnMut(u8, f64) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, BrowserError>>,
{
    let mut scale = if dpr.is_finite() && dpr > 0.0 {
        dpr
    } else {
        1.0
    };
    let mut bytes = shoot(70, scale).await?;
    if bytes.len() <= MAX_IMAGE_BYTES {
        return Ok((bytes, scale));
    }
    bytes = shoot(40, scale).await?;
    while bytes.len() > MAX_IMAGE_BYTES {
        scale /= 2.0;
        if scale < 0.05 {
            return Err(BrowserError::Protocol(
                "picture cannot be made small enough".into(),
            ));
        }
        bytes = shoot(40, scale).await?;
    }
    Ok((bytes, scale))
}

impl Shared {
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.check_containment()?;
        let reply = self
            .cdp
            .call(Some(&self.session), method, params, CALL_TIMEOUT)
            .await;
        self.check_containment()?;
        reply
    }

    fn check_containment(&self) -> Result<(), BrowserError> {
        if let Some(guard) = &self.containment {
            guard.epoch()?;
        }
        Ok(())
    }

    pub(crate) async fn disclose<T>(
        &self,
        read: impl Future<Output = Result<T, BrowserError>>,
    ) -> Result<T, BrowserError> {
        self.verified_read(read).await.map(|(result, _)| result)
    }

    async fn verified_read<T>(
        &self,
        read: impl Future<Output = Result<T, BrowserError>>,
    ) -> Result<(T, Option<u64>), BrowserError> {
        let Some(guard) = &self.containment else {
            return Ok((read.await?, None));
        };
        let epoch = guard.epoch()?;
        let before = self.disclosure_tree(guard, epoch).await?;
        drop(guard.confirm(epoch)?);

        let result = read.await;
        let after = self.disclosure_tree(guard, epoch).await?;
        let _confirmed = guard.confirm(epoch)?;
        if before != after {
            return Err(BrowserError::DisclosureUnavailable);
        }
        Ok((result?, Some(epoch)))
    }

    async fn disclosure_tree(
        &self,
        guard: &Containment,
        epoch: u64,
    ) -> Result<Value, BrowserError> {
        let targets = self
            .cdp
            .call(None, "Target.getTargets", json!({}), CALL_TIMEOUT)
            .await;
        self.check_containment()?;
        guard.verify_targets(&targets?)?;
        let mut result = self.call("Page.getFrameTree", json!({})).await?;
        let tree = result
            .get_mut("frameTree")
            .map(Value::take)
            .unwrap_or(Value::Null);
        drop(guard.confirm(epoch)?);
        for frame_id in guard.check_frame_tree(&tree)? {
            let reply = self
                .call(
                    "Storage.getStorageKeyForFrame",
                    json!({"frameId": frame_id}),
                )
                .await;
            drop(guard.confirm(epoch)?);
            guard.verify_storage_origin(reply)?;
        }
        Ok(tree)
    }

    fn emit(&self, e: BrowserEvent) {
        let _ = self.events.send(e);
    }

    fn viewport(&self) -> Viewport {
        self.st.lock().unwrap().viewport
    }

    async fn shoot(
        &self,
        clip: ClipRect,
        quality: u8,
        scale: f64,
    ) -> Result<Vec<u8>, BrowserError> {
        let r = self
            .call(
                "Page.captureScreenshot",
                json!({
                    "format": "jpeg", "quality": quality, "captureBeyondViewport": false,
                    "clip": {"x": clip.x, "y": clip.y, "width": clip.w, "height": clip.h, "scale": scale},
                }),
            )
            .await?;
        b64(r["data"].as_str().unwrap_or_default())
    }

    /// Put the page back at scale 1. Under touch emulation Chrome zooms the
    /// visual viewport onto a screenshot's clip, and a double tap zooms too;
    /// the zoom survives a reload, so the stream stayed fixed on one element
    /// and later boxes and crops were mapped against the zoomed page. Best
    /// effort: an older browser without the method keeps working.
    async fn reset_zoom(&self) {
        let _ = self.call("Emulation.resetPageScaleFactor", json!({})).await;
    }

    pub(crate) async fn capture(
        &self,
        clip: Option<ClipRect>,
        dpr: f64,
    ) -> Result<Picture, BrowserError> {
        self.disclose(self.capture_picture(clip, dpr)).await
    }

    async fn capture_picture(
        &self,
        clip: Option<ClipRect>,
        dpr: f64,
    ) -> Result<Picture, BrowserError> {
        // One screenshot at a time: the stream's polling loop (scale 1) must
        // not race a final picture (scale 2). See `Shared::shot`.
        let _one_at_a_time = self.shot.lock().await;
        // Crops are taken on the unzoomed page the operator saw, and leave it
        // unzoomed for the stream. A full-view shot (each stream frame) zooms nothing.
        if clip.is_some() {
            self.reset_zoom().await;
        }
        let picture = match self.layout().await {
            Ok(layout) => self.capture_area(clip, &layout, dpr).await,
            Err(error) => Err(error),
        };
        if clip.is_some() {
            self.reset_zoom().await;
        }
        picture
    }

    /// Where the visible part of the page is and how big the page is, in CSS
    /// px. Guessing would shoot or hit-test the wrong place, so it fails instead.
    pub(crate) async fn layout(&self) -> Result<PageLayout, BrowserError> {
        let m = self.call("Page.getLayoutMetrics", json!({})).await?;
        PageLayout::from_metrics(&m).ok_or_else(|| {
            BrowserError::Protocol("Chrome did not say where the page is scrolled".into())
        })
    }

    async fn capture_area(
        &self,
        clip: Option<ClipRect>,
        layout: &PageLayout,
        dpr: f64,
    ) -> Result<Picture, BrowserError> {
        let vp = self.viewport();
        let (clip, dpr) = match clip {
            Some(seen) => (whole_pixels(layout.page_clip(seen)), dpr),
            None => layout.view_clip(vp, dpr),
        };
        if !(clip.w >= 1.0 && clip.h >= 1.0) {
            return Err(BrowserError::Protocol("empty capture area".into()));
        }
        let (bytes, scale) = fit_to_budget(|q, s| self.shoot(clip, q, s), dpr).await?;
        let (width, height) = jpeg_dims(&bytes).unwrap_or((
            (clip.w * scale).round() as u32,
            (clip.h * scale).round() as u32,
        ));
        let capture = PictureCapture {
            viewport_w: vp.width,
            viewport_h: vp.height,
            device_scale_factor: vp.dpr,
            mobile: vp.touch,
            x: clip.x as u32,
            y: clip.y as u32,
            w: clip.w as u32,
            h: clip.h as u32,
            screenshot_scale: scale,
        };
        assert!(
            capture.w > 0 && capture.h > 0,
            "a picture shows a nonempty area"
        );
        assert!(scale > 0.0, "the budget keeps a positive screenshot scale");
        Ok(Picture {
            mime: "image/jpeg",
            bytes,
            width,
            height,
            capture,
        })
    }

    fn start_polling(self: &Arc<Self>) {
        let sh = self.clone();
        let h = tokio::spawn(async move {
            loop {
                let since_input = sh.st.lock().unwrap().last_input.map(|at| at.elapsed());
                tokio::time::sleep(frame_interval(since_input)).await;
                match sh.poll_frame().await {
                    Ok(()) | Err(BrowserError::DisclosureUnavailable) => {}
                    Err(BrowserError::Closed | BrowserError::Containment(_)) => break,
                    Err(error) => tracing::debug!(%error, "preview frame was withheld"),
                }
            }
        });
        if let Some(old) = self.st.lock().unwrap().poll.replace(h) {
            old.abort();
        }
    }

    async fn poll_frame(&self) -> Result<(), BrowserError> {
        let (picture, epoch) = self.verified_read(self.capture_picture(None, 1.0)).await?;
        let _confirmed = match (&self.containment, epoch) {
            (Some(guard), Some(epoch)) => Some(guard.confirm(epoch)?),
            (None, None) => None,
            _ => unreachable!("verified read returned a mismatched document epoch"),
        };
        self.relay
            .push(picture.bytes, picture.width, picture.height);
        Ok(())
    }

    /// Returns true when the loop should stop.
    fn handle(self: &Arc<Self>, ev: CdpEvent) -> bool {
        if ev.method == CLOSED_EVENT {
            self.relay.close();
            if self.check_containment().is_ok() {
                self.emit(BrowserEvent::Exited);
            }
            return true;
        }
        if ev.session_id.as_deref() != Some(self.session.as_str()) {
            return false;
        }
        let params = &ev.params;
        match ev.method.as_str() {
            "Inspector.targetCrashed" => self.emit(BrowserEvent::Crashed),
            "Page.javascriptDialogOpening" => self.emit_dialog(params),
            "Page.fileChooserOpened" => self.emit(BrowserEvent::FileChooserRefused),
            "Page.frameNavigated" if params["frame"]["parentId"].is_null() => {
                let frame = &params["frame"];
                self.record_document(frame["id"].as_str(), frame["url"].as_str());
            }
            "Page.navigatedWithinDocument" => {
                let frame = params["frameId"].as_str();
                if self.st.lock().unwrap().main_frame.as_deref() == frame {
                    self.record_document(frame, params["url"].as_str());
                }
            }
            "Page.loadEventFired" => {
                let url = clean(&self.st.lock().unwrap().url);
                self.emit(BrowserEvent::Loaded { url });
            }
            "Network.requestWillBeSent" => self.on_request(params),
            "Network.loadingFinished" => {
                let mut state = self.st.lock().unwrap();
                state.last_net = Instant::now();
                if let Some(id) = params["requestId"].as_str() {
                    state.reqs.remove(id);
                }
            }
            "Network.loadingFailed" => self.on_failed(params),
            _ => {}
        }
        false
    }

    fn emit_dialog(&self, params: &Value) {
        let kind = match params["type"].as_str() {
            Some("confirm") => "confirm",
            Some("prompt") => "prompt",
            Some("beforeunload") => "beforeunload",
            _ => "alert",
        }
        .into();
        let (message, default_prompt) = if self.containment.is_some() {
            ("The preview opened a dialog.".into(), None)
        } else {
            let message = clean(params["message"].as_str().unwrap_or_default());
            let prompt = params["defaultPrompt"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(clean);
            (message, prompt)
        };
        self.emit(BrowserEvent::Dialog {
            kind,
            message,
            default_prompt,
        });
    }

    fn on_request(&self, params: &Value) {
        let (Some(id), Some(url)) = (
            params["requestId"].as_str(),
            params["request"]["url"].as_str(),
        ) else {
            return;
        };
        let mut state = self.st.lock().unwrap();
        state.last_net = Instant::now();
        if state.reqs.len() < 4096 {
            state.reqs.insert(id.to_owned(), url.to_owned());
        }
    }

    fn record_document(&self, frame: Option<&str>, url: Option<&str>) {
        if let Some(url) = url {
            let mut state = self.st.lock().unwrap();
            state.main_frame = frame.map(str::to_owned);
            state.url = url.to_owned();
        }
    }

    async fn guard_document(&self, event: &CdpEvent) -> Result<(), BrowserError> {
        let Some(guard) = &self.containment else {
            return Ok(());
        };
        let params = &event.params;
        let id = params["requestId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| BrowserError::Protocol("document request has no request ID".into()))?;
        let url = params["request"]["url"]
            .as_str()
            .ok_or_else(|| BrowserError::Protocol("document request has no URL".into()))?;
        let (method, args) = if guard.policy.origin.allows_navigation(url) {
            ("Fetch.continueRequest", json!({"requestId": id}))
        } else {
            self.emit_blocked(url);
            (
                "Fetch.failRequest",
                json!({"requestId": id, "errorReason": "BlockedByClient"}),
            )
        };
        self.call(method, args).await?;
        Ok(())
    }

    fn stop_for_policy(&self, reason: &'static str) {
        if let Some(guard) = &self.containment {
            guard.stop(reason);
        }
        self.cdp.abort();
    }

    fn emit_blocked(&self, url: &str) {
        if let Some(host) = host_of(url) {
            let host = clean(&host);
            let mut st = self.st.lock().unwrap();
            if st.blocked.len() < 256 && st.blocked.insert(host.clone()) {
                drop(st);
                self.emit(BrowserEvent::BlockedHost { host });
            }
        }
    }

    fn on_failed(&self, p: &Value) {
        let err = p["errorText"].as_str().unwrap_or_default();
        let url = {
            let mut st = self.st.lock().unwrap();
            st.last_net = Instant::now();
            p["requestId"]
                .as_str()
                .and_then(|id| st.reqs.remove(id))
                .unwrap_or_default()
        };
        let proxy_failure = err.contains("ERR_PROXY_CONNECTION_FAILED")
            || err.contains("ERR_TUNNEL_CONNECTION_FAILED");
        if proxy_failure
            || self
                .containment
                .as_ref()
                .is_some_and(|guard| !guard.policy.origin.allows_network_url(&url))
        {
            self.emit_blocked(&url);
        }
        if p["type"] == "Document" && p["canceled"] != true && host_of(&url).is_some() {
            self.emit(BrowserEvent::NavigationFailed {
                url: clean(&url),
                error: clean(err),
            });
        }
    }
}

async fn handle_events(
    sh: Arc<Shared>,
    mut events: broadcast::Receiver<CdpEvent>,
    mut documents: broadcast::Receiver<CdpEvent>,
) {
    loop {
        let event = tokio::select! {
            biased;
            result = documents.recv() => match result {
                Err(broadcast::error::RecvError::Lagged(_)) if sh.containment.is_some() => {
                    sh.stop_for_policy("lost document events");
                    return;
                }
                other => other,
            },
            result = events.recv() => result,
        };
        match event {
            Ok(ev)
                if ev.method == "Fetch.requestPaused"
                    && ev.session_id.as_deref() == Some(&sh.session) =>
            {
                if let Err(error) = sh.guard_document(&ev).await {
                    tracing::debug!(%error, "could not enforce the preview document request policy");
                    sh.stop_for_policy(
                        "Chromium could not enforce the preview document request policy",
                    );
                    return;
                }
            }
            Ok(ev) => {
                if sh.handle(ev) {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn read_stderr(mut stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut t = tail.lock().unwrap();
        t.push_str(&String::from_utf8_lossy(&buf[..n]));
        if t.len() > 16 * 1024 {
            let cut = t.len() - 8 * 1024;
            let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
            t.drain(..cut);
        }
    }
}

fn launch_failure(tail: &Mutex<String>, why: String) -> BrowserError {
    let text = tail.lock().unwrap();
    classify_stderr(&text).unwrap_or_else(|| {
        let offset = text
            .char_indices()
            .rev()
            .nth(499)
            .map_or(0, |(index, _)| index);
        BrowserError::Launch(format!("{why}: {}", text[offset..].trim()))
    })
}

async fn attach_page(
    cdp: &Cdp,
    rx: &mut broadcast::Receiver<CdpEvent>,
) -> Result<(String, String), BrowserError> {
    cdp.call(
        None,
        "Target.setDiscoverTargets",
        json!({"discover": true}),
        CALL_TIMEOUT,
    )
    .await?;
    let target_id = tokio::time::timeout(CALL_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(e)
                    if e.method == "Target.targetCreated"
                        && e.params["targetInfo"]["type"] == "page" =>
                {
                    return Ok(e.params["targetInfo"]["targetId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string());
                }
                Ok(e) if e.method == CLOSED_EVENT => return Err(BrowserError::Closed),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return Err(BrowserError::Closed),
            }
        }
    })
    .await
    .map_err(|_| BrowserError::Timeout("no page target".into()))??;
    let r = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            CALL_TIMEOUT,
        )
        .await?;
    let session = r["sessionId"]
        .as_str()
        .ok_or_else(|| BrowserError::Protocol("attachToTarget gave no sessionId".into()))?
        .to_string();
    Ok((target_id, session))
}

async fn configure_page(cdp: &Cdp, session: &str, confined: bool) -> Result<(), BrowserError> {
    cdp.call(
        None,
        "Browser.setDownloadBehavior",
        json!({"behavior": "deny"}),
        CALL_TIMEOUT,
    )
    .await?;
    for m in [
        "Page.enable",
        "DOM.enable",
        "CSS.enable",
        "Network.enable",
        "Runtime.enable",
    ] {
        cdp.call(Some(session), m, json!({}), CALL_TIMEOUT).await?;
    }
    cdp.call(
        Some(session),
        "Page.setInterceptFileChooserDialog",
        json!({"enabled": true}),
        CALL_TIMEOUT,
    )
    .await?;
    if confined {
        cdp.call(
            None,
            "Browser.grantPermissions",
            json!({"permissions": []}),
            CALL_TIMEOUT,
        )
        .await?;
        cdp.call(
            Some(session),
            "Fetch.enable",
            json!({"patterns": [{"resourceType": "Document"}]}),
            CALL_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

async fn configure_target(
    cdp: &Cdp,
    target: &str,
    session: &str,
    owned: Option<OwnedSession>,
    events: &broadcast::Sender<BrowserEvent>,
    relay: &Arc<RelayShared>,
) -> Result<Option<Arc<Containment>>, BrowserError> {
    let containment = owned.map(|owned| {
        Arc::new(Containment {
            target: target.to_owned(),
            session: session.to_owned(),
            policy: owned.policy,
            process: owned.process,
            events: events.clone(),
            relay: relay.clone(),
            state: Mutex::default(),
        })
    });
    if let Some(guard) = &containment {
        cdp.install_containment(guard.clone());
        let targets = cdp
            .call(None, "Target.getTargets", json!({}), CALL_TIMEOUT)
            .await?;
        guard.verify_targets(&targets)?;
    }
    configure_page(cdp, session, containment.is_some()).await?;
    Ok(containment)
}

impl Browser {
    /// Start Chromium and attach to its page.
    pub async fn launch(spec: LaunchSpec) -> Result<Browser, BrowserError> {
        Self::launch_recorded(spec, |_, _| std::future::ready(Ok(()))).await
    }

    /// Persist the process identity before releasing the child to execute Chromium.
    pub async fn launch_recorded<F>(
        spec: LaunchSpec,
        record: impl FnOnce(u32, u64) -> F,
    ) -> Result<Browser, BrowserError>
    where
        F: Future<Output = Result<(), BrowserError>>,
    {
        use std::os::unix::fs::DirBuilderExt;
        let io = |e: std::io::Error| BrowserError::Launch(e.to_string());
        let policy = Arc::new(Policy::new(&spec.preview_url, spec.upstream_guard.clone())?);
        let proxy = Proxy::start(policy.clone()).await.map_err(io)?;
        if let Some(parent) = spec.profile_dir.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&spec.profile_dir)
            .map_err(io)?;
        Self::spawn(&spec, policy, proxy, record).await
    }

    async fn spawn<F>(
        spec: &LaunchSpec,
        policy: Arc<Policy>,
        proxy: Proxy,
        record: impl FnOnce(u32, u64) -> F,
    ) -> Result<Browser, BrowserError>
    where
        F: Future<Output = Result<(), BrowserError>>,
    {
        let mut guard = LaunchGuard {
            pid: None,
            profile: Some(spec.profile_dir.clone()),
        };
        write_profile_preferences(&spec.profile_dir).map_err(|error| {
            BrowserError::Launch(format!("create private browser preferences: {error}"))
        })?;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(SHELL_SCRIPT)
            .arg(&spec.binary)
            .args(launch_args(spec, proxy.addr))
            .env_clear()
            .envs(launch_env(spec))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| BrowserError::Launch(e.to_string()))?;
        let pid = child.id();
        guard.pid = pid;
        let started_at = pid.and_then(process_start_time);
        let process = pid.zip(started_at).ok_or_else(|| {
            BrowserError::Launch("browser process identity is unavailable".into())
        })?;
        record(process.0, process.1).await?;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| BrowserError::Launch("missing launch pipe".into()))?;
        tokio::io::AsyncWriteExt::write_all(stdin, b"START\n")
            .await
            .map_err(|e| BrowserError::Launch(format!("release recorded browser: {e}")))?;
        let mut b =
            Self::attach_child(&mut child, spec.viewport, OwnedSession { policy, process }).await?;
        b.proxy = Some(proxy);
        b.child = Some(child);
        b.profile = Some(spec.profile_dir.clone());
        b.pid = pid;
        b.started_at = started_at;
        guard.pid = None;
        guard.profile = None;
        Ok(b)
    }

    async fn attach_child(
        child: &mut Child,
        viewport: Viewport,
        owned: OwnedSession,
    ) -> Result<Browser, BrowserError> {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(BrowserError::Launch("child pipes missing".into()));
        };
        let tail = Arc::new(Mutex::new(String::new()));
        let stderr_task = tokio::spawn(read_stderr(stderr, tail.clone()));
        let connected = tokio::select! {
            result = tokio::time::timeout(
                LAUNCH_TIMEOUT,
                Self::attach(stdout, stdin, Some(viewport), Some(owned)),
            ) => result,
            status = child.wait() => {
                let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
                return Err(launch_failure(&tail, format!("browser exited early ({status:?})")));
            }
        };
        match connected {
            Ok(Ok(browser)) => Ok(browser),
            Ok(Err(error @ BrowserError::Containment(_))) => Err(error),
            Ok(Err(error)) => {
                let _ = tokio::time::timeout(Duration::from_millis(300), stderr_task).await;
                Err(launch_failure(&tail, error.to_string()))
            }
            Err(_) => Err(launch_failure(
                &tail,
                "browser did not answer in time".into(),
            )),
        }
    }

    /// Drive a browser over an existing CDP pipe (no process is owned).
    #[cfg(test)]
    pub(crate) async fn connect<R, W>(reader: R, writer: W) -> Result<Browser, BrowserError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::attach(reader, writer, None, None).await
    }

    async fn attach<R, W>(
        reader: R,
        writer: W,
        viewport: Option<Viewport>,
        owned: Option<OwnedSession>,
    ) -> Result<Browser, BrowserError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let cdp = Cdp::new(reader, writer, MAX_FRAME);
        let mut rx = cdp.subscribe();
        let documents = cdp.subscribe_documents();
        let (target_id, session) = attach_page(&cdp, &mut rx).await?;
        let (events, _) = broadcast::channel(256);
        let relay = Arc::new(RelayShared::default());
        let containment =
            configure_target(&cdp, &target_id, &session, owned, &events, &relay).await?;
        let vp = viewport.unwrap_or(Viewport {
            width: 1280,
            height: 800,
            touch: false,
            dpr: 1.0,
        });
        let sh = Arc::new(Shared {
            cdp,
            session,
            containment,
            events,
            relay,
            shot: tokio::sync::Mutex::new(()),
            st: Mutex::new(State {
                url: String::new(),
                main_frame: None,
                reqs: HashMap::new(),
                last_net: Instant::now(),
                blocked: HashSet::new(),
                viewport: vp,
                poll: None,
                last_input: None,
                held: Held::default(),
            }),
        });
        let event_task = tokio::spawn(handle_events(sh.clone(), rx, documents));
        let b = Browser {
            sh,
            child: None,
            profile: None,
            pid: None,
            started_at: None,
            event_task,
            proxy: None,
        };
        if let Some(v) = viewport {
            b.set_viewport(v).await?;
        }
        Ok(b)
    }

    /// Process id of the child, if this browser owns one.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Start time of the child (for [`crate::kill_if_matches`]).
    pub fn started_at(&self) -> Option<u64> {
        self.started_at
    }

    /// Navigate the page.
    pub async fn navigate(&self, url: &str) -> Result<(), BrowserError> {
        self.sh.check_containment()?;
        if self
            .sh
            .containment
            .as_ref()
            .is_some_and(|guard| !guard.policy.origin.allows_navigation(url))
        {
            return Err(BrowserError::Navigation {
                url: clean(url),
                error: "URL is outside the registered preview origin".into(),
            });
        }
        let r = self.sh.call("Page.navigate", json!({"url": url})).await?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            let (url, error) = (clean(url), clean(e));
            self.sh.emit(BrowserEvent::NavigationFailed {
                url: url.clone(),
                error: error.clone(),
            });
            return Err(BrowserError::Navigation { url, error });
        }
        Ok(())
    }

    /// Check whether the verified preview server responds to HTTP.
    pub async fn probe(&self, url: &str) -> bool {
        if self.sh.check_containment().is_err() {
            return false;
        }
        match &self.sh.containment {
            Some(guard) => guard.policy.probe(url).await,
            None => false,
        }
    }

    /// Reload the page.
    pub async fn reload(&self) -> Result<(), BrowserError> {
        self.sh.reset_zoom().await;
        self.sh.call("Page.reload", json!({})).await.map(|_| ())
    }

    /// Resize and set touch emulation.
    pub async fn set_viewport(&self, v: Viewport) -> Result<(), BrowserError> {
        self.sh
            .call(
                "Emulation.setDeviceMetricsOverride",
                json!({"width": v.width, "height": v.height, "deviceScaleFactor": v.dpr, "mobile": v.touch}),
            )
            .await?;
        self.sh
            .call(
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": v.touch}),
            )
            .await?;
        self.sh.reset_zoom().await;
        self.sh.st.lock().unwrap().viewport = v;
        Ok(())
    }

    /// Dispatch input events in order, within [`INPUT_DEADLINE`]. Past it the
    /// rest is dropped and whatever is held down is let go, so nothing sticks.
    pub async fn input(&self, events: &[Input]) -> Result<(), BrowserError> {
        self.sh.st.lock().unwrap().last_input = Some(Instant::now());
        let dispatch = async {
            for e in events {
                self.sh.st.lock().unwrap().held.note(e);
                let (method, params) = command(e);
                self.sh.call(method, params).await?;
            }
            Ok(())
        };
        let Ok(done) = tokio::time::timeout(INPUT_DEADLINE, dispatch).await else {
            let released = self.release_held().await;
            let held = if released.is_ok() {
                ""
            } else {
                "; a held key or button may still be down"
            };
            return Err(BrowserError::Timeout(format!(
                "input took over {} s; some of it may not have reached the app{held}",
                INPUT_DEADLINE.as_secs()
            )));
        };
        done
    }

    /// Let go of every key, button and finger still held. Chrome runs a target's
    /// commands in order, so these follow any press it has yet to act on.
    async fn release_held(&self) -> Result<(), BrowserError> {
        let releases = std::mem::take(&mut self.sh.st.lock().unwrap().held).releases();
        let release = async {
            for e in &releases {
                let (method, params) = command(e);
                self.sh.call(method, params).await?;
            }
            Ok::<(), BrowserError>(())
        };
        let result = tokio::time::timeout(RELEASE_DEADLINE, release)
            .await
            .unwrap_or_else(|_| Err(BrowserError::Timeout("releasing held input".into())));
        if let Err(error) = &result {
            tracing::warn!(%error, count = releases.len(), "held preview input was not released");
        }
        result
    }

    /// Start delivering verified viewport captures at up to four frames per second.
    pub async fn start_screencast(&self) -> Result<FrameRelay, BrowserError> {
        self.sh.check_containment()?;
        self.sh.start_polling();
        Ok(FrameRelay {
            shared: self.sh.relay.clone(),
        })
    }

    /// Stop polling for viewport captures.
    pub async fn stop_screencast(&self) -> Result<(), BrowserError> {
        if let Some(task) = self.sh.st.lock().unwrap().poll.take() {
            task.abort();
        }
        Ok(())
    }

    /// Chrome's own picture of what the page shows, with no clip, at the
    /// stream's quality: what a stream frame must match once the page rests.
    pub async fn viewport_screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        let sh = &self.sh;
        let shot = async {
            let _one_at_a_time = sh.shot.lock().await;
            let r = sh
                .call(
                    "Page.captureScreenshot",
                    json!({"format": "jpeg", "quality": 70, "captureBeyondViewport": false}),
                )
                .await?;
            b64(r["data"].as_str().unwrap_or_default())
        };
        sh.disclose(shot).await
    }

    /// A picture of the viewport or `clip`, at most 560 KiB.
    pub async fn capture(&self, clip: Option<ClipRect>, dpr: f64) -> Result<Picture, BrowserError> {
        self.sh.capture(clip, dpr).await
    }

    /// Path of the current main-frame URL (no query, no fragment).
    pub async fn current_path(&self) -> Result<String, BrowserError> {
        Ok(clean(&path_of(&self.read_location().await?)))
    }

    /// Current same-origin path, query and fragment for restoring a viewport.
    pub async fn current_location(&self) -> Result<String, BrowserError> {
        let url = self.read_location().await?;
        let uri = parse_url(&url)
            .filter(|uri| uri.authority().is_some() && uri.scheme_str() == Some("http"))
            .ok_or(BrowserError::DisclosureUnavailable)?;
        let mut location = uri.path().to_owned();
        if let Some(query) = uri.query() {
            location.push('?');
            location.push_str(query);
        }
        if let Some((_, fragment)) = url.split_once('#') {
            location.push('#');
            location.push_str(fragment);
        }
        debug_assert!(location.starts_with('/'));
        Ok(location)
    }

    async fn read_location(&self) -> Result<String, BrowserError> {
        let sh = &self.sh;
        let Some(guard) = &sh.containment else {
            return Ok(sh.st.lock().unwrap().url.clone());
        };
        let epoch = guard.epoch()?;
        let tree = sh.disclosure_tree(guard, epoch).await?;
        let _confirmed = guard.confirm(epoch)?;
        let frame = &tree["frame"];
        let url = frame["url"]
            .as_str()
            .expect("validated main frame has a URL");
        match frame.get("urlFragment") {
            None => Ok(url.to_owned()),
            Some(Value::String(fragment)) if fragment.starts_with('#') => {
                Ok(format!("{url}{fragment}"))
            }
            _ => Err(BrowserError::DisclosureUnavailable),
        }
    }

    /// Wait until no request has been in flight for `quiet`; false if `max` passes first.
    pub async fn wait_network_idle(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + max;
        loop {
            self.sh.check_containment()?;
            let (idle, since) = {
                let st = self.sh.st.lock().unwrap();
                (st.reqs.is_empty(), st.last_net.elapsed())
            };
            if idle && since >= quiet {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Whether a frame newer than `since_seq` arrives within `max`.
    pub async fn frame_changed_within(&self, since_seq: u64, max: Duration) -> bool {
        self.sh.relay.wait_seq_after(since_seq, max).await
    }

    /// Answer the open JavaScript dialog.
    pub async fn answer_dialog(
        &self,
        accept: bool,
        text: Option<String>,
    ) -> Result<(), BrowserError> {
        let mut p = json!({"accept": accept});
        if let Some(t) = text {
            p["promptText"] = json!(t);
        }
        self.sh
            .call("Page.handleJavaScriptDialog", p)
            .await
            .map(|_| ())
    }

    /// The first containment failure, if the browser was stopped for one.
    pub fn containment_reason(&self) -> Option<String> {
        self.sh.containment.as_ref()?.reason().map(String::from)
    }

    /// Subscribe to browser events.
    pub fn events(&self) -> broadcast::Receiver<BrowserEvent> {
        self.sh.events.subscribe()
    }

    /// Resident memory of the browser's process group, in bytes.
    pub fn rss_bytes(&self) -> Option<u64> {
        let pid = self.pid?;
        let mut total = 0u64;
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let name = e.file_name();
            let Some(p) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/{p}/stat")) else {
                continue;
            };
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            // Fields after comm: state ppid pgrp ...
            if rest
                .split_whitespace()
                .nth(2)
                .and_then(|g| g.parse::<u32>().ok())
                != Some(pid)
            {
                continue;
            }
            if let Ok(status) = fs::read_to_string(format!("/proc/{p}/status")) {
                let kb = status
                    .lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                    .unwrap_or(0);
                total += kb * 1024;
            }
        }
        Some(total)
    }

    /// Kill the process group, wait for it and delete the profile.
    pub async fn close(mut self) -> Result<(), BrowserError> {
        self.proxy.take();
        self.sh.relay.close();
        if let Some(h) = self.sh.st.lock().unwrap().poll.take() {
            h.abort();
        }
        if let (Some(pid), Some(_)) = (self.pid, self.child.as_ref()) {
            kill_group(pid);
        }
        if let Some(mut c) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
        }
        self.event_task.abort();
        self.sh.cdp.abort();
        if let Some(p) = self.profile.clone() {
            let mut last = None;
            for _ in 0..20 {
                match fs::remove_dir_all(&p) {
                    Ok(()) => {
                        self.profile = None;
                        return Ok(());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.profile = None;
                        return Ok(());
                    }
                    Err(e) => {
                        last = Some(e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            if let Some(e) = last {
                return Err(BrowserError::Launch(format!("profile removal failed: {e}")));
            }
        }
        Ok(())
    }
}

/// The page as `Page.getLayoutMetrics` describes it. Every clip Archon holds
/// (the whole view, an element box, a drawn region) is relative to what is
/// visible, but `Page.captureScreenshot` clips in page coordinates: a scrolled
/// page shot at (0, 0) showed its top, so a sticky header was drawn far down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageLayout {
    /// The visible area's top-left corner in page CSS px.
    pub(crate) scroll: (f64, f64),
    /// The page's full size, when the browser says.
    pub(crate) size: Option<(f64, f64)>,
    /// The page scale: below 1 where a phone shows a page wider than itself
    /// zoomed out, above 1 after a pinch or double tap.
    pub(crate) zoom: f64,
}

impl Default for PageLayout {
    fn default() -> Self {
        Self {
            scroll: (0.0, 0.0),
            size: None,
            zoom: 1.0,
        }
    }
}

impl PageLayout {
    /// None when the visible area's place or zoom is missing or not a number.
    pub(crate) fn from_metrics(m: &Value) -> Option<Self> {
        let visual = &m["cssVisualViewport"];
        // An overscrolled page can report a small negative offset; it shows the top.
        let offset = |key: &str| {
            visual[key]
                .as_f64()
                .filter(|v| v.is_finite())
                .map(|v| v.max(0.0))
        };
        let size = &m["cssContentSize"];
        let size = match (size["width"].as_f64(), size["height"].as_f64()) {
            (Some(w), Some(h)) if w >= 1.0 && h >= 1.0 => Some((w, h)),
            _ => None,
        };
        let zoom = match visual.get("scale") {
            None => 1.0,
            Some(scale) => scale.as_f64().filter(|z| z.is_finite() && *z > 0.0)?,
        };
        Some(Self {
            scroll: (offset("pageX")?, offset("pageY")?),
            size,
            zoom,
        })
    }

    /// The whole view as Chrome shows it, and the screenshot scale that draws
    /// it at `vp`'s size times `dpr`: zoomed, it covers more or less of the page.
    pub(crate) fn view_clip(&self, vp: Viewport, dpr: f64) -> (ClipRect, f64) {
        assert!(vp.width > 0 && vp.height > 0, "a viewport has a size");
        let view = self.on_page(ClipRect {
            x: 0.0,
            y: 0.0,
            w: f64::from(vp.width) / self.zoom,
            h: f64::from(vp.height) / self.zoom,
        });
        if self.zoom == 1.0 {
            return (whole_pixels(view), dpr);
        }
        // Snapping a zoomed view to whole page pixels would change its size on screen.
        (view, dpr * self.zoom)
    }

    /// `seen`, a rectangle of the visible area, in page coordinates.
    pub(crate) fn on_page(&self, seen: ClipRect) -> ClipRect {
        assert!(
            seen.x.is_finite() && seen.y.is_finite() && seen.w >= 0.0 && seen.h >= 0.0,
            "a capture area is a finite, nonnegative rectangle: {seen:?}"
        );
        let page = ClipRect {
            x: seen.x + self.scroll.0,
            y: seen.y + self.scroll.1,
            ..seen
        };
        debug_assert!(page.x >= seen.x && page.y >= seen.y);
        page
    }

    /// A point on the frame (what is visible, zoom included) on the page.
    pub(crate) fn page_point(&self, x: f64, y: f64) -> (f64, f64) {
        assert!(x.is_finite() && y.is_finite(), "a frame point is finite");
        let point = (self.scroll.0 + x / self.zoom, self.scroll.1 + y / self.zoom);
        debug_assert!(point.0.is_finite() && point.1.is_finite());
        point
    }

    /// A crop of the visible area as a page clip, cut to the page's edges.
    pub(crate) fn page_clip(&self, seen: ClipRect) -> ClipRect {
        let page = self.on_page(seen);
        let clipped = match self.size {
            Some((w, h)) => clamp_to_page(page, w, h),
            None => page,
        };
        debug_assert!(clipped.w <= seen.w && clipped.h <= seen.h);
        clipped
    }
}

/// `clip` cut to the page (`width` × `height` CSS px): a margin around an
/// element at the page's edge must not add an empty strip beyond it.
pub(crate) fn clamp_to_page(clip: ClipRect, width: f64, height: f64) -> ClipRect {
    let x = clip.x.clamp(0.0, width);
    let y = clip.y.clamp(0.0, height);
    ClipRect {
        x,
        y,
        w: ((clip.x + clip.w).min(width) - x).max(0.0),
        h: ((clip.y + clip.h).min(height) - y).max(0.0),
    }
}

/// `clip` grown outward to whole CSS px, so the area a picture records is
/// exactly the area it shows and a build can be cut to the same one.
pub(crate) fn whole_pixels(clip: ClipRect) -> ClipRect {
    let (x, y) = (clip.x.max(0.0).floor(), clip.y.max(0.0).floor());
    let snapped = ClipRect {
        x,
        y,
        w: ((clip.x + clip.w).ceil() - x).max(0.0),
        h: ((clip.y + clip.h).ceil() - y).max(0.0),
    };
    debug_assert!(snapped.w.fract() == 0.0 && snapped.h.fract() == 0.0);
    snapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crop_is_grown_to_whole_css_pixels() {
        let crop = whole_pixels(ClipRect {
            x: 10.4,
            y: 0.6,
            w: 100.2,
            h: 20.0,
        });
        assert_eq!(
            crop,
            ClipRect {
                x: 10.0,
                y: 0.0,
                w: 101.0,
                h: 21.0
            }
        );
        let whole = ClipRect {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        };
        assert_eq!(whole_pixels(whole), whole);
    }

    /// LOW-crop: a full-width header with a 24 px margin was 2928 px wide
    /// at 2x on a 1440 px page; cut to the page it is 2880.
    #[test]
    fn a_crop_never_reaches_past_the_page() {
        let header = ClipRect {
            x: 0.0,
            y: 0.0,
            w: 1464.0,
            h: 88.0,
        };
        let cut = clamp_to_page(header, 1440.0, 3000.0);
        assert_eq!((cut.x, cut.w, cut.h), (0.0, 1440.0, 88.0));
        let inside = ClipRect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
        };
        assert_eq!(
            clamp_to_page(inside, 1440.0, 900.0),
            inside,
            "inside: unchanged"
        );
        let low = clamp_to_page(
            ClipRect {
                x: 1400.0,
                y: 880.0,
                w: 100.0,
                h: 100.0,
            },
            1440.0,
            900.0,
        );
        assert_eq!((low.w, low.h), (40.0, 20.0));
    }

    #[test]
    fn url_pieces() {
        assert_eq!(
            path_of("http://127.0.0.1:5173/settings/a?x=1#h"),
            "/settings/a"
        );
        assert_eq!(path_of("http://localhost:3000"), "/");
        assert_eq!(
            host_of("https://user@evil.example:8443/x"),
            Some("evil.example".into())
        );
        assert_eq!(host_of("http://[::1]:80/"), Some("[::1]".into()));
        assert_eq!(host_of("data:text/html,http://fixture/"), None);
        assert_eq!(host_of("blob:http://localhost:80/fixture"), None);
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::testutil::fake_browser;

    fn jpeg_of(len: usize) -> String {
        B64.encode(vec![7u8; len])
    }

    // T-U10
    /// F3: a clipped capture under touch emulation left the phone preview
    /// zoomed onto the clip. Every capture, preset change and reload puts the
    /// page back at scale 1, before the shot and after it.
    #[tokio::test]
    async fn captures_presets_and_reloads_reset_the_page_zoom() {
        let (b, log) = fake_browser(|m, _| {
            (m == "Page.captureScreenshot").then(|| json!({"data": jpeg_of(64)}))
        })
        .await;
        let clip = ClipRect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
        };
        b.capture(Some(clip), 2.0).await.unwrap();
        let methods = |log: &std::sync::Mutex<Vec<(String, Value)>>| -> Vec<String> {
            log.lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .filter(|m| {
                    m == "Page.captureScreenshot"
                        || m == "Emulation.resetPageScaleFactor"
                        || m == "Page.reload"
                })
                .collect()
        };
        assert_eq!(
            methods(&log),
            [
                "Emulation.resetPageScaleFactor",
                "Page.captureScreenshot",
                "Emulation.resetPageScaleFactor"
            ]
        );
        log.lock().unwrap().clear();
        b.capture(None, 1.0).await.unwrap();
        assert_eq!(
            methods(&log),
            ["Page.captureScreenshot"],
            "a full-view shot needs no reset"
        );
        log.lock().unwrap().clear();
        b.reload().await.unwrap();
        assert_eq!(
            methods(&log),
            ["Emulation.resetPageScaleFactor", "Page.reload"]
        );
        log.lock().unwrap().clear();
        b.set_viewport(Viewport {
            width: 390,
            height: 844,
            dpr: 2.0,
            touch: true,
        })
        .await
        .unwrap();
        assert_eq!(methods(&log), ["Emulation.resetPageScaleFactor"]);
    }

    /// The operator's screenshot: a scrolled page's frame showed the top of
    /// the document, its sticky header a third of the way down. Every shot is
    /// clipped where the visible area is on the page.
    #[tokio::test]
    async fn a_scrolled_page_is_shot_where_it_is_scrolled_to() {
        let (b, log) = fake_browser(|m, _| match m {
            "Page.captureScreenshot" => Some(json!({"data": jpeg_of(64)})),
            "Page.getLayoutMetrics" => Some(json!({
                "cssVisualViewport": {"pageX": 30.0, "pageY": 450.0},
                "cssContentSize": {"width": 1000.0, "height": 1000.0},
            })),
            _ => None,
        })
        .await;
        let frame = b.capture(None, 1.0).await.unwrap();
        let crop = ClipRect {
            x: 10.0,
            y: 500.0,
            w: 100.0,
            h: 100.0,
        };
        let cropped = b.capture(Some(crop), 2.0).await.unwrap();
        let clips: Vec<Value> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "Page.captureScreenshot")
            .map(|(_, p)| p["clip"].clone())
            .collect();
        assert_eq!(clips.len(), 2, "{clips:?}");
        let vp = b.sh.viewport();
        assert_eq!(
            (clips[0]["x"].as_f64(), clips[0]["y"].as_f64()),
            (Some(30.0), Some(450.0))
        );
        assert_eq!(clips[0]["width"].as_f64(), Some(f64::from(vp.width)));
        assert_eq!((frame.capture.x, frame.capture.y), (30, 450));
        // The crop starts 950 px down the 1000 px page: it is cut at the bottom.
        assert_eq!(
            (clips[1]["y"].as_f64(), clips[1]["height"].as_f64()),
            (Some(950.0), Some(50.0))
        );
        assert_eq!((cropped.capture.x, cropped.capture.h), (40, 50));
    }

    /// Comment-mode picks hit nothing on a scrolled page: Chrome's hit test
    /// takes page coordinates and was given the frame's.
    #[tokio::test]
    async fn a_pick_on_a_scrolled_page_asks_for_the_page_point() {
        let (b, log) = fake_browser(|m, _| match m {
            "Page.getLayoutMetrics" => Some(json!({
                "cssVisualViewport": {"pageX": 0.0, "pageY": 450.0, "scale": 2.0},
            })),
            _ => None,
        })
        .await;
        let refused = b.resolve(100.0, 300.0, 0).await;
        assert!(refused.is_err(), "the fake has no node: {refused:?}");
        let asked = log
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == "DOM.getNodeForLocation")
            .map(|(_, p)| p.clone())
            .expect("a hit test");
        assert_eq!(
            (asked["x"].as_i64(), asked["y"].as_i64()),
            (Some(50), Some(600))
        );
    }

    #[test]
    fn a_zoomed_view_is_shot_whole_at_its_page_scale() {
        let layout = PageLayout {
            scroll: (0.0, 100.0),
            size: None,
            zoom: 0.25,
        };
        let vp = Viewport {
            width: 390,
            height: 844,
            touch: true,
            dpr: 2.0,
        };
        let (clip, scale) = layout.view_clip(vp, 2.0);
        assert_eq!(
            (clip.x, clip.y, clip.w, clip.h),
            (0.0, 100.0, 1560.0, 3376.0)
        );
        assert_eq!(scale, 0.5, "drawn at the viewport's size times its dpr");
        let unzoomed = PageLayout::default().view_clip(vp, 2.0);
        assert_eq!(
            (unzoomed.0.w, unzoomed.0.h, unzoomed.1),
            (390.0, 844.0, 2.0)
        );
    }

    #[test]
    fn a_layout_without_a_visible_area_is_refused_not_guessed() {
        assert_eq!(PageLayout::from_metrics(&json!({})), None);
        let odd = json!({"cssVisualViewport": {"pageX": -4.0, "pageY": "x"}});
        assert_eq!(PageLayout::from_metrics(&odd), None);
        let flat = json!({"cssVisualViewport": {"pageX": 0.0, "pageY": 0.0, "scale": 0.0}});
        assert_eq!(PageLayout::from_metrics(&flat), None);
        let overscrolled = PageLayout::from_metrics(&json!({
            "cssVisualViewport": {"pageX": -4.0, "pageY": 12.0},
            "cssContentSize": {"width": 0.0, "height": 10.0},
        }));
        let expected = PageLayout {
            scroll: (0.0, 12.0),
            ..PageLayout::default()
        };
        assert_eq!(overscrolled, Some(expected));
    }

    #[tokio::test]
    async fn a_browser_that_hides_its_layout_gives_no_frame() {
        let (b, _log) = fake_browser(|m, _| match m {
            "Page.captureScreenshot" => Some(json!({"data": jpeg_of(64)})),
            "Page.getLayoutMetrics" => Some(json!({})),
            _ => None,
        })
        .await;
        assert!(matches!(
            b.capture(None, 1.0).await,
            Err(BrowserError::Protocol(_))
        ));
    }

    /// A batch cut short by its deadline must not leave Chrome holding a press:
    /// whatever was sent down is let go, and the caller hears input was lost.
    #[tokio::test(start_paused = true)]
    async fn input_that_hangs_is_cut_short_and_lets_go_of_what_it_held() {
        let (b, log) = fake_browser(|m, p| {
            (m == "Input.dispatchKeyEvent" && p["type"] == "rawKeyDown")
                .then(crate::testutil::no_answer)
        })
        .await;
        let press = |action| Input::Mouse {
            action,
            x: 5.0,
            y: 6.0,
            button: crate::MouseButton::Left,
            click_count: 1,
            modifiers: 0,
        };
        let key = |action| Input::Key {
            action,
            key: "b".into(),
            code: "KeyB".into(),
            text: None,
            modifiers: 0,
        };
        log.lock().unwrap().clear();
        let events = [
            press(crate::MouseAction::Down),
            key(crate::KeyAction::Down),
            key(crate::KeyAction::Up),
            press(crate::MouseAction::Up),
        ];
        let lost = b.input(&events).await;
        assert!(
            matches!(&lost, Err(BrowserError::Timeout(m)) if m.contains("may not have reached")),
            "{lost:?}"
        );
        let sent: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m.starts_with("Input."))
            .map(|(_, p)| p["type"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            sent,
            ["mousePressed", "rawKeyDown", "mouseReleased", "keyUp"]
        );
        b.input(&[
            press(crate::MouseAction::Down),
            press(crate::MouseAction::Up),
        ])
        .await
        .expect("input goes on after a cut-short batch");
    }

    #[test]
    fn frames_come_faster_just_after_input() {
        assert_eq!(frame_interval(None), IDLE_FRAME_INTERVAL);
        assert_eq!(
            frame_interval(Some(Duration::from_millis(10))),
            ACTIVE_FRAME_INTERVAL
        );
        assert_eq!(
            frame_interval(Some(ACTIVE_AFTER_INPUT)),
            IDLE_FRAME_INTERVAL
        );
        assert!(ACTIVE_FRAME_INTERVAL < IDLE_FRAME_INTERVAL);
    }

    #[tokio::test]
    async fn screenshots_never_overlap_so_a_final_picture_is_not_tiled() {
        // The polling loop's scale-1 shot and a final scale-2 shot were in
        // flight together and Chrome tiled the 1x page across the 2x canvas.
        let (b, log) = crate::testutil::fake_browser_slow_shots(
            |m, _| (m == "Page.captureScreenshot").then(|| json!({"data": jpeg_of(100)})),
            Some(Duration::from_millis(40)),
        )
        .await;
        let b = Arc::new(b);
        let poller = {
            let b = b.clone();
            tokio::spawn(async move {
                for _ in 0..4 {
                    b.capture(None, 1.0).await.unwrap();
                }
            })
        };
        for _ in 0..3 {
            b.capture(None, 2.0).await.unwrap();
        }
        poller.await.unwrap();
        let mut open = 0i32;
        let mut shots = 0;
        for (m, _) in log.lock().unwrap().iter() {
            match m.as_str() {
                "Page.captureScreenshot" => {
                    open += 1;
                    shots += 1;
                    assert_eq!(open, 1, "a screenshot began while another was in flight");
                }
                "<answered>" => open -= 1,
                _ => {}
            }
        }
        assert_eq!(shots, 7);
    }

    #[tokio::test]
    async fn oversize_capture_is_retried_until_it_fits() {
        // Size shrinks with quality and scale: 3 MiB at q70/1.0, still big at q40, fits at 0.5.
        let (b, log) = fake_browser(|m, p| {
            if m != "Page.captureScreenshot" {
                return None;
            }
            let q = p["quality"].as_u64().unwrap();
            let s = p["clip"]["scale"].as_f64().unwrap();
            let base = if q == 70 {
                3 * 1024 * 1024
            } else {
                1024 * 1024
            };
            let len = (base as f64 * s * s) as usize;
            Some(json!({"data": jpeg_of(len)}))
        })
        .await;
        let pic = b
            .capture(
                Some(ClipRect {
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 50.0,
                }),
                1.0,
            )
            .await
            .unwrap();
        assert!(pic.bytes.len() <= MAX_IMAGE_BYTES, "{}", pic.bytes.len());
        assert_eq!(pic.mime, "image/jpeg");
        let shots: Vec<_> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "Page.captureScreenshot")
            .map(|(_, p)| {
                (
                    p["quality"].as_u64().unwrap(),
                    p["clip"]["scale"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(shots, vec![(70, 1.0), (40, 1.0), (40, 0.5)]);
        assert_eq!(
            pic.capture.screenshot_scale, 0.5,
            "the scale that fit is recorded"
        );
        assert_eq!((pic.capture.w, pic.capture.h), (100, 50));
        // The wire JSON of a data-URL frame stays under 768 KiB.
        let wire = json!({"jpeg_b64": B64.encode(&pic.bytes), "w": pic.width, "h": pic.height})
            .to_string();
        assert!(wire.len() <= 768 * 1024, "{}", wire.len());
        let worst = json!({"jpeg_b64": B64.encode(vec![0u8; MAX_IMAGE_BYTES]), "w": 99999, "h": 99999, "at": 1_700_000_000_000u64, "seq": 1u64 << 40}).to_string();
        assert!(worst.len() <= 768 * 1024);
    }

    #[tokio::test]
    async fn guards_and_events() {
        let (b, log) = fake_browser(|_, _| None).await;
        let methods: Vec<String> = log.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
        for want in [
            "Page.enable",
            "DOM.enable",
            "CSS.enable",
            "Network.enable",
            "Runtime.enable",
            "Page.setInterceptFileChooserDialog",
            "Browser.setDownloadBehavior",
        ] {
            assert!(methods.iter().any(|m| m == want), "missing {want}");
        }
        assert!(b.pid().is_none() && b.started_at().is_none());
        let _ = b.events();
    }
}
