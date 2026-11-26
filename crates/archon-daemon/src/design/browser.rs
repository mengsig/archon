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
const RSS_EMERGENCY: u64 = 3 * 1024 * 1024 * 1024;
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
    /// Check whether the attached preview server answers at the registered URL.
    async fn probe(&self, url: &str) -> bool;
    async fn reload(&self) -> Result<(), adb::BrowserError>;
    async fn set_viewport(&self, viewport: adb::Viewport) -> Result<(), adb::BrowserError>;
    async fn input(&self, events: &[adb::Input]) -> Result<(), adb::BrowserError>;
    /// A batch failed or was refused before it ran: whatever it would have let
    /// go is released before the next batch, or by [`Self::reconcile_input`].
    fn input_lost(&self);
    /// The idle cleanup after a lost batch: let go of everything still held.
    /// True when nothing a lost batch may have left is held any more.
    async fn reconcile_input(&self) -> bool;
    async fn start_screencast(&self) -> Result<Box<dyn FrameFeed>, adb::BrowserError>;
    async fn stop_screencast(&self) -> Result<(), adb::BrowserError>;
    async fn capture(
        &self,
        clip: Option<adb::ClipRect>,
        dpr: f64,
    ) -> Result<adb::Picture, adb::BrowserError>;
    async fn resolve(&self, x: f64, y: f64, depth: i32)
    -> Result<adb::Resolved, adb::BrowserError>;
    /// The box of the element `selector` names; of several matches, the one
    /// nearest `near` (the box the note recorded), else the first.
    async fn resolve_selector(
        &self,
        selector: &str,
        near: Option<adb::ClipRect>,
    ) -> Result<Option<adb::ClipRect>, adb::BrowserError>;
    async fn current_path(&self) -> Result<String, adb::BrowserError>;
    async fn current_location(&self) -> Result<String, adb::BrowserError> {
        self.current_path().await
    }
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
    /// The JavaScript dialog the page has open and nobody has answered.
    fn open_dialog(&self) -> Option<adb::BrowserEvent> {
        None
    }
    /// Set files on the file input whose chooser the page opened.
    async fn set_files(&self, _node: i64, _files: &[PathBuf]) -> Result<(), adb::BrowserError> {
        Err(adb::BrowserError::Protocol(
            "this preview cannot take files".into(),
        ))
    }
    /// Drop files on the page at a point, in CSS px.
    async fn drop_files(
        &self,
        _x: f64,
        _y: f64,
        _files: &[PathBuf],
    ) -> Result<(), adb::BrowserError> {
        Err(adb::BrowserError::Protocol(
            "this preview cannot take files".into(),
        ))
    }
    /// The `accept` of the file input at a point, in CSS px; `None` when
    /// the point is not on a file input.
    async fn accept_at(&self, _x: f64, _y: f64) -> Result<Option<String>, adb::BrowserError> {
        Ok(None)
    }
    /// Answer the page's native picker; true when the page took the answer.
    async fn answer_picker(
        &self,
        _id: u64,
        _value: Option<&str>,
    ) -> Result<bool, adb::BrowserError> {
        Err(adb::BrowserError::Protocol(
            "this preview has no pickers".into(),
        ))
    }
    /// Back one page; false when there is none.
    async fn go_back(&self) -> Result<bool, adb::BrowserError> {
        Err(adb::BrowserError::Protocol(
            "this preview has no history".into(),
        ))
    }
    /// Reload, wait until the page has loaded, then until it has settled (no
    /// request in flight, no pixel still moving). `false` when `max` passed.
    async fn reload_settled(&self, max: Duration) -> Result<bool, adb::BrowserError> {
        self.reload().await?;
        self.wait_settled(max).await
    }
    /// Navigate, wait until the page has loaded, then until it has settled.
    async fn navigate_settled(&self, url: &str, max: Duration) -> Result<bool, adb::BrowserError> {
        self.navigate(url).await?;
        self.wait_settled(max).await
    }
    /// Wait until the page has stopped changing; `false` when `max` passed.
    async fn wait_settled(&self, max: Duration) -> Result<bool, adb::BrowserError> {
        self.wait_network_idle(Duration::from_millis(400), max)
            .await
    }
    /// The centre of the element `selector` matches, scrolled into view.
    async fn center_of(&self, selector: &str) -> Result<Option<(f64, f64)>, adb::BrowserError> {
        Ok(self
            .resolve_selector(selector, None)
            .await?
            .map(|r| (r.x + r.w / 2.0, r.y + r.h / 2.0)))
    }
    /// Whether two pictures, each `(mime, bytes)`, show the same thing.
    async fn pictures_match(
        &self,
        a: (&str, &[u8]),
        b: (&str, &[u8]),
    ) -> Result<bool, adb::BrowserError> {
        Ok(a.1 == b.1)
    }
    fn containment_reason(&self) -> Option<String> {
        None
    }
    fn events(&self) -> broadcast::Receiver<adb::BrowserEvent>;
    fn rss_bytes(&self) -> Option<u64>;
    fn terminate(&self) -> std::io::Result<()>;
    async fn close(&self);
}

