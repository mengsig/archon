//! A scripted preview browser and a design-session fixture for the
//! design tests. The browser records what the service asked of it; nothing
//! here starts Chromium.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::config::Config;
use archon_core::design::DesignSessionId;
use archon_core::ids::QueueItemId;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::design::{BrowserFactory, FrameFeed, PreviewBrowser, StageEnd, WorktreeRef};
use archon_design_browser as adb;
use tokio::sync::{broadcast, mpsc};

use super::{Harness, git};

/// What the fake browser was asked, in order.
#[derive(Debug, Default)]
pub struct Calls {
    pub navigated: Vec<String>,
    pub reloads: u32,
    pub viewports: Vec<adb::Viewport>,
    pub inputs: usize,
    pub captures: Vec<(Option<adb::ClipRect>, f64)>,
    pub screencasts_started: u32,
    pub screencasts_stopped: u32,
    pub closed: bool,
}

/// One scripted browser.
pub struct FakeBrowser {
    pub calls: Mutex<Calls>,
    events: broadcast::Sender<adb::BrowserEvent>,
    feed: Mutex<Option<mpsc::UnboundedSender<adb::Frame>>>,
    resolved: Mutex<Option<adb::Resolved>>,
    rss: Mutex<u64>,
    pid: u32,
}

impl FakeBrowser {
    fn new(pid: u32) -> Self {
        Self {
            calls: Mutex::default(),
            events: broadcast::channel(64).0,
            feed: Mutex::default(),
            resolved: Mutex::default(),
            rss: Mutex::new(100 * 1024 * 1024),
            pid,
        }
    }

    /// The page crashed.
    pub fn crash(&self) {
        let _ = self.events.send(adb::BrowserEvent::Crashed);
    }

    /// Push a frame to the screencast.
    pub fn frame(&self, seq: u64) {
        if let Some(feed) = self.feed.lock().unwrap().as_ref() {
            let _ = feed.send(adb::Frame {
                seq,
                jpeg: jpeg(8, 8),
                width: 8,
                height: 8,
            });
        }
    }

    /// What the next `resolve` answers.
    pub fn resolves_to(&self, resolved: adb::Resolved) {
        *self.resolved.lock().unwrap() = Some(resolved);
    }

    pub fn set_rss(&self, bytes: u64) {
        *self.rss.lock().unwrap() = bytes;
    }

    pub fn calls<T>(&self, read: impl FnOnce(&Calls) -> T) -> T {
        read(&self.calls.lock().unwrap())
    }
}

struct Feed(mpsc::UnboundedReceiver<adb::Frame>);

#[async_trait::async_trait]
impl FrameFeed for Feed {
    async fn next(&mut self) -> Option<adb::Frame> {
        self.0.recv().await
    }
    fn ack(&mut self, _seq: u64) {}
}

