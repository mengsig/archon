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
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cdp::{CLOSED_EVENT, Cdp, CdpEvent, MAX_FRAME};
use crate::launch::{
    SHELL_SCRIPT, classify_stderr, kill_group, launch_args, launch_env, process_start_time,
};
use crate::relay::{FrameRelay, RelayShared};
use crate::sanitize::clean;
use crate::{
    BrowserError, BrowserEvent, ClipRect, Input, KeyAction, LaunchSpec, MAX_IMAGE_BYTES,
    MouseAction, MouseButton, Picture, TouchAction, Viewport, jpeg_dims,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct State {
    url: String,
    reqs: HashMap<String, String>,
    last_net: Instant,
    blocked: HashSet<String>,
    viewport: Viewport,
    /// Screencast degradation step: 0 q70, 1 q40, 2+ q40 at half width, quarter, ...
    step: u32,
    restarting: bool,
    poll: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) cdp: Cdp,
    pub(crate) session: String,
    target_id: String,
    events: broadcast::Sender<BrowserEvent>,
    pub(crate) relay: Arc<RelayShared>,
    st: Mutex<State>,
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
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.event_task.abort();
        if let (Some(pid), Some(_)) = (self.pid, self.child.as_ref()) {
            kill_group(pid);
        }
        if let Some(h) = self.sh.st.lock().unwrap().poll.take() {
            h.abort();
        }
    }
}