/// Records a child's kernel identity while its launch pipe still holds it.
#[derive(Debug)]
pub struct BrowserRegistration<'a> {
    ctx: &'a DaemonContext,
    session: &'a DesignSessionId,
}

impl BrowserRegistration<'_> {
    /// The launcher calls this once for the new process before starting Chromium.
    pub async fn record(&self, pid: u32, started: u64) -> Result<(), adb::BrowserError> {
        if pid == 0 || started == 0 {
            return Err(adb::BrowserError::Launch(
                "missing preview process identity".into(),
            ));
        }
        let patch = archon_queue::store::SessionPatch {
            browser: Some(Some((pid, started))),
            ..Default::default()
        };
        self.ctx
            .store
            .lock()
            .await
            .design_session_patch(self.session, patch)
            .map(|_| ())
            .map_err(|e| adb::BrowserError::Launch(format!("recording the preview browser: {e}")))
    }
}

/// How browsers are found and started; tests replace it.
#[async_trait::async_trait]
pub trait BrowserFactory: Send + Sync {
    /// The browser binary, or the driver's refusal with its install hint.
    fn binary(&self, configured: Option<&std::path::Path>) -> Result<PathBuf, adb::BrowserError>;
    /// Verify ownership of the registered preview server and prepare socket checks.
    async fn verify_server(
        &self,
        worktree: &std::path::Path,
        url: &str,
    ) -> Result<Arc<dyn adb::UpstreamGuard>, ArchonError> {
        super::server_origin::verify(worktree, url).await
    }
    async fn launch(
        &self,
        spec: adb::LaunchSpec,
        registration: &BrowserRegistration<'_>,
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
        registration: &BrowserRegistration<'_>,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError> {
        let browser =
            adb::Browser::launch_recorded(spec, |pid, started| registration.record(pid, started))
                .await?;
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
    async fn probe(&self, url: &str) -> bool {
        match self.get() {
            Ok(browser) => browser.probe(url).await,
            Err(_) => false,
        }
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
    fn input_lost(&self) {
        // A closed browser took everything it held with it.
        if let Ok(browser) = self.get() {
            browser.input_lost();
        }
    }
    async fn reconcile_input(&self) -> bool {
        match self.get() {
            Ok(browser) => browser.reconcile_input().await,
            Err(_) => true,
        }
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
        near: Option<adb::ClipRect>,
    ) -> Result<Option<adb::ClipRect>, adb::BrowserError> {
        self.get()?.resolve_selector(selector, near).await
    }
    async fn current_location(&self) -> Result<String, adb::BrowserError> {
        self.get()?.current_location().await
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
    fn open_dialog(&self) -> Option<adb::BrowserEvent> {
        self.get().ok().and_then(|browser| browser.open_dialog())
    }
    async fn set_files(&self, node: i64, files: &[PathBuf]) -> Result<(), adb::BrowserError> {
        self.get()?.set_files(node, files).await
    }
    async fn drop_files(&self, x: f64, y: f64, files: &[PathBuf]) -> Result<(), adb::BrowserError> {
        self.get()?.drop_files(x, y, files).await
    }
    async fn accept_at(&self, x: f64, y: f64) -> Result<Option<String>, adb::BrowserError> {
        self.get()?.accept_at(x, y).await
    }
    async fn answer_picker(&self, id: u64, value: Option<&str>) -> Result<bool, adb::BrowserError> {
        self.get()?.answer_picker(id, value).await
    }
    async fn go_back(&self) -> Result<bool, adb::BrowserError> {
        self.get()?.go_back().await
    }
    async fn reload_settled(&self, max: Duration) -> Result<bool, adb::BrowserError> {
        self.get()?.reload_settled(max).await
    }
    async fn navigate_settled(&self, url: &str, max: Duration) -> Result<bool, adb::BrowserError> {
        self.get()?.navigate_settled(url, max).await
    }
    async fn wait_settled(&self, max: Duration) -> Result<bool, adb::BrowserError> {
        self.get()?.wait_settled(max).await
    }
    async fn center_of(&self, selector: &str) -> Result<Option<(f64, f64)>, adb::BrowserError> {
        self.get()?.center_of(selector).await
    }
    async fn pictures_match(
        &self,
        a: (&str, &[u8]),
        b: (&str, &[u8]),
    ) -> Result<bool, adb::BrowserError> {
        self.get()?.pictures_match(a, b).await
    }
    fn containment_reason(&self) -> Option<String> {
        self.get()
            .ok()
            .and_then(|browser| browser.containment_reason())
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
    fn terminate(&self) -> std::io::Result<()> {
        let Some((pid, started)) = self.pid().zip(self.started_at()) else {
            return Ok(());
        };
        adb::kill_if_matches(pid, started).map(drop)
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
        if let Some((pid, started)) = arc.pid().zip(arc.started_at())
            && let Err(error) = adb::kill_if_matches(pid, started)
        {
            tracing::error!(%error, pid, "failed to terminate a busy preview browser");
        }
    }
}

/// What the service keeps of one session's browser.
pub(crate) struct Runtime {
    pub events: broadcast::Sender<DesignEvent>,
    pub frames: watch::Sender<Option<Arc<adb::Frame>>>,
    pub state: std::sync::Mutex<RtState>,
    /// Serializes launch, relaunch and close.
    pub gate: tokio::sync::Mutex<()>,
    pub delivery: tokio::sync::Mutex<()>,
    /// Uploads, the open file chooser and downloads of the running browser.
    pub files: tokio::sync::Mutex<super::files::Transfers>,
}

/// A reserved slot; releasing it on drop covers every error path.
struct SlotGuard<'a> {
    runtime: &'a Runtime,
    service: &'a DesignService,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let freed = {
            let mut state = self.runtime.st();
            debug_assert!(state.launching);
            state.launching = false;
            if state.browser.is_none() && state.status == BrowserStatus::Starting {
                state.status = BrowserStatus::Idle;
            }
            state.browser.is_none()
        };
        if freed && self.service.enabled() {
            self.service.spawn_start_queued();
        }
    }
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
    frame_seq: u64,
    containment: Option<String>,
    feeder: Option<tokio::task::JoinHandle<()>>,
    monitor: Option<tokio::task::JoinHandle<()>>,
    pub last_sub_gone: Option<Instant>,
    pub crashes: VecDeque<Instant>,
    pub last_url: Option<String>,
    rss_over_since: Option<Instant>,
    memory_restarts: usize,
    pub counters: Counters,
    pub pending_input: Option<Instant>,
    pub input_window: (Instant, u32),
    pub resolve_window: (Instant, u32),
    /// A launch holds this session's slot before its browser exists.
    pub launching: bool,
    /// Health probes missed in a row; one slow answer is not "down".
    pub probe_misses: u32,
    /// Streams of this runtime that have ended: see [`AdmittedInput`].
    streams_ended: u64,
}

impl Default for RtState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            browser: None,
            status: BrowserStatus::Idle,
            subscribers: 0,
            frame_seq: 0,
            containment: None,
            feeder: None,
            monitor: None,
            last_sub_gone: None,
            crashes: VecDeque::new(),
            last_url: None,
            rss_over_since: None,
            memory_restarts: 0,
            counters: Counters::default(),
            pending_input: None,
            input_window: (now, 0),
            resolve_window: (now, 0),
            launching: false,
            probe_misses: 0,
            streams_ended: 0,
        }
    }
}

