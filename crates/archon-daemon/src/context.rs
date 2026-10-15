//! State shared by every daemon task. Built once in `run`; each component
//! receives `Arc<DaemonContext>` explicitly.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use archon_aether::AetherLink;
use archon_core::config::Config;
use archon_core::notify::FeedItem;
use archon_core::ports::AgentBackend;
use archon_core::queue::Caps;
use archon_lavish::Lavish;
use archon_queue::Store;
use tokio::sync::{Mutex, Notify, broadcast};

use crate::disk::DiskWatch;
use crate::executor::RunTable;
use crate::feed::ReplyTracker;
use crate::github::GithubState;
use crate::mirror::MirrorQueue;
use crate::supervisor::Supervisor;
use crate::triggers::TriggerQueue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub socket: PathBuf,
    pub data_dir: PathBuf,
    pub store: PathBuf,
    pub worktrees: PathBuf,
    pub orchestrator_workspace: PathBuf,
    pub scratch: PathBuf,
    pub reports: PathBuf,
    pub logs: PathBuf,
    /// This executable, for `archon setup` and MCP autostart.
    pub archon_bin: PathBuf,
}

impl DaemonPaths {
    pub fn resolve(config: &Config, archon_bin: PathBuf) -> Self {
        let data_dir = archon_core::paths::data_dir();
        let worktrees = config
            .worktrees
            .root
            .clone()
            .unwrap_or_else(|| data_dir.join("worktrees"));
        Self {
            socket: archon_core::paths::socket_path(),
            store: archon_core::paths::store_path(),
            worktrees,
            orchestrator_workspace: data_dir.join("orchestrator"),
            scratch: data_dir.join("scratch"),
            reports: data_dir.join("reports"),
            logs: archon_core::paths::log_dir(),
            archon_bin,
            data_dir,
        }
    }
}

pub struct DaemonContext {
    pub config: Config,
    pub paths: DaemonPaths,
    /// The only writer of `archon.db`. Held briefly; never across an await
    /// on Aether.
    pub store: Mutex<Store>,
    pub aether: Arc<AetherLink>,
    pub backend: Arc<dyn AgentBackend>,
    /// Every appended feed item, for `SubscribeFeed` connections.
    pub feed: broadcast::Sender<FeedItem>,
    /// Poked on enqueue, settle, caps or pause change, and when a gc mark
    /// goes.
    pub dispatch: Arc<Notify>,
    pub coordination: crate::executor::coordination::Coordination,
    /// Active run drivers and their pending spawns.
    pub runs: Mutex<RunTable>,
    /// Orchestrator chat reply detection.
    pub replies: Mutex<ReplyTracker>,
    /// Held while an operator answer is delivered and then recorded, so a
    /// question is delivered at most once (see `handlers::operator`).
    pub answering: Mutex<()>,
    /// Serializes report snapshot refreshes with pruning.
    pub report_files: Arc<Mutex<()>>,
    /// Serializes orchestrator create/revive and announces its id.
    pub supervisor: Supervisor,
    /// Serializes edits of Aether's GitHub watcher config.
    pub github: GithubState,
    pub triggers: TriggerQueue,
    /// Wakes the worker that keeps the orchestrator's repository mirrors.
    pub mirrors: MirrorQueue,
    /// Signalled when a spawn's outcome is recorded, for a fleet event of the
    /// new agent that arrived before it (`pump::fleet`).
    pub spawn_recorded: Notify,
    /// Runtime override from `SetCaps`; `None` uses `config.limits`.
    pub caps_override: Mutex<Option<Caps>>,
    pub lavish: Option<Lavish>,
    pub started_at: archon_core::Timestamp,
    /// Free disk space and the dispatch hold it drives.
    pub disk: DiskWatch,
    /// Set once archond's store hit a full disk: its work has stopped and it
    /// serves only reads until it restarts (see `standby`).
    pub standby: OnceLock<String>,
    /// Held through each worktree gc pass, so passes never overlap; when
    /// the last one ended.
    pub gc: Mutex<Option<std::time::Instant>>,
    /// Items a gc pass is collecting right now; none of them is started.
    pub collecting: crate::gc::Collecting,
    /// The operator's upgrade or restart, and the versions `status` shows.
    pub ops: crate::ops::OpsState,
}

impl DaemonContext {
    /// The directories whose filesystems the disk hold watches.
    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        vec![self.paths.data_dir.clone(), self.paths.worktrees.clone()]
    }
}

impl std::fmt::Debug for DaemonContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonContext")
            .field("paths", &self.paths)
            .field("aether_connected", &self.aether.is_connected())
            .field("lavish", &self.lavish)
            .finish_non_exhaustive()
    }
}