fn b64(s: &str) -> Result<Vec<u8>, BrowserError> {
    B64.decode(s)
        .map_err(|e| BrowserError::Protocol(format!("bad base64: {e}")))
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
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

fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::None => "none",
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
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
        self.cdp
            .call(Some(&self.session), method, params, CALL_TIMEOUT)
            .await
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

    async fn capture(&self, clip: Option<ClipRect>, dpr: f64) -> Result<Picture, BrowserError> {
        // Crops are in page CSS pixels: take them on the unzoomed page the
        // operator saw, and leave it unzoomed for the stream. A full-view
        // shot (each polled stream frame) zooms nothing and pays nothing.
        if clip.is_none() {
            return self.capture_at_scale_one(None, dpr).await;
        }
        self.reset_zoom().await;
        let picture = self.capture_at_scale_one(clip, dpr).await;
        self.reset_zoom().await;
        picture
    }

    async fn capture_at_scale_one(
        &self,
        clip: Option<ClipRect>,
        dpr: f64,
    ) -> Result<Picture, BrowserError> {
        let vp = self.viewport();
        let clip = clip.unwrap_or(ClipRect {
            x: 0.0,
            y: 0.0,
            w: f64::from(vp.width),
            h: f64::from(vp.height),
        });
        if !(clip.w >= 1.0 && clip.h >= 1.0) {
            return Err(BrowserError::Protocol("empty capture area".into()));
        }
        let (bytes, scale) = fit_to_budget(|q, s| self.shoot(clip, q, s), dpr).await?;
        let (width, height) = jpeg_dims(&bytes).unwrap_or((
            (clip.w * scale).round() as u32,
            (clip.h * scale).round() as u32,
        ));
        Ok(Picture {
            mime: "image/jpeg",
            bytes,
            width,
            height,
        })
    }

    fn screencast_params(&self) -> Value {
        let st = self.st.lock().unwrap();
        let (q, f) = match st.step {
            0 => (70, 1.0),
            1 => (40, 1.0),
            n => (40, 0.5f64.powi(n as i32 - 1)),
        };
        let w = (f64::from(st.viewport.width) * f).max(1.0).round();
        let h = (f64::from(st.viewport.height) * f).max(1.0).round();
        json!({"format": "jpeg", "quality": q, "maxWidth": w, "maxHeight": h, "everyNthFrame": 1})
    }

    async fn begin_screencast(&self) -> Result<(), BrowserError> {
        self.call("Page.startScreencast", self.screencast_params())
            .await
            .map(|_| ())
    }

    fn degrade_screencast(self: &Arc<Self>) {
        {
            let mut st = self.st.lock().unwrap();
            if st.restarting || st.poll.is_some() || st.step >= 8 {
                return;
            }
            st.restarting = true;
            st.step += 1;
        }
        let sh = self.clone();
        tokio::spawn(async move {
            let _ = sh.call("Page.stopScreencast", json!({})).await;
            let _ = sh.begin_screencast().await;
            sh.st.lock().unwrap().restarting = false;
        });
    }

    fn start_polling(self: &Arc<Self>) {
        let sh = self.clone();
        let h = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                tick.tick().await;
                match sh.capture(None, 1.0).await {
                    Ok(p) => {
                        sh.relay.push(p.bytes, p.width, p.height);
                    }
                    Err(BrowserError::Closed) => break,
                    Err(_) => {}
                }
            }
        });
        if let Some(old) = self.st.lock().unwrap().poll.replace(h) {
            old.abort();
        }
    }

    /// Returns true when the loop should stop.
    fn handle(self: &Arc<Self>, ev: CdpEvent) -> bool {
        if ev.method == CLOSED_EVENT {
            self.emit(BrowserEvent::Exited);
            return true;
        }
        let p = &ev.params;
        let ours = ev.session_id.as_deref() == Some(self.session.as_str());
        match ev.method.as_str() {
            "Target.targetCreated" => {
                let info = &p["targetInfo"];
                let id = info["targetId"].as_str().unwrap_or_default();
                if info["type"] == "page" && id != self.target_id {
                    let url = clean(info["url"].as_str().unwrap_or_default());
                    let sh = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        let _ = sh
                            .cdp
                            .call(
                                None,
                                "Target.closeTarget",
                                json!({"targetId": id}),
                                CALL_TIMEOUT,
                            )
                            .await;
                    });
                    self.emit(BrowserEvent::PopupClosed { url });
                }
            }
            "Inspector.targetCrashed" if ours => self.emit(BrowserEvent::Crashed),
            "Page.javascriptDialogOpening" if ours => self.emit(BrowserEvent::Dialog {
                kind: clean(p["type"].as_str().unwrap_or("alert")),
                message: clean(p["message"].as_str().unwrap_or_default()),
                default_prompt: p["defaultPrompt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(clean),
            }),
            "Page.fileChooserOpened" if ours => self.emit(BrowserEvent::FileChooserRefused),
            "Page.frameNavigated" if ours && p["frame"]["parentId"].is_null() => {
                if let Some(u) = p["frame"]["url"].as_str() {
                    self.st.lock().unwrap().url = u.to_string();
                }
            }
            "Page.navigatedWithinDocument" if ours => {
                if let Some(u) = p["url"].as_str() {
                    self.st.lock().unwrap().url = u.to_string();
                }
            }
            "Page.loadEventFired" if ours => {
                let url = clean(&self.st.lock().unwrap().url);
                self.emit(BrowserEvent::Loaded { url });
            }
            "Page.screencastFrame" if ours => self.on_frame(p),
            "Network.requestWillBeSent" if ours => {
                if let (Some(id), Some(url)) =
                    (p["requestId"].as_str(), p["request"]["url"].as_str())
                {
                    let mut st = self.st.lock().unwrap();
                    st.last_net = Instant::now();
                    if st.reqs.len() < 4096 {
                        st.reqs.insert(id.to_string(), url.to_string());
                    }
                }
            }
            "Network.loadingFinished" if ours => {
                let mut st = self.st.lock().unwrap();
                st.last_net = Instant::now();
                if let Some(id) = p["requestId"].as_str() {
                    st.reqs.remove(id);
                }
            }
            "Network.loadingFailed" if ours => self.on_failed(p),
            _ => {}
        }
        false
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
        if let (true, Some(host)) = (proxy_failure, host_of(&url)) {
            let host = clean(&host);
            if self.st.lock().unwrap().blocked.insert(host.clone()) {
                self.emit(BrowserEvent::BlockedHost { host });
            }
        }
        if p["type"] == "Document" && p["canceled"] != true {
            self.emit(BrowserEvent::NavigationFailed {
                url: clean(&url),
                error: clean(err),
            });
        }
    }

    fn on_frame(self: &Arc<Self>, p: &Value) {
        if let Some(sid) = p["sessionId"].as_i64() {
            let sh = self.clone();
            tokio::spawn(async move {
                let _ = sh
                    .call("Page.screencastFrameAck", json!({"sessionId": sid}))
                    .await;
            });
        }
        let Ok(jpeg) = b64(p["data"].as_str().unwrap_or_default()) else {
            return;
        };
        if jpeg.len() > MAX_IMAGE_BYTES {
            self.degrade_screencast();
            return;
        }
        let (w, h) = jpeg_dims(&jpeg).unwrap_or((
            p["metadata"]["deviceWidth"].as_f64().unwrap_or(0.0) as u32,
            p["metadata"]["deviceHeight"].as_f64().unwrap_or(0.0) as u32,
        ));
        self.relay.push(jpeg, w, h);
    }
}

