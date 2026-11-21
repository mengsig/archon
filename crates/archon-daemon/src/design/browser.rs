//! The preview browser as the design service sees it: a trait over the
//! `archon_design_browser` driver, so tests drive a fake, and the manager that
//! launches it lazily, keeps it inside the session cap and relaunches it.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::design::DesignSessionId;
use archon_core::error::ArchonError;
use archon_core::protocol::{BrowserStatus, DesignEvent};
use archon_design_browser as adb;
use tokio::sync::{broadcast, watch};

use super::DesignService;
use crate::context::DaemonContext;

/// Most crashes of one session's browser inside [`CRASH_WINDOW`].
pub(crate) const MAX_CRASHES: usize = 5;
pub(crate) const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// Resident memory above which the browser is restarted, held this long.
pub(crate) const RSS_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const RSS_HOLD: Duration = Duration::from_secs(30);
/// The screencast stops this long after the last subscriber left.
pub(crate) const IDLE_STOP: Duration = Duration::from_secs(60);

/// Frames of a running screencast.
#[async_trait::async_trait]
pub trait FrameFeed: Send {
    async fn next(&mut self) -> Option<adb::Frame>;
    fn ack(&mut self, seq: u64);
}

/// The driver's methods the service uses. Implemented for the real browser
/// and by a fake in tests.
#[async_trait::async_trait]
pub trait PreviewBrowser: Send + Sync {
    fn pid(&self) -> Option<u32>;
    fn started_at(&self) -> Option<u64>;
    async fn navigate(&self, url: &str) -> Result<(), adb::BrowserError>;
    async fn reload(&self) -> Result<(), adb::BrowserError>;
    async fn set_viewport(&self, viewport: adb::Viewport) -> Result<(), adb::BrowserError>;
    async fn input(&self, events: &[adb::Input]) -> Result<(), adb::BrowserError>;
    async fn start_screencast(&self) -> Result<Box<dyn FrameFeed>, adb::BrowserError>;
    async fn stop_screencast(&self) -> Result<(), adb::BrowserError>;
    async fn capture(
        &self,
        clip: Option<adb::ClipRect>,
        dpr: f64,
    ) -> Result<adb::Picture, adb::BrowserError>;
    async fn resolve(&self, x: f64, y: f64, depth: i32)
    -> Result<adb::Resolved, adb::BrowserError>;
    async fn resolve_selector(
        &self,
        selector: &str,
    ) -> Result<Option<adb::ClipRect>, adb::BrowserError>;
    async fn current_path(&self) -> Result<String, adb::BrowserError>;
    async fn wait_network_idle(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<bool, adb::BrowserError>;
    fn latest_seq(&self) -> u64;
    async fn frame_changed_within(&self, since_seq: u64, max: Duration) -> bool;
    async fn answer_dialog(
        &self,
        accept: bool,
        text: Option<String>,
    ) -> Result<(), adb::BrowserError>;
    fn events(&self) -> broadcast::Receiver<adb::BrowserEvent>;
    fn rss_bytes(&self) -> Option<u64>;
    async fn close(&self);
}

/// How browsers are found and started; tests replace it.
#[async_trait::async_trait]
pub trait BrowserFactory: Send + Sync {
    /// The browser binary, or the driver's refusal with its install hint.
    fn binary(&self, configured: Option<&std::path::Path>) -> Result<PathBuf, adb::BrowserError>;
    async fn launch(
        &self,
        spec: adb::LaunchSpec,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError>;
}

/// The real driver.
#[derive(Debug)]
pub struct RealFactory;

#[async_trait::async_trait]
impl BrowserFactory for RealFactory {
    fn binary(&self, configured: Option<&std::path::Path>) -> Result<PathBuf, adb::BrowserError> {
        let path = std::env::var("PATH").unwrap_or_default();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        adb::find_browser(configured, &path, &home)
    }