#[async_trait::async_trait]
impl PreviewBrowser for FakeBrowser {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }
    fn started_at(&self) -> Option<u64> {
        Some(1_000)
    }
    async fn navigate(&self, url: &str) -> Result<(), adb::BrowserError> {
        self.calls.lock().unwrap().navigated.push(url.to_owned());
        Ok(())
    }
    async fn reload(&self) -> Result<(), adb::BrowserError> {
        self.calls.lock().unwrap().reloads += 1;
        Ok(())
    }
    async fn set_viewport(&self, viewport: adb::Viewport) -> Result<(), adb::BrowserError> {
        self.calls.lock().unwrap().viewports.push(viewport);
        Ok(())
    }
    async fn input(&self, events: &[adb::Input]) -> Result<(), adb::BrowserError> {
        self.calls.lock().unwrap().inputs += events.len();
        Ok(())
    }
    async fn start_screencast(&self) -> Result<Box<dyn FrameFeed>, adb::BrowserError> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.feed.lock().unwrap() = Some(tx);
        self.calls.lock().unwrap().screencasts_started += 1;
        Ok(Box::new(Feed(rx)))
    }
    async fn stop_screencast(&self) -> Result<(), adb::BrowserError> {
        self.feed.lock().unwrap().take();
        self.calls.lock().unwrap().screencasts_stopped += 1;
        Ok(())
    }
    async fn capture(
        &self,
        clip: Option<adb::ClipRect>,
        dpr: f64,
    ) -> Result<adb::Picture, adb::BrowserError> {
        self.calls.lock().unwrap().captures.push((clip, dpr));
        Ok(adb::Picture {
            mime: "image/jpeg",
            bytes: jpeg(16, 16),
            width: 16,
            height: 16,
        })
    }
    async fn resolve(
        &self,
        x: f64,
        y: f64,
        _depth: i32,
    ) -> Result<adb::Resolved, adb::BrowserError> {
        let scripted = self.resolved.lock().unwrap().clone();
        Ok(scripted.unwrap_or_else(|| resolved_at("/", "button.save", x, y)))
    }
    async fn resolve_selector(
        &self,
        _selector: &str,
    ) -> Result<Option<adb::ClipRect>, adb::BrowserError> {
        Ok(Some(adb::ClipRect {
            x: 10.0,
            y: 10.0,
            w: 80.0,
            h: 24.0,
        }))
    }
    async fn current_path(&self) -> Result<String, adb::BrowserError> {
        Ok("/".into())
    }
    async fn wait_network_idle(&self, _: Duration, _: Duration) -> Result<bool, adb::BrowserError> {
        Ok(true)
    }
    fn latest_seq(&self) -> u64 {
        0
    }
    async fn frame_changed_within(&self, _since_seq: u64, _max: Duration) -> bool {
        true
    }
    async fn answer_dialog(&self, _: bool, _: Option<String>) -> Result<(), adb::BrowserError> {
        Ok(())
    }
    fn events(&self) -> broadcast::Receiver<adb::BrowserEvent> {
        self.events.subscribe()
    }
    fn rss_bytes(&self) -> Option<u64> {
        Some(*self.rss.lock().unwrap())
    }
    async fn close(&self) {
        self.calls.lock().unwrap().closed = true;
    }
}

/// An element the fake resolves a point to.
pub fn resolved_at(path: &str, selector: &str, x: f64, y: f64) -> adb::Resolved {
    adb::Resolved {
        path: path.into(),
        selector: Some(selector.into()),
        role_name: Some("button \"Save\"".into()),
        component: None,
        x,
        y,
        w: 80.0,
        h: 24.0,
        styles: Default::default(),
        label: "button \"Save\"".into(),
    }
}