impl Browser {
    /// Start Chromium and attach to its page.
    pub async fn launch(spec: LaunchSpec) -> Result<Browser, BrowserError> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let io = |e: std::io::Error| BrowserError::Launch(e.to_string());
        if let Some(parent) = spec.profile_dir.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&spec.profile_dir)
            .map_err(io)?;
        fs::set_permissions(&spec.profile_dir, fs::Permissions::from_mode(0o700)).map_err(io)?;
        let result = Self::spawn(&spec).await;
        if result.is_err() {
            let _ = fs::remove_dir_all(&spec.profile_dir);
        }
        result
    }

    async fn spawn(spec: &LaunchSpec) -> Result<Browser, BrowserError> {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(SHELL_SCRIPT)
            .arg(&spec.binary)
            .args(launch_args(spec))
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
        let started_at = pid.and_then(process_start_time);
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(BrowserError::Launch("child pipes missing".into()));
        };
        let tail = Arc::new(Mutex::new(String::new()));
        let t2 = tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = t2.lock().unwrap();
                t.push_str(&line);
                t.push('\n');
                if t.len() > 16 * 1024 {
                    let cut = t.len() - 8 * 1024;
                    let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                    t.drain(..cut);
                }
            }
        });
        let fail = |child: &mut Child, why: String| {
            if let Some(p) = child.id() {
                kill_group(p);
            }
            let text = tail.lock().unwrap().clone();
            classify_stderr(&text).unwrap_or_else(|| {
                let t: String = text
                    .chars()
                    .rev()
                    .take(500)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                BrowserError::Launch(format!("{why}: {}", t.trim()))
            })
        };
        let connected = tokio::select! {
            r = tokio::time::timeout(LAUNCH_TIMEOUT, Self::attach(stdout, stdin, Some(spec.viewport))) => Some(r),
            status = child.wait() => {
                let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
                let why = format!("browser exited early ({})", status.map(|s| s.to_string()).unwrap_or_default());
                return Err(fail(&mut child, why));
            }
        };
        let mut b = match connected {
            Some(Ok(Ok(b))) => b,
            Some(Ok(Err(e))) => {
                let _ = tokio::time::timeout(Duration::from_millis(300), stderr_task).await;
                let text = tail.lock().unwrap().clone();
                if let Some(s) = classify_stderr(&text) {
                    if let Some(p) = child.id() {
                        kill_group(p);
                    }
                    return Err(s);
                }
                return Err(fail(&mut child, e.to_string()));
            }
            _ => return Err(fail(&mut child, "browser did not answer in time".into())),
        };
        b.child = Some(child);
        b.profile = Some(spec.profile_dir.clone());
        b.pid = pid;
        b.started_at = started_at;
        Ok(b)
    }

    /// Drive a browser over an existing CDP pipe (no process is owned).
    pub async fn connect<R, W>(reader: R, writer: W) -> Result<Browser, BrowserError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::attach(reader, writer, None).await
    }

    async fn attach<R, W>(
        reader: R,
        writer: W,
        viewport: Option<Viewport>,
    ) -> Result<Browser, BrowserError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let cdp = Cdp::new(reader, writer, MAX_FRAME);
        let mut rx = cdp.subscribe();
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
            cdp.call(Some(&session), m, json!({}), CALL_TIMEOUT).await?;
        }
        cdp.call(
            Some(&session),
            "Page.setInterceptFileChooserDialog",
            json!({"enabled": true}),
            CALL_TIMEOUT,
        )
        .await?;
        let vp = viewport.unwrap_or(Viewport {
            width: 1280,
            height: 800,
            touch: false,
            dpr: 1.0,
        });
        let (events, _) = broadcast::channel(256);
        let sh = Arc::new(Shared {
            cdp,
            session,
            target_id,
            events,
            relay: Arc::new(RelayShared::default()),
            st: Mutex::new(State {
                url: String::new(),
                reqs: HashMap::new(),
                last_net: Instant::now(),
                blocked: HashSet::new(),
                viewport: vp,
                step: 0,
                restarting: false,
                poll: None,
            }),
        });
        let sh2 = sh.clone();
        let event_task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if sh2.handle(ev) {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
        let b = Browser {
            sh,
            child: None,
            profile: None,
            pid: None,
            started_at: None,
            event_task,
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

    /// Dispatch input events in order.
    pub async fn input(&self, events: &[Input]) -> Result<(), BrowserError> {
        for e in events {
            let (method, params) = match e {
                Input::Mouse {
                    action,
                    x,
                    y,
                    button,
                    click_count,
                    modifiers,
                } => (
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": match action { MouseAction::Move => "mouseMoved", MouseAction::Down => "mousePressed", MouseAction::Up => "mouseReleased" },
                        "x": x, "y": y, "button": button_name(*button), "clickCount": click_count, "modifiers": modifiers,
                    }),
                ),
                Input::Wheel {
                    x,
                    y,
                    dx,
                    dy,
                    modifiers,
                } => (
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy, "modifiers": modifiers}),
                ),
                Input::Key {
                    action,
                    key,
                    code,
                    text,
                    modifiers,
                } => {
                    let ty = match (action, text.as_deref().is_some_and(|t| !t.is_empty())) {
                        (KeyAction::Up, _) => "keyUp",
                        (KeyAction::Down, true) => "keyDown",
                        (KeyAction::Down, false) => "rawKeyDown",
                    };
                    let mut p =
                        json!({"type": ty, "key": key, "code": code, "modifiers": modifiers});
                    if let (KeyAction::Down, Some(t)) = (action, text) {
                        p["text"] = json!(t);
                    }
                    ("Input.dispatchKeyEvent", p)
                }
                Input::Text { text } => ("Input.insertText", json!({"text": text})),
                Input::Touch { action, points } => (
                    "Input.dispatchTouchEvent",
                    json!({
                        "type": match action { TouchAction::Start => "touchStart", TouchAction::Move => "touchMove", TouchAction::End => "touchEnd", TouchAction::Cancel => "touchCancel" },
                        "touchPoints": points.iter().map(|p| json!({"x": p.x, "y": p.y, "id": p.id})).collect::<Vec<_>>(),
                    }),
                ),
            };
            self.sh.call(method, params).await?;
        }
        Ok(())
    }

    /// Start the screencast; falls back to polling at 4 fps if the browser refuses it.
    pub async fn start_screencast(&self) -> Result<FrameRelay, BrowserError> {
        {
            let mut st = self.sh.st.lock().unwrap();
            st.step = 0;
            st.restarting = false;
            if let Some(h) = st.poll.take() {
                h.abort();
            }
        }
        if self.sh.begin_screencast().await.is_err() {
            self.sh.start_polling();
        }
        Ok(FrameRelay {
            shared: self.sh.relay.clone(),
        })
    }

    /// Stop the screencast (or the polling fallback).
    pub async fn stop_screencast(&self) -> Result<(), BrowserError> {
        let poll = self.sh.st.lock().unwrap().poll.take();
        match poll {
            Some(h) => {
                h.abort();
                Ok(())
            }
            None => self
                .sh
                .call("Page.stopScreencast", json!({}))
                .await
                .map(|_| ()),
        }
    }

    /// A picture of the viewport or `clip`, at most 560 KiB.
    pub async fn capture(&self, clip: Option<ClipRect>, dpr: f64) -> Result<Picture, BrowserError> {
        self.sh.capture(clip, dpr).await
    }

    /// Path of the current main-frame URL (no query, no fragment).
    pub async fn current_path(&self) -> Result<String, BrowserError> {
        Ok(clean(&path_of(&self.sh.st.lock().unwrap().url)))
    }

    /// Wait until no request has been in flight for `quiet`; false if `max` passes first.
    pub async fn wait_network_idle(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + max;
        loop {
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
        if let Some(p) = self.profile.take() {
            let mut last = None;
            for _ in 0..20 {
                match fs::remove_dir_all(&p) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;

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
