//! `archond`, the Archon daemon (ARCHITECTURE §2, §5, §16).

mod agent_messages;
mod chat_answer;
pub mod context;
pub mod disk;
pub mod dispatcher;
pub mod executor;
pub mod feed;
pub mod gc;
pub mod github;
pub mod handlers;
pub mod mirror;
pub mod prune;
pub mod pump;
pub mod reports;
pub mod server;
mod standby;
pub mod supervisor;
pub mod triggers;
pub mod worktree;

mod knobs;
mod learned;
mod resolve;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_aether::AetherLink;
use archon_aether::backend::AetherBackend;
use archon_aether::client::Endpoint;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_lavish::Lavish;
use archon_queue::Store;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinSet;

use crate::context::{DaemonContext, DaemonPaths};
use crate::github::GithubState;
use crate::supervisor::Supervisor;
use crate::triggers::TriggerQueue;

/// Feed items buffered per subscriber before it lags (and catches up from
/// the store).
const FEED_BROADCAST_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub config: Config,
    /// This executable (for MCP registration hints and autostart).
    pub archon_bin: PathBuf,
}

/// Run until `Shutdown` or SIGTERM/SIGINT: open the store, recover runs,
/// bind the socket, then run the server, event pump, dispatcher, trigger
/// delivery, mirror refresh, store pruning and worktree gc concurrently. The
/// socket serves even while Aether is down, and in standby after the store
/// hit a full disk.
pub async fn run(options: DaemonOptions) -> Result<(), ArchonError> {
    let endpoint = Endpoint::from_config(&options.config.aether)?;
    let ctx = build_context(options)?;
    let ctx = Arc::new(ctx);
    // Bound under the daemon lock before recovery, so a second daemon never
    // touches live runs. The lock is released only once the socket is gone.
    let (listener, lock) = server::bind(&ctx)?;
    let mut signals = Signals::install()?;
    let result = match recover(&ctx).await {
        Ok(()) => supervise(ctx.clone(), listener, endpoint, &mut signals).await,
        Err(error @ ArchonError::DiskFull(_)) => {
            standby::enter(&ctx, &error);
            let server = tokio::spawn(server::serve(ctx.clone(), listener));
            standby::serve(&ctx, server, signals.stop()).await
        }
        Err(error) => Err(error),
    };
    remove_socket(&ctx.paths.socket);
    drop(lock);
    if let Err(error) = &result {
        tracing::error!(%error, "archond stopped");
    }
    result
}

/// Recover runs, with the disk hold read first so a low disk defers the
/// spawns recovery would issue.
async fn recover(ctx: &DaemonContext) -> Result<(), ArchonError> {
    dispatcher::check_disk(ctx).await?;
    note_legacy_config(ctx, &archon_core::paths::config_dir()).await?;
    executor::recover(ctx).await
}

/// `kv`: set while the operator has been told of an ignored `archon.toml`.
const LEGACY_CONFIG_TOLD_KEY: &str = "legacy_config_told";

/// Tell the operator, once, that the `archon.toml` in `config_dir` is not
/// read: they may believe it sets the limits Archon runs with.
pub async fn note_legacy_config(ctx: &DaemonContext, config_dir: &Path) -> Result<(), ArchonError> {
    let notice = archon_core::config_files::legacy_config_notice(config_dir);
    let told = {
        let mut store = ctx.store.lock().await;
        let told = store.kv_get(LEGACY_CONFIG_TOLD_KEY)?.is_some();
        match (notice.is_some(), told) {
            // The file is gone: one that appears later is news again.
            (false, true) => store.kv_delete(LEGACY_CONFIG_TOLD_KEY)?,
            (true, false) => store.kv_set(LEGACY_CONFIG_TOLD_KEY, "1")?,
            _ => {}
        }
        told
    };
    let Some(text) = notice.filter(|_| !told) else {
        return Ok(());
    };
    feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    Ok(())
}

/// SIGTERM and SIGINT, which stop the daemon cleanly.
struct Signals {
    sigterm: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> Result<Self, ArchonError> {
        let sigterm = signal(SignalKind::terminate())
            .map_err(|e| ArchonError::io("installing the SIGTERM handler", e))?;
        Ok(Self { sigterm })
    }

    async fn stop(&mut self) -> Result<(), ArchonError> {
        tokio::select! {
            _ = self.sigterm.recv() => {
                tracing::info!("SIGTERM received; stopping");
                Ok(())
            }
            interrupted = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received; stopping");
                interrupted.map_err(|e| ArchonError::io("waiting for SIGINT", e))
            }
        }
    }
}