/// A small valid JPEG header (SOI, a SOF0 with the size, EOI): enough for
/// the daemon's dimension and magic-byte checks.
pub fn jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    bytes.extend(height.to_be_bytes());
    bytes.extend(width.to_be_bytes());
    bytes.extend([0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
    bytes.extend([0xFF, 0xD9]);
    bytes
}

/// Hands out [`FakeBrowser`]s and records every launch.
#[derive(Default)]
pub struct FakeFactory {
    pub launches: Mutex<Vec<adb::LaunchSpec>>,
    pub browsers: Mutex<Vec<Arc<FakeBrowser>>>,
    /// The next launch fails with this error.
    pub fail: Mutex<Option<adb::BrowserError>>,
    /// No Chromium on the host.
    pub missing: Mutex<bool>,
}

impl FakeFactory {
    pub fn browser(&self, n: usize) -> Arc<FakeBrowser> {
        self.browsers.lock().unwrap()[n].clone()
    }

    pub fn launched(&self) -> usize {
        self.launches.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl BrowserFactory for FakeFactory {
    fn binary(&self, _configured: Option<&Path>) -> Result<PathBuf, adb::BrowserError> {
        if *self.missing.lock().unwrap() {
            return Err(adb::BrowserError::NotFound {
                hint: "install chromium".into(),
            });
        }
        Ok(PathBuf::from("/usr/bin/fake-chromium"))
    }

    async fn launch(
        &self,
        spec: adb::LaunchSpec,
    ) -> Result<Arc<dyn PreviewBrowser>, adb::BrowserError> {
        if let Some(error) = self.fail.lock().unwrap().take() {
            return Err(error);
        }
        let mut launches = self.launches.lock().unwrap();
        launches.push(spec);
        let browser = Arc::new(FakeBrowser::new(40_000 + launches.len() as u32));
        self.browsers.lock().unwrap().push(browser.clone());
        Ok(browser)
    }
}

/// A config with design sessions on.
pub fn design_config() -> Config {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.design.enabled = true;
    config
}

/// A running design stage: item, session, worktree and the fake browser factory.
pub struct Fixture {
    pub h: Harness,
    pub factory: Arc<FakeFactory>,
    pub item: QueueItemId,
    pub session: DesignSessionId,
    pub agent: AetherAgentId,
    pub worktree: WorktreeRef,
    pub run: archon_core::ids::PipelineRunId,
    pub stage_id: String,
    pub attempt: u32,
}

impl Fixture {
    pub async fn new() -> Self {
        Self::with(design_config()).await
    }

    pub async fn with(config: Config) -> Self {
        let h = Harness::with_config(config).await;
        let factory = Arc::new(FakeFactory::default());
        h.ctx.design.set_factory(factory.clone());
        let item = h.enqueue("design the save button").await;
        h.dispatch().await;
        let run = h.run(&item.id).await;
        // The dispatched stage's own agent, which the fake backend knows.
        let stage = h.running().await.remove(0);
        let agent = stage
            .aether_agent
            .clone()
            .expect("a running stage has an agent");
        let worktree = add_worktree(&h.repo, h.dir.path(), "design-1");
        let stage_id = stage.stage_id.clone();
        let session = h
            .ctx
            .design
            .stage_started(
                &h.ctx,
                &item.id,
                &run.id,
                &stage_id,
                stage.attempt,
                &agent,
                worktree.clone(),
            )
            .await
            .expect("session starts");
        Self {
            h,
            factory,
            item: item.id,
            session,
            agent,
            worktree,
            run: run.id,
            stage_id,
            attempt: stage.attempt,
        }
    }

    /// A request as the operator's own client sends it.
    pub async fn operator(
        &self,
        request: Request,
    ) -> Result<Reply, archon_core::error::ArchonError> {
        self.h.cli(request).await
    }

    /// A request as an observer (the web client) sends it.
    pub async fn observer(
        &self,
        request: Request,
    ) -> Result<Reply, archon_core::error::ArchonError> {
        Box::pin(archon_daemon::handlers::handle(
            &self.h.ctx,
            ClientKind::ArchonWeb,
            request,
        ))
        .await
    }

    /// A request as the design stage's agent sends it through MCP.
    pub async fn as_agent(
        &self,
        request: Request,
    ) -> Result<Reply, archon_core::error::ArchonError> {
        Box::pin(archon_daemon::handlers::handle(
            &self.h.ctx,
            ClientKind::ArchonMcp,
            request,
        ))
        .await
    }

    /// Commit a file in the worktree; returns the new HEAD.
    pub fn commit(&self, file: &str, body: &str, message: &str) -> String {
        std::fs::write(self.worktree.worktree.join(file), body).expect("write");
        git(&self.worktree.worktree, &["add", "."]);
        git(
            &self.worktree.worktree,
            &["commit", "--quiet", "-m", message],
        );
        self.head()
    }

    pub fn head(&self) -> String {
        git(&self.worktree.worktree, &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    pub async fn end(&self, how: StageEnd) {
        self.h
            .ctx
            .design
            .stage_settled(&self.h.ctx, &self.run, &self.stage_id, self.attempt, how)
            .await
            .expect("stage settled");
    }
}

/// A linked worktree of `repo` on a new branch.
pub fn add_worktree(repo: &Path, under: &Path, name: &str) -> WorktreeRef {
    let worktree = under.join(name);
    let branch = format!("archon/{name}");
    git(
        repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            &branch,
            worktree.to_str().unwrap(),
        ],
    );
    git(&worktree, &["config", "user.name", "Archon Test"]);
    git(&worktree, &["config", "user.email", "test@archon.invalid"]);
    git(&worktree, &["config", "commit.gpgsign", "false"]);
    WorktreeRef {
        repo: repo.to_owned(),
        worktree,
        branch,
    }
}