    async fn launch(
        &self,
        spec: adb::LaunchSpec,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError> {
        let browser = adb::Browser::launch(spec).await?;
        Ok(Arc::new(Real {
            inner: std::sync::Mutex::new(Some(Arc::new(browser))),
            relay: std::sync::Mutex::new(None),
        }))
    }
}

struct Real {
    inner: std::sync::Mutex<Option<Arc<adb::Browser>>>,
    relay: std::sync::Mutex<Option<adb::FrameRelay>>,
}

impl Real {
    fn get(&self) -> Result<Arc<adb::Browser>, adb::BrowserError> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(adb::BrowserError::Closed)
    }
}

struct RealFeed(adb::FrameSubscriber);

#[async_trait::async_trait]
impl FrameFeed for RealFeed {
    async fn next(&mut self) -> Option<adb::Frame> {
        self.0.next().await
    }
    fn ack(&mut self, seq: u64) {
        self.0.ack(seq);
    }
}

#[async_trait::async_trait]
impl PreviewBrowser for Real {
    fn pid(&self) -> Option<u32> {
        self.get().ok().and_then(|b| b.pid())
    }
    fn started_at(&self) -> Option<u64> {
        self.get().ok().and_then(|b| b.started_at())
    }
    async fn navigate(&self, url: &str) -> Result<(), adb::BrowserError> {
        self.get()?.navigate(url).await
    }
    async fn reload(&self) -> Result<(), adb::BrowserError> {
        self.get()?.reload().await
    }
    async fn set_viewport(&self, viewport: adb::Viewport) -> Result<(), adb::BrowserError> {
        self.get()?.set_viewport(viewport).await
    }
    async fn input(&self, events: &[adb::Input]) -> Result<(), adb::BrowserError> {
        self.get()?.input(events).await
    }
    async fn start_screencast(&self) -> Result<Box<dyn FrameFeed>, adb::BrowserError> {
        let relay = self.get()?.start_screencast().await?;
        let feed = RealFeed(relay.subscribe());
        *self.relay.lock().unwrap_or_else(|e| e.into_inner()) = Some(relay);
        Ok(Box::new(feed))
    }
    async fn stop_screencast(&self) -> Result<(), adb::BrowserError> {
        self.relay.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.get()?.stop_screencast().await
    }
    async fn capture(
        &self,
        clip: Option<adb::ClipRect>,
        dpr: f64,
    ) -> Result<adb::Picture, adb::BrowserError> {
        self.get()?.capture(clip, dpr).await
    }
    async fn resolve(
        &self,
        x: f64,
        y: f64,
        depth: i32,
    ) -> Result<adb::Resolved, adb::BrowserError> {
        self.get()?.resolve(x, y, depth).await
    }
    async fn resolve_selector(
        &self,
        selector: &str,
    ) -> Result<Option<adb::ClipRect>, adb::BrowserError> {
        self.get()?.resolve_selector(selector).await
    }
    async fn current_path(&self) -> Result<String, adb::BrowserError> {
        self.get()?.current_path().await
    }
    async fn wait_network_idle(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<bool, adb::BrowserError> {
        self.get()?.wait_network_idle(quiet, max).await
    }
    fn latest_seq(&self) -> u64 {
        self.relay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |relay| relay.latest_seq())
    }
    async fn frame_changed_within(&self, since_seq: u64, max: Duration) -> bool {
        match self.get() {
            Ok(browser) => browser.frame_changed_within(since_seq, max).await,
            Err(_) => false,
        }
    }
    async fn answer_dialog(
        &self,
        accept: bool,
        text: Option<String>,
    ) -> Result<(), adb::BrowserError> {
        self.get()?.answer_dialog(accept, text).await
    }
    fn events(&self) -> broadcast::Receiver<adb::BrowserEvent> {
        match self.get() {
            Ok(browser) => browser.events(),
            Err(_) => broadcast::channel(1).1,
        }
    }
    fn rss_bytes(&self) -> Option<u64> {
        self.get().ok().and_then(|b| b.rss_bytes())
    }
    async fn close(&self) {
        let taken = self.inner.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(mut arc) = taken else {
            return;
        };
        // Calls in flight hold clones; wait briefly for them to finish.
        for _ in 0..50 {
            match Arc::try_unwrap(arc) {
                Ok(browser) => {
                    if let Err(error) = browser.close().await {
                        tracing::warn!(%error, "closing the preview browser failed");
                    }
                    return;
                }
                Err(back) => {
                    arc = back;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        tracing::warn!("a preview browser call did not end; the browser is dropped");
    }
}

/// What the service keeps of one session's browser.
pub(crate) struct Runtime {
    pub events: broadcast::Sender<DesignEvent>,
    pub frames: watch::Sender<Option<Arc<adb::Frame>>>,
    pub state: std::sync::Mutex<RtState>,
    /// Serializes launch, relaunch and close.
    pub gate: tokio::sync::Mutex<()>,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub inputs: u64,
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub crashes: u64,
    pub blocked: BTreeSet<String>,
    pub latencies_ms: Vec<u64>,
}

pub(crate) struct RtState {
    pub browser: Option<Arc<dyn PreviewBrowser>>,
    pub status: BrowserStatus,
    pub subscribers: u32,
    pub screencast: bool,
    pub last_sub_gone: Option<Instant>,
    pub crashes: VecDeque<Instant>,
    pub last_url: Option<String>,
    pub rss_over_since: Option<Instant>,
    pub counters: Counters,
    pub pending_input: Option<Instant>,
    pub input_window: (Instant, u32),
    pub resolve_window: (Instant, u32),
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Default for RtState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            browser: None,
            status: BrowserStatus::Idle,
            subscribers: 0,
            screencast: false,
            last_sub_gone: None,
            crashes: VecDeque::new(),
            last_url: None,
            rss_over_since: None,
            counters: Counters::default(),
            pending_input: None,
            input_window: (now, 0),
            resolve_window: (now, 0),
            tasks: Vec::new(),
        }
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            events: broadcast::channel(256).0,
            frames: watch::channel(None).0,
            state: std::sync::Mutex::new(RtState::default()),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, RtState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn browser(&self) -> Option<Arc<dyn PreviewBrowser>> {
        self.st().browser.clone()
    }
}

/// Per-second windows: at most `limit` calls each second.
pub(crate) fn within_rate(window: &mut (Instant, u32), limit: u32) -> bool {
    let now = Instant::now();
    if now.duration_since(window.0) >= Duration::from_secs(1) {
        *window = (now, 0);
    }
    if window.1 >= limit {
        return false;
    }
    window.1 += 1;
    true
}

pub(crate) fn to_adb_viewport(
    preset: archon_core::design::ViewportPreset,
) -> (archon_core::design::Viewport, adb::Viewport) {
    let (w, h) = preset.size();
    (
        archon_core::design::Viewport { w, h, preset },
        adb::Viewport {
            width: w,
            height: h,
            touch: preset.touch(),
            dpr: 1.0,
        },
    )
}

impl DesignService {
    pub(crate) fn runtime(&self, session: &DesignSessionId) -> Arc<Runtime> {
        let mut all = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        all.entry(session.clone())
            .or_insert_with(|| Arc::new(Runtime::new()))
            .clone()
    }

    pub(crate) fn existing_runtime(&self, session: &DesignSessionId) -> Option<Arc<Runtime>> {
        self.rt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .cloned()
    }

    fn live_browsers(&self) -> usize {
        let all = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        all.values().filter(|rt| rt.st().browser.is_some()).count()
    }

    pub(crate) fn profile_dir(&self, session: &DesignSessionId) -> PathBuf {
        self.data_dir
            .join("design")
            .join("profiles")
            .join(&session.0)
    }

    /// The session's browser, launched now if it is not running: lazily, on an
    /// operator's subscribe or capture. `Ok(None)` when none can run (no
    /// preview yet, no slot, no browser on the host); the status says why.
    pub(crate) async fn ensure_browser(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<Option<Arc<dyn PreviewBrowser>>, ArchonError> {
        let rt = self.runtime(session);
        let _gate = rt.gate.lock().await;
        if let Some(browser) = rt.browser() {
            return Ok(Some(browser));
        }
        let row = {
            let store = ctx.store.lock().await;
            store.design_session(session)?
        };
        let Some(row) = row else {
            return Ok(None);
        };
        let Some(url) = rt.st().last_url.clone().or(row.preview_url.clone()) else {
            return Ok(None);
        };
        if self.live_browsers() >= self.cfg.max_sessions as usize {
            self.set_status(&rt, session, BrowserStatus::QueuedForSlot);
            return Ok(None);
        }
        if rt.st().status == BrowserStatus::CrashedTooOften {
            return Ok(None);
        }
        self.set_status(&rt, session, BrowserStatus::Starting);
        let wt = self.worktree_of(ctx, &row).await?;
        let allow_hosts = super::repo_hosts::allow_hosts(ctx, &wt, &self.data_dir).await;
        match self.launch_into(ctx, &rt, &row, &url, allow_hosts).await {
            Ok(browser) => Ok(Some(browser)),
            Err(adb::BrowserError::NotFound { hint }) => {
                self.set_status(&rt, session, BrowserStatus::Missing { hint });
                Ok(None)
            }
            Err(adb::BrowserError::SandboxUnavailable { hint }) => {
                self.set_status(&rt, session, BrowserStatus::SandboxUnavailable { hint });
                Ok(None)
            }
            Err(error) => {
                self.set_status(&rt, session, BrowserStatus::Idle);
                Err(ArchonError::Unavailable(format!(
                    "the preview browser did not start: {error}"
                )))
            }
        }
    }

    async fn launch_into(
        &self,
        ctx: &DaemonContext,
        rt: &Arc<Runtime>,
        row: &archon_core::design::DesignSession,
        url: &str,
        allow_hosts: Vec<String>,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError> {
        let factory = self.factory();
        let binary = factory.binary(self.cfg.browser.as_deref())?;
        let profile = self.profile_dir(&row.id);
        let _ = std::fs::remove_dir_all(&profile);
        if let Some(parent) = profile.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| adb::BrowserError::Launch(e.to_string()))?;
        }
        let (_, viewport) = to_adb_viewport(row.viewport_preset);
        let spec = adb::LaunchSpec {
            binary,
            profile_dir: profile,
            sandbox: !row.sandbox_off,
            allow_hosts,
            viewport,
            path_env: std::env::var("PATH").unwrap_or_default(),
            lang: "en-US".into(),
            tz: "UTC".into(),
        };
        let browser = factory.launch(spec).await?;
        if let Err(error) = browser.navigate(url).await {
            tracing::info!(%error, url, "the first navigation of the preview failed");
        }
        let pid = browser.pid();
        let started = browser.started_at();
        {
            let mut state = rt.st();
            state.browser = Some(browser.clone());
            state.last_url = Some(url.to_owned());
            state.status = if row.sandbox_off {
                BrowserStatus::Unsandboxed
            } else {
                BrowserStatus::Running
            };
        }
        {
            let mut store = ctx.store.lock().await;
            let patch = archon_queue::store::SessionPatch {
                browser: Some(pid.zip(started)),
                ..Default::default()
            };
            if let Err(error) = store.design_session_patch(&row.id, patch) {
                tracing::warn!(%error, "the browser's pid was not recorded");
            }
        }
        tracing::info!(session = %row.id, ?pid, sandbox = !row.sandbox_off, device = ?super::current_device_label(), "design: browser started");
        self.emit_session(ctx, &row.id).await;
        self.spawn_monitor(ctx, row.id.clone());
        Ok(browser)
    }

    fn set_status(&self, rt: &Runtime, session: &DesignSessionId, status: BrowserStatus) {
        rt.st().status = status;
        let _ = session;
    }

    /// Start frames for a subscriber; the feeder runs while any is attached.
    pub(crate) async fn subscriber_joined(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<(), ArchonError> {
        let browser = self.ensure_browser(ctx, session).await?;
        let rt = self.runtime(session);
        let start = {
            let mut st = rt.st();
            st.last_sub_gone = None;
            !st.screencast && browser.is_some()
        };
        if let (true, Some(browser)) = (start, browser) {
            match browser.start_screencast().await {
                Ok(mut feed) => {
                    rt.st().screencast = true;
                    let sender = rt.frames.clone();
                    let task = tokio::spawn(async move {
                        while let Some(frame) = feed.next().await {
                            let seq = frame.seq;
                            sender.send_replace(Some(Arc::new(frame)));
                            feed.ack(seq);
                        }
                    });
                    rt.st().tasks.push(task);
                }
                Err(error) => tracing::warn!(%error, "the screencast did not start"),
            }
        }
        Ok(())
    }

    /// Count a subscriber; its guard calls `subscriber_left`.
    pub(crate) fn subscriber_counted(&self, session: &DesignSessionId) {
        let rt = self.runtime(session);
        let mut st = rt.st();
        st.subscribers += 1;
        st.last_sub_gone = None;
    }

    /// Subscribers attached to a session's stream.
    pub fn subscriber_count(&self, session: &DesignSessionId) -> u32 {
        self.existing_runtime(session)
            .map_or(0, |rt| rt.st().subscribers)
    }

    pub(crate) fn subscriber_left(&self, session: &DesignSessionId) {
        if let Some(rt) = self.existing_runtime(session) {
            let mut st = rt.st();
            st.subscribers = st.subscribers.saturating_sub(1);
            if st.subscribers == 0 {
                st.last_sub_gone = Some(Instant::now());
            }
        }
    }

    /// Close the session's browser, delete its profile and log its counters.
    pub(crate) async fn close_session_browser(&self, session: &DesignSessionId, reason: &str) {
        let Some(rt) = self.existing_runtime(session) else {
            let _ = std::fs::remove_dir_all(self.profile_dir(session));
            return;
        };
        let _gate = rt.gate.lock().await;
        let (browser, tasks, counters) = {
            let mut st = rt.st();
            for task in st.tasks.drain(..) {
                task.abort();
            }
            st.screencast = false;
            let counters = std::mem::take(&mut st.counters);
            st.status = BrowserStatus::Idle;
            (st.browser.take(), Vec::<()>::new(), counters)
        };
        let _ = tasks;
        if let Some(browser) = browser {
            browser.close().await;
            tracing::info!(%session, reason, "design: browser stopped");
        }
        let _ = std::fs::remove_dir_all(self.profile_dir(session));
        let mut lat = counters.latencies_ms.clone();
        lat.sort_unstable();
        let pct = |p: usize| {
            lat.get((lat.len().saturating_sub(1)) * p / 100)
                .copied()
                .unwrap_or(0)
        };
        tracing::info!(
            %session,
            inputs = counters.inputs,
            frames_sent = counters.frames_sent,
            frames_dropped = counters.frames_dropped,
            input_to_frame_p50_ms = pct(50),
            input_to_frame_p95_ms = pct(95),
            crashes = counters.crashes,
            blocked_hosts = counters.blocked.len(),
            "design: session counters"
        );
        self.rt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
    }

    fn spawn_monitor(&self, ctx: &DaemonContext, session: DesignSessionId) {
        let Some(arc) = self.arc() else {
            return;
        };
        let _ = ctx;
        let rt = self.runtime(&session);
        // Subscribe now, so an event right after the launch is not missed.
        let Some(events) = rt.browser().map(|b| b.events()) else {
            return;
        };
        let task = tokio::spawn(async move {
            monitor(arc, session, events).await;
        });
        rt.st().tasks.push(task);
    }

    /// Relaunch after a crash (fresh profile, the last URL), within the budget.
    pub(crate) async fn relaunch(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        crash: bool,
    ) -> Result<(), ArchonError> {
        let rt = self.runtime(session);
        {
            let mut st = rt.st();
            if crash {
                let now = Instant::now();
                while st
                    .crashes
                    .front()
                    .is_some_and(|at| now.duration_since(*at) > CRASH_WINDOW)
                {
                    st.crashes.pop_front();
                }
                st.crashes.push_back(now);
                st.counters.crashes += 1;
                if st.crashes.len() > MAX_CRASHES {
                    st.status = BrowserStatus::CrashedTooOften;
                }
            }
        }
        let old = {
            let _gate = rt.gate.lock().await;
            let (gave_up, old) = {
                let mut st = rt.st();
                st.screencast = false;
                if st.status == BrowserStatus::CrashedTooOften {
                    (true, st.browser.take())
                } else {
                    st.rss_over_since = None;
                    (false, st.browser.take())
                }
            };
            if gave_up {
                if let Some(old) = old {
                    old.close().await;
                }
                self.emit_session(ctx, session).await;
                return Ok(());
            }
            old
        };
        if let Some(old) = old {
            old.close().await;
        }
        let want_frames = rt.st().subscribers > 0;
        let browser = self.ensure_browser(ctx, session).await?;
        if let (true, Some(browser)) = (want_frames, browser) {
            // Re-attach the feeder to the new screencast.
            let mut feed = browser
                .start_screencast()
                .await
                .map_err(|e| ArchonError::Unavailable(e.to_string()))?;
            rt.st().screencast = true;
            let sender = rt.frames.clone();
            let task = tokio::spawn(async move {
                while let Some(frame) = feed.next().await {
                    let seq = frame.seq;
                    sender.send_replace(Some(Arc::new(frame)));
                    feed.ack(seq);
                }
            });
            rt.st().tasks.push(task);
        }
        Ok(())
    }
}

/// Watches one session's browser: crashes, dialogs, blocked hosts, memory,
/// idle screencast and whether the app still answers.
async fn monitor(
    ctx: Arc<DaemonContext>,
    session: DesignSessionId,
    mut events: tokio::sync::broadcast::Receiver<adb::BrowserEvent>,
) {
    let service = ctx.design.clone();
    let rt = service.runtime(&session);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(adb::BrowserEvent::Crashed | adb::BrowserEvent::Exited) => {
                    tracing::warn!(%session, "design: the preview browser crashed");
                    if let Err(error) = service.relaunch(&ctx, &session, true).await {
                        tracing::warn!(%error, "relaunching the preview browser failed");
                    }
                    return;
                }
                Ok(adb::BrowserEvent::Dialog { kind, message, default_prompt }) => {
                    let _ = rt.events.send(DesignEvent::Dialog { kind, message, default_prompt });
                }
                Ok(adb::BrowserEvent::BlockedHost { host }) => {
                    let hosts = {
                        let mut st = rt.st();
                        st.counters.blocked.insert(host);
                        st.counters.blocked.iter().cloned().collect::<Vec<_>>()
                    };
                    let _ = rt.events.send(DesignEvent::BlockedHosts { hosts });
                }
                Ok(adb::BrowserEvent::PopupClosed { url }) => {
                    let message = match archon_core::design::validate_preview_url(&url) {
                        Ok(_) => format!("A pop-up was closed. Open {url} here instead?"),
                        Err(_) => "A pop-up was closed.".to_owned(),
                    };
                    let _ = rt.events.send(DesignEvent::Toast { message });
                }
                Ok(adb::BrowserEvent::FileChooserRefused) => {
                    let _ = rt.events.send(DesignEvent::Toast {
                        message: "File pickers aren't available in the preview yet".into(),
                    });
                }
                Ok(adb::BrowserEvent::NavigationFailed { url, .. }) => {
                    service.mark_degraded(&ctx, &session, true, Some(&url)).await;
                }
                Ok(adb::BrowserEvent::Loaded { .. }) => {
                    service.mark_degraded(&ctx, &session, false, None).await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if !service.tick(&ctx, &session, &rt).await {
                    return;
                }
            }
        }
    }
}

impl DesignService {
    /// One monitor tick; false when the monitor should end.
    async fn tick(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        rt: &Arc<Runtime>,
    ) -> bool {
        let (browser, idle) = {
            let st = rt.st();
            let idle = st.screencast
                && st.subscribers == 0
                && st.last_sub_gone.is_some_and(|at| at.elapsed() >= IDLE_STOP);
            (st.browser.clone(), idle)
        };
        let Some(browser) = browser else {
            return false;
        };
        if idle {
            let _ = browser.stop_screencast().await;
            rt.st().screencast = false;
        }
        let restart = {
            let mut st = rt.st();
            match browser.rss_bytes() {
                Some(rss) if rss > RSS_LIMIT => {
                    let since = *st.rss_over_since.get_or_insert_with(Instant::now);
                    since.elapsed() >= RSS_HOLD
                }
                _ => {
                    st.rss_over_since = None;
                    false
                }
            }
        };
        if restart {
            tracing::warn!(%session, "design: the preview browser used over 2 GiB for 30 s; restarting it");
            if let Err(error) = self.relaunch(ctx, session, false).await {
                tracing::warn!(%error, "restarting the preview browser failed");
            }
            return false;
        }
        // Is the app answering?
        let url = rt.st().last_url.clone();
        if let Some(url) = url {
            let reachable = probe(&url).await;
            self.mark_degraded(ctx, session, !reachable, Some(&url))
                .await;
        }
        true
    }
}

/// Whether something accepts connections at the preview's loopback address.
pub(crate) async fn probe(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    // `host:port` or `[v6]:port` as given; a bare host or `[v6]` gets port 80.
    let has_port = if authority.starts_with('[') {
        authority.contains("]:")
    } else {
        authority.contains(':')
    };
    let target = if has_port {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(&target)
        )
        .await,
        Ok(Ok(_))
    )
}