type TaskResult = (&'static str, Result<(), ArchonError>);

/// The server and every worker task; the first to end stops the daemon.
/// Only the server ends cleanly (on `Shutdown`); a worker ending is a
/// failure, and one on a full disk turns the daemon to standby.
async fn supervise(
    ctx: Arc<DaemonContext>,
    listener: tokio::net::UnixListener,
    endpoint: Endpoint,
    signals: &mut Signals,
) -> Result<(), ArchonError> {
    let mut server: standby::Server = tokio::spawn(server::serve(ctx.clone(), listener));
    let mut workers: JoinSet<TaskResult> = JoinSet::new();
    let c = ctx.clone();
    workers.spawn(async move { ("event pump", pump::run(c, endpoint).await) });
    let c = ctx.clone();
    workers.spawn(async move { ("dispatcher", dispatcher::run(c).await) });
    let c = ctx.clone();
    workers.spawn(async move { ("coordination", executor::coordination::run(c).await) });
    let c = ctx.clone();
    workers.spawn(async move { ("trigger delivery", triggers::deliver_loop(c).await) });
    let c = ctx.clone();
    workers.spawn(async move { ("store pruning", prune::run(c).await) });
    let c = ctx.clone();
    workers.spawn(async move { ("repository mirrors", mirror::run(c).await) });
    let auto_gc = ctx.config.worktrees.auto_gc;
    if auto_gc {
        let c = ctx.clone();
        workers.spawn(async move { ("worktree gc", gc::run(c).await) });
    }
    assert_eq!(
        workers.len(),
        5 + usize::from(auto_gc),
        "every daemon worker is supervised"
    );
    let result = tokio::select! {
        stopped = signals.stop() => stopped,
        served = &mut server => server_ended(served),
        Some(done) = workers.join_next() => task_ended(done),
    };
    workers.shutdown().await;
    match result {
        Err(error @ ArchonError::DiskFull(_)) if !server.is_finished() => {
            standby::enter(&ctx, &error);
            standby::serve(&ctx, server, signals.stop()).await
        }
        result => {
            server.abort();
            result
        }
    }
}

/// The server ends cleanly only on `Shutdown`.
fn server_ended(
    served: Result<Result<(), ArchonError>, tokio::task::JoinError>,
) -> Result<(), ArchonError> {
    task_ended(served.map(|result| ("server", result)))
}

fn task_ended(done: Result<TaskResult, tokio::task::JoinError>) -> Result<(), ArchonError> {
    match done {
        Ok(("server", Ok(()))) => Ok(()),
        Ok((name, Ok(()))) => Err(ArchonError::io(
            format!("the {name} task"),
            std::io::Error::other("stopped unexpectedly"),
        )),
        Ok((name, Err(error))) => {
            tracing::error!(task = name, %error, "daemon task failed");
            Err(error)
        }
        Err(error) => Err(ArchonError::io(
            "a daemon task",
            std::io::Error::other(format!("panicked or was cancelled: {error}")),
        )),
    }
}

/// Resolve absolute paths, create the directories, open the store, and
/// build the shared context.
fn build_context(options: DaemonOptions) -> Result<DaemonContext, ArchonError> {
    let DaemonOptions { config, archon_bin } = options;
    let paths = absolute_paths(DaemonPaths::resolve(&config, archon_bin))?;
    for dir in [
        &paths.data_dir,
        &paths.worktrees,
        &paths.orchestrator_workspace,
        &paths.scratch,
        &paths.reports,
        &paths.logs,
    ] {
        std::fs::create_dir_all(dir)
            .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
    }
    let store = Store::open(&paths.store)?;
    let lavish = Lavish::detect(&config.lavish)?;
    let aether = Arc::new(AetherLink::new());
    let backend = Arc::new(AetherBackend::new(aether.clone(), config.models.policy()));
    let (feed, _) = broadcast::channel(FEED_BROADCAST_CAPACITY);
    let dispatch = Arc::new(Notify::new());
    let ctx = DaemonContext {
        config,
        paths,
        store: Mutex::new(store),
        aether,
        backend,
        feed,
        dispatch: dispatch.clone(),
        coordination: Default::default(),
        runs: Mutex::new(executor::RunTable::default()),
        replies: Mutex::new(feed::ReplyTracker::default()),
        answering: Mutex::new(()),
        report_files: Arc::new(Mutex::new(())),
        triggers: TriggerQueue::default(),
        mirrors: mirror::MirrorQueue::default(),
        spawn_recorded: Notify::new(),
        caps_override: Mutex::new(None),
        lavish,
        supervisor: Supervisor::default(),
        github: GithubState::default(),
        started_at: archon_core::now(),
        disk: disk::DiskWatch::system(),
        standby: std::sync::OnceLock::new(),
        gc: Mutex::new(None),
        collecting: gc::Collecting::new(dispatch),
    };
    Ok(ctx)
}

/// Relative paths (e.g. from `ARCHON_DATA_DIR=.`) resolve against the
/// working directory at start, so later directory changes cannot move them.
fn absolute_paths(paths: DaemonPaths) -> Result<DaemonPaths, ArchonError> {
    let cwd =
        std::env::current_dir().map_err(|e| ArchonError::io("reading the working directory", e))?;
    let abs = |p: PathBuf| if p.is_absolute() { p } else { cwd.join(p) };
    let paths = DaemonPaths {
        socket: abs(paths.socket),
        data_dir: abs(paths.data_dir),
        store: abs(paths.store),
        worktrees: abs(paths.worktrees),
        orchestrator_workspace: abs(paths.orchestrator_workspace),
        scratch: abs(paths.scratch),
        reports: abs(paths.reports),
        logs: abs(paths.logs),
        archon_bin: paths.archon_bin,
    };
    assert!(paths.socket.is_absolute() && paths.store.is_absolute());
    assert!(paths.worktrees.is_absolute() && paths.scratch.is_absolute());
    Ok(paths)
}

/// The socket and pid file are ours once bound; leftovers only cost the
/// next start a stale-socket check.
fn remove_socket(socket: &Path) {
    for path in [
        socket.to_path_buf(),
        archon_core::paths::pid_path_for(socket),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(path = %path.display(), %error, "not removed at exit"),
        }
    }
}