/// An input request read off a connection, held until it ends. One read
/// before a stream ended may press after that end let go, so it lets go again.
pub struct AdmittedInput {
    service: Arc<DesignService>,
    session: DesignSessionId,
    /// The session's runtime when it was read, and its streams ended by then.
    seen: Option<(Arc<Runtime>, u64)>,
}

impl std::fmt::Debug for AdmittedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedInput")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Drop for AdmittedInput {
    fn drop(&mut self) {
        let Some(now) = self.service.existing_runtime(&self.session) else {
            return;
        };
        let browser = {
            let st = now.st();
            let ended_since = match &self.seen {
                Some((then, ended)) if Arc::ptr_eq(then, &now) => {
                    debug_assert!(st.streams_ended >= *ended, "stream ends only add up");
                    st.streams_ended > *ended
                }
                // Made after this was read: each stream it ended came later.
                _ => st.streams_ended > 0,
            };
            if !ended_since {
                return;
            }
            st.browser.clone()
        };
        if let Some(browser) = browser {
            browser.input_lost();
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
            delivery: tokio::sync::Mutex::new(()),
            files: tokio::sync::Mutex::default(),
        }
    }

    pub fn st(&self) -> std::sync::MutexGuard<'_, RtState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish_browser(
        &self,
        ctx: &DaemonContext,
        row: &archon_core::design::DesignSession,
        url: &str,
        browser: Arc<dyn PreviewBrowser>,
        events: broadcast::Receiver<adb::BrowserEvent>,
    ) {
        let pid = browser.pid();
        let mut state = self.st();
        debug_assert!(state.launching && state.browser.is_none());
        debug_assert!(state.monitor.is_none());
        state.browser = Some(browser);
        state.last_url = Some(url.to_owned());
        state.status = if row.sandbox_off {
            BrowserStatus::Unsandboxed
        } else {
            BrowserStatus::Running
        };
        state.monitor = Some(tokio::spawn(monitor(ctx.arc(), row.id.clone(), events)));
        tracing::info!(session = %row.id, ?pid, sandbox = !row.sandbox_off, device = ?super::current_device_label(), "design: browser started");
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
        all.values()
            .filter(|rt| {
                let st = rt.st();
                st.browser.is_some() || st.launching
            })
            .count()
    }

    /// Take a slot for `rt`'s launch, atomically with the count: the map lock
    /// is held from the count to the mark, so two launches never both see a
    /// free slot. `None` when all are taken.
    fn reserve_slot<'a>(&'a self, rt: &'a Runtime) -> Option<SlotGuard<'a>> {
        let all = self.rt.lock().unwrap_or_else(|e| e.into_inner());
        let taken = all
            .values()
            .filter(|other| {
                let st = other.st();
                st.browser.is_some() || st.launching
            })
            .count();
        if taken >= self.cfg.max_sessions as usize {
            rt.st().status = BrowserStatus::QueuedForSlot;
            return None;
        }
        let mut state = rt.st();
        state.launching = true;
        state.status = BrowserStatus::Starting;
        Some(SlotGuard {
            service: self,
            runtime: rt,
        })
    }

    /// A slot was freed: start a queued session's browser, off this path.
    pub(crate) fn spawn_start_queued(&self) {
        if let Some(arc) = self.arc() {
            tokio::spawn(async move { arc.design.start_queued(&arc).await });
        }
    }

    pub(crate) fn profile_dir(&self, session: &DesignSessionId) -> PathBuf {
        self.data_dir
            .join("design")
            .join("profiles")
            .join(session.as_str())
    }

    fn remove_profile(&self, session: &DesignSessionId) {
        if let Err(error) = std::fs::remove_dir_all(self.profile_dir(session))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, %session, "profile cleanup deferred to daemon startup");
        }
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
        if let Some(browser) = self.running_browser(&rt)? {
            return Ok(Some(browser));
        }
        let Some((row, url)) = self.launch_candidate(ctx, session, &rt).await? else {
            return Ok(None);
        };
        let Some(slot) = self.reserve_slot(&rt) else {
            return Ok(None);
        };
        let upstream_guard = self.verify_registered_preview(ctx, &row, &url).await?;
        let launched = self.launch_into(ctx, &rt, &row, &url, upstream_guard).await;
        drop(slot);
        match launched {
            Ok(browser) => Ok(Some(browser)),
            Err(adb::BrowserError::Containment(reason)) => {
                rt.st().containment = Some(format!("{CONTAINMENT_END}: {reason}"));
                drop(_gate);
                end_for_containment(ctx, session, &rt, &reason).await;
                Err(ArchonError::Forbidden(format!(
                    "{CONTAINMENT_END}: {reason}"
                )))
            }
            Err(adb::BrowserError::NotFound { hint }) => {
                rt.st().status = BrowserStatus::Missing { hint };
                Ok(None)
            }
            Err(adb::BrowserError::SandboxUnavailable { hint }) => {
                rt.st().status = BrowserStatus::SandboxUnavailable { hint };
                Ok(None)
            }
            Err(error) => {
                rt.st().status = BrowserStatus::Idle;
                Err(ArchonError::Unavailable(format!(
                    "the preview browser did not start: {error}"
                )))
            }
        }
    }

    fn running_browser(
        &self,
        rt: &Runtime,
    ) -> Result<Option<Arc<dyn PreviewBrowser>>, ArchonError> {
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ArchonError::Unavailable(
                "the daemon is shutting down".into(),
            ));
        }
        if let Some(reason) = rt.st().containment.clone() {
            return Err(ArchonError::Forbidden(reason));
        }
        if let Some(browser) = rt.browser() {
            if let Some(reason) = browser.containment_reason() {
                return Err(ArchonError::Forbidden(format!(
                    "{CONTAINMENT_END}: {reason}"
                )));
            }
            return Ok(Some(browser));
        }
        Ok(None)
    }

    async fn launch_candidate(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        rt: &Runtime,
    ) -> Result<Option<(archon_core::design::DesignSession, String)>, ArchonError> {
        let row = ctx.store.lock().await.design_session(session)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if matches!(
            row.state,
            archon_core::design::SessionState::Ended | archon_core::design::SessionState::Confirmed
        ) {
            return Ok(None);
        }
        let Some(url) = rt.st().last_url.clone().or(row.preview_url.clone()) else {
            return Ok(None);
        };
        if rt.st().status == BrowserStatus::CrashedTooOften {
            return Ok(None);
        }
        Ok(Some((row, url)))
    }

    pub(super) async fn reload_preview(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<(), ArchonError> {
        let rt = self.runtime(session);
        {
            let _gate = rt.gate.lock().await;
            let mut state = rt.st();
            if state.status == BrowserStatus::CrashedTooOften {
                state.crashes.clear();
                state.status = BrowserStatus::Idle;
            }
        }
        let browser = self.ensure_browser(ctx, session).await?.ok_or_else(|| {
            ArchonError::Unavailable(
                "the preview browser is unavailable or waiting for a slot".into(),
            )
        })?;
        browser
            .reload()
            .await
            .map_err(|err| ArchonError::Unavailable(format!("reload preview: {err}")))?;
        if rt.st().subscribers > 0 {
            self.subscriber_joined(ctx, session).await?;
        }
        Ok(())
    }

    async fn verify_registered_preview(
        &self,
        ctx: &DaemonContext,
        row: &archon_core::design::DesignSession,
        url: &str,
    ) -> Result<Arc<dyn adb::UpstreamGuard>, ArchonError> {
        let wt = self.worktree_of(ctx, row).await?;
        self.factory().verify_server(&wt.worktree, url).await
    }

    async fn launch_into(
        &self,
        ctx: &DaemonContext,
        rt: &Arc<Runtime>,
        row: &archon_core::design::DesignSession,
        url: &str,
        upstream_guard: Arc<dyn adb::UpstreamGuard>,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError> {
        let factory = self.factory();
        let binary = factory.binary(self.cfg.browser.as_deref())?;
        let root = self.profile_dir(&row.id);
        let profile = root.join(ulid::Ulid::generate().to_string());
        // The new browser starts with no uploads: removing the root deletes them.
        *rt.files.lock().await = super::files::Transfers::default();
        match std::fs::remove_dir_all(&root) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(adb::BrowserError::Launch(format!(
                    "remove stale preview profile: {error}"
                )));
            }
            _ => {}
        }
        if let Some(parent) = profile.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| adb::BrowserError::Launch(e.to_string()))?;
        }
        let (_, viewport) = to_adb_viewport(row.viewport_preset);
        let spec = adb::LaunchSpec {
            binary,
            profile_dir: profile,
            sandbox: !row.sandbox_off,
            preview_url: url.to_owned(),
            upstream_guard,
            viewport,
            path_env: std::env::var("PATH").unwrap_or_default(),
            lang: "en-US".into(),
            tz: "UTC".into(),
        };
        let registration = BrowserRegistration {
            ctx,
            session: &row.id,
        };
        let browser = factory.launch(spec, &registration).await?;
        let events = browser.events();
        let navigation = browser.navigate(url).await;
        if let Some(reason) = browser.containment_reason() {
            browser.close().await;
            return Err(adb::BrowserError::Containment(reason));
        }
        if let Err(error) = navigation {
            browser.close().await;
            return Err(error);
        }
        rt.publish_browser(ctx, row, url, browser.clone(), events);
        self.emit_session(ctx, &row.id).await;
        Ok(browser)
    }

    /// Start frames for a subscriber; the feeder runs while any is attached.
    pub(crate) async fn subscriber_joined(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<(), ArchonError> {
        let Some(browser) = self.ensure_browser(ctx, session).await? else {
            return Ok(());
        };
        let rt = self.runtime(session);
        let _gate = rt.gate.lock().await;
        if !rt
            .browser()
            .is_some_and(|current| Arc::ptr_eq(&current, &browser))
        {
            return Ok(());
        }
        {
            let mut state = rt.st();
            state.last_sub_gone = None;
            if state.subscribers == 0 || state.feeder.as_ref().is_some_and(|f| !f.is_finished()) {
                return Ok(());
            }
        }
        let mut feed = browser.start_screencast().await.map_err(|error| {
            ArchonError::Unavailable(format!("the preview stream did not start: {error}"))
        })?;
        let runtime = Arc::clone(&rt);
        let task = tokio::spawn(async move {
            while let Some(mut frame) = feed.next().await {
                let original_seq = frame.seq;
                let mut state = runtime.st();
                if !state
                    .browser
                    .as_ref()
                    .is_some_and(|b| Arc::ptr_eq(b, &browser))
                    || state.containment.is_some()
                    || browser.containment_reason().is_some()
                {
                    break;
                }
                state.frame_seq = original_seq.max(state.frame_seq.saturating_add(1));
                frame.seq = state.frame_seq;
                runtime.frames.send_replace(Some(Arc::new(frame)));
                feed.ack(original_seq);
            }
        });
        rt.st().feeder = Some(task);
        Ok(())
    }

    /// Start the browsers of sessions queued for a slot that someone still
    /// watches, oldest request first, while slots are free: run after a
    /// session's browser closed, so "starts as soon as one is free" holds.
    pub(crate) async fn start_queued(&self, ctx: &DaemonContext) {
        let queued: Vec<DesignSessionId> = {
            let all = self.rt.lock().unwrap_or_else(|e| e.into_inner());
            let mut queued: Vec<_> = all
                .iter()
                .filter(|(_, rt)| {
                    let st = rt.st();
                    st.status == BrowserStatus::QueuedForSlot && st.subscribers > 0
                })
                .map(|(id, _)| id.clone())
                .collect();
            // Session ids are ULIDs: their order is their age.
            queued.sort();
            queued
        };
        for session in queued {
            if self.live_browsers() >= self.cfg.max_sessions as usize {
                break;
            }
            if let Err(error) = self.subscriber_joined(ctx, &session).await {
                tracing::warn!(%error, %session, "design: a queued preview did not start");
            }
        }
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

    /// Note an input request for `session` as read, before the connection
    /// reads on; dropped once it ends, after any press it made.
    pub fn input_admitted(self: &Arc<Self>, session: &DesignSessionId) -> AdmittedInput {
        let seen = self.existing_runtime(session).map(|rt| {
            let ended = rt.st().streams_ended;
            (rt, ended)
        });
        AdmittedInput {
            service: Arc::clone(self),
            session: session.clone(),
            seen,
        }
    }

    /// A stream ended: its page closed, reset or lost its connection, and any
    /// input it still had queued, a release among it, will not come.
    pub(crate) fn subscriber_left(&self, session: &DesignSessionId) {
        let Some(rt) = self.existing_runtime(session) else {
            return;
        };
        let browser = {
            let mut st = rt.st();
            st.subscribers = st.subscribers.saturating_sub(1);
            if st.subscribers == 0 {
                st.last_sub_gone = Some(Instant::now());
            }
            st.streams_ended = st.streams_ended.saturating_add(1);
            st.browser.clone()
        };
        if let Some(browser) = browser {
            browser.input_lost();
        }
    }

    /// Close the session's browser, delete its profile, log its counters and
    /// forget its runtime: for a session that has ended.
    pub(crate) async fn close_session_browser(&self, session: &DesignSessionId, reason: &str) {
        self.stop_browser(session, reason, true).await;
    }

    /// Close the session's browser and delete its profile. Unless `forget`,
    /// the runtime stays: open streams of a confirmed session hold its event
    /// sender, and must still get its later state and its end.
    pub(crate) async fn stop_browser(&self, session: &DesignSessionId, reason: &str, forget: bool) {
        let Some(rt) = self.existing_runtime(session) else {
            self.remove_profile(session);
            return;
        };
        let _gate = rt.gate.lock().await;
        let (browser, counters) = {
            let mut st = rt.st();
            stop_tasks(&mut st);
            rt.frames.send_replace(None);
            let counters = std::mem::take(&mut st.counters);
            st.status = BrowserStatus::Idle;
            // A relaunched browser starts its preview probe afresh: misses
            // the stopped one counted must not mark the new one down.
            st.probe_misses = 0;
            (st.browser.take(), counters)
        };
        if let Some(browser) = browser {
            browser.close().await;
            tracing::info!(%session, reason, "design: browser stopped");
        }
        self.remove_profile(session);
        *rt.files.lock().await = super::files::Transfers::default();
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
        if forget {
            self.rt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(session);
        }
    }

    async fn relaunch(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
    ) -> Result<(), ArchonError> {
        let rt = self.runtime(session);
        let gave_up = {
            let _gate = rt.gate.lock().await;
            let (gave_up, old) = {
                let mut st = rt.st();
                let now = Instant::now();
                st.crashes
                    .retain(|at| now.duration_since(*at) <= CRASH_WINDOW);
                st.crashes.push_back(now);
                st.counters.crashes += 1;
                let gave_up = st.crashes.len() > MAX_CRASHES;
                if gave_up {
                    st.status = BrowserStatus::CrashedTooOften;
                }
                st.rss_over_since = None;
                st.probe_misses = 0;
                stop_tasks(&mut st);
                rt.frames.send_replace(None);
                (gave_up, st.browser.take())
            };
            if let Some(old) = old {
                old.close().await;
            }
            gave_up
        };
        if gave_up {
            self.emit_session(ctx, session).await;
            self.spawn_start_queued();
        } else if rt.st().subscribers > 0 {
            self.subscriber_joined(ctx, session).await?;
        } else {
            self.ensure_browser(ctx, session).await?;
        }
        Ok(())
    }
}

fn stop_tasks(state: &mut RtState) {
    for task in [state.feeder.take(), state.monitor.take()]
        .into_iter()
        .flatten()
    {
        if Some(task.id()) != tokio::task::try_id() {
            task.abort();
        }
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
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if let Some(reason) = rt.browser().and_then(|b| b.containment_reason()) {
            end_for_containment(&ctx, &session, &rt, &reason).await;
            return;
        }
        tokio::select! {
            event = events.recv() => match event {
                Ok(adb::BrowserEvent::ContainmentStopped { reason }) => {
                    end_for_containment(&ctx, &session, &rt, &reason).await;
                    return;
                }
                Ok(adb::BrowserEvent::Crashed | adb::BrowserEvent::Exited) => {
                    if let Some(reason) = rt.browser().and_then(|b| b.containment_reason()) {
                        end_for_containment(&ctx, &session, &rt, &reason).await;
                        return;
                    }
                    tracing::warn!(%session, "design: the preview browser crashed");
                    if let Err(error) = service.relaunch(&ctx, &session).await {
                        tracing::warn!(%error, "relaunching the preview browser failed");
                    }
                    return;
                }
                Ok(event) => report_browser_event(&ctx, &session, &rt, event).await,
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

/// The operator's view of a dialog the page opened.
pub(crate) fn dialog_event(dialog: adb::BrowserEvent) -> DesignEvent {
    let adb::BrowserEvent::Dialog {
        kind,
        message,
        default_prompt,
    } = dialog
    else {
        unreachable!("only a dialog is shown as one: {dialog:?}");
    };
    debug_assert!(!kind.is_empty(), "a dialog has a kind");
    DesignEvent::Dialog {
        kind,
        message,
        default_prompt,
    }
}

async fn report_browser_event(
    ctx: &DaemonContext,
    session: &DesignSessionId,
    rt: &Runtime,
    event: adb::BrowserEvent,
) {
    match event {
        dialog @ adb::BrowserEvent::Dialog { .. } => {
            let _ = rt.events.send(dialog_event(dialog));
        }
        adb::BrowserEvent::BlockedHost { host } => {
            let hosts = {
                let mut st = rt.st();
                if st.counters.blocked.len() >= 64 || !st.counters.blocked.insert(host) {
                    return;
                }
                st.counters.blocked.iter().cloned().collect::<Vec<_>>()
            };
            let _ = rt.events.send(DesignEvent::BlockedHosts { hosts });
        }
        adb::BrowserEvent::PopupClosed { .. } => {
            let message = "A pop-up was closed.".to_owned();
            let _ = rt.events.send(DesignEvent::Toast { message });
        }
        adb::BrowserEvent::FileChooser { .. }
        | adb::BrowserEvent::FileChooserFailed { .. }
        | adb::BrowserEvent::Downloaded { .. }
        | adb::BrowserEvent::DownloadFailed { .. }
        | adb::BrowserEvent::Picker(_)
        | adb::BrowserEvent::Copied { .. }
        | adb::BrowserEvent::NewTab { .. } => {
            let event = super::transfer::report(rt, event).await;
            let event = event.expect("every file, picker and tab event has a stream event");
            let _ = rt.events.send(event);
        }
        adb::BrowserEvent::NavigationFailed { .. } => {
            ctx.design.record_health(ctx, session, true).await;
        }
        adb::BrowserEvent::Loaded { .. } => {
            ctx.design.record_health(ctx, session, false).await;
        }
        adb::BrowserEvent::ContainmentStopped { .. }
        | adb::BrowserEvent::Crashed
        | adb::BrowserEvent::Exited => {
            unreachable!("the monitor handles terminal browser events before forwarding")
        }
    }
}

pub(super) const CONTAINMENT_END: &str = "Preview stopped for containment";

async fn end_for_containment(
    ctx: &DaemonContext,
    session: &DesignSessionId,
    rt: &Runtime,
    reason: &str,
) {
    let reason = format!("{CONTAINMENT_END}: {reason}");
    {
        let mut state = rt.st();
        state.containment = Some(reason.clone());
        rt.frames.send_replace(None);
    }
    ctx.design.stop_browser(session, &reason, false).await;
    tracing::warn!(%session, %reason, "design: preview terminated");
    let result = async {
        let Some(row) = ctx.store.lock().await.design_session(session)? else {
            return Ok(());
        };
        ctx.design.end_session(ctx, &row, &reason).await
    }
    .await;
    if let Err(error) = result {
        tracing::error!(%session, %error, "failed to record the containment stop");
    }
}

impl DesignService {
    async fn record_health(&self, ctx: &DaemonContext, session: &DesignSessionId, degraded: bool) {
        if let Err(error) = self.mark_degraded(ctx, session, degraded).await {
            tracing::warn!(%error, %session, "recording preview health failed");
        }
    }

    async fn memory_stop(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        rt: &Runtime,
        rss: Option<u64>,
    ) {
        let attempts = {
            let mut state = rt.st();
            state.memory_restarts += 1;
            state.memory_restarts
        };
        tracing::warn!(%session, ?rss, attempts, "preview exceeded its memory limit");
        if attempts > MAX_CRASHES {
            end_for_containment(
                ctx,
                session,
                rt,
                "the browser exceeded its memory limit after five automatic restarts",
            )
            .await;
        } else if let Err(error) = self.relaunch(ctx, session).await {
            tracing::warn!(%error, %session, "restarting the preview browser failed");
        }
    }

    /// One monitor tick; false when the monitor should end.
    async fn tick(
        &self,
        ctx: &DaemonContext,
        session: &DesignSessionId,
        rt: &Arc<Runtime>,
    ) -> bool {
        let (browser, idle) = {
            let st = rt.st();
            let idle = st.feeder.is_some()
                && st.subscribers == 0
                && st.last_sub_gone.is_some_and(|at| at.elapsed() >= IDLE_STOP);
            (st.browser.clone(), idle)
        };
        let Some(browser) = browser else {
            return false;
        };
        if idle {
            let _gate = rt.gate.lock().await;
            if rt.st().subscribers == 0 {
                if let Err(error) = browser.stop_screencast().await {
                    tracing::warn!(%error, %session, "stopping the idle preview stream failed");
                }
                if let Some(feeder) = rt.st().feeder.take() {
                    feeder.abort();
                }
            }
        }
        // A lost batch's held input is let go within a tick, even if no batch follows.
        if !browser.reconcile_input().await {
            tracing::debug!(%session, "held preview input is still owed a release");
        }
        let rss = browser.rss_bytes();
        if memory_limit_reached(rss, &mut rt.st().rss_over_since, Instant::now()) {
            if let Err(error) = browser.terminate() {
                tracing::error!(%session, %error, "signalling the over-budget preview failed; closing it");
            }
            self.memory_stop(ctx, session, rt, rss).await;
            return false;
        }
        // Is the app answering?
        let url = rt.st().last_url.clone();
        if let Some(url) = url {
            let reachable = browser.probe(&url).await;
            // A dev server compiling a route can miss one probe; two misses
            // in a row (about 3 s apart) mean the preview is down.
            let down = {
                let mut st = rt.st();
                st.probe_misses = if reachable {
                    0
                } else {
                    st.probe_misses.saturating_add(1)
                };
                st.probe_misses >= PROBE_MISSES_DOWN
            };
            if reachable || down {
                self.record_health(ctx, session, down).await;
            }
        }
        true
    }
}

fn memory_limit_reached(rss: Option<u64>, over_since: &mut Option<Instant>, now: Instant) -> bool {
    let Some(rss) = rss.filter(|rss| *rss > RSS_LIMIT) else {
        *over_since = None;
        return false;
    };
    let since = over_since.get_or_insert(now);
    rss >= RSS_EMERGENCY || now.duration_since(*since) >= RSS_HOLD
}

/// Consecutive failed health probes before a preview is marked down.
const PROBE_MISSES_DOWN: u32 = 2;

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn sustained_and_emergency_samples_have_separate_thresholds() {
        let start = Instant::now();
        let mut since = None;
        let over = Some(RSS_LIMIT + 1);
        assert!(!memory_limit_reached(over, &mut since, start));
        assert!(memory_limit_reached(over, &mut since, start + RSS_HOLD));
        assert!(!memory_limit_reached(
            Some(RSS_LIMIT),
            &mut since,
            start + RSS_HOLD
        ));
        assert_eq!(since, None);
        assert!(memory_limit_reached(
            Some(RSS_EMERGENCY),
            &mut since,
            start + RSS_HOLD
        ));
    }
}
