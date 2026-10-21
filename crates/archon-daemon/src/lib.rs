//! `archond`, the Archon daemon (ARCHITECTURE §2, §5, §16).

mod agent_messages;
mod artifacts;
mod authority;
mod build_cache;
mod chat_answer;
mod chat_merge;
mod chat_ops;
mod command_notice;
pub mod context;
pub mod disk;
pub mod dispatcher;
pub mod executor;
mod experiments;
pub mod feed;
pub mod gc;
mod gh_account;
pub mod github;
pub mod handlers;
pub mod integration;
pub mod mirror;
pub mod notification_center;
mod operator_commands;
mod operator_sentence;
pub mod ops;
pub mod prune;
pub mod pump;
pub mod push;
pub mod quota;
pub mod recovery;
pub mod reports;
pub mod server;
mod standby;
pub mod status_health;
mod storage;
pub mod supervisor;
pub mod triggers;
pub mod worktree;

mod knobs;
pub mod learned;
mod resolve;
pub mod settings;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
use tokio::task::{JoinError, JoinSet};

use crate::context::{DaemonContext, DaemonPaths};
use crate::github::GithubState;
use crate::supervisor::Supervisor;
use crate::triggers::TriggerQueue;

/// Feed items buffered per subscriber before it lags (and catches up from
/// the store).
const FEED_BROADCAST_CAPACITY: usize = 1024;
/// Most the daemon waits for its aborted workers to stop; a supervisor's
/// grace period (the bench's is 20 s) must cover it.
const WORKER_STOP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub config: Config,
    /// This executable (for MCP registration hints and autostart).
    pub archon_bin: PathBuf,
    /// Starts Archon's own aetherd when it is gone; `None` in shared mode.
    pub aether_starter: Option<Arc<dyn pump::AetherStarter>>,
}

/// Run until `Shutdown` or SIGTERM/SIGINT: bind the socket, serve it, recover,
/// then run every worker; it serves while Aether is down, while it recovers
/// (reads only) and in standby.
pub async fn run(options: DaemonOptions) -> Result<(), ArchonError> {
    let endpoint = Endpoint::from_config(&options.config.aether)?;
    let starter = options.aether_starter.clone();
    // Holds the data directory's lock from before the store opens until the
    // store is dropped with the context, after the socket is gone.
    let ctx = build_context(options)?;
    let ctx = Arc::new(ctx);
    // Bound under the daemon lock before recovery, so a second daemon never
    // touches live runs. The lock is released only once the socket is gone.
    let (listener, lock) = server::bind(&ctx)?;
    let mut signals = Signals::install()?;
    // Begun before the server starts, so no change slips in ahead of recovery.
    ctx.recovery.begin();
    let server: standby::Server = tokio::spawn(server::serve(ctx.clone(), listener));
    let result = match recover(&ctx).await {
        Ok(()) => {
            ctx.recovery.finish();
            supervise(ctx.clone(), server, endpoint, starter, &mut signals).await
        }
        Err(error @ ArchonError::DiskFull(_)) => {
            // Standby first: the changes waiting on recovery meet its refusal.
            standby::enter(&ctx, &error);
            ctx.recovery.fail();
            standby::serve(&ctx, server, signals.stop()).await
        }
        Err(error) => {
            ctx.recovery.fail();
            server.abort();
            Err(error)
        }
    };
    remove_socket(&ctx.paths.socket);
    drop(lock);
    match &result {
        Ok(()) => tracing::info!("archond stopped"),
        Err(error) => {
            tracing::error!(%error, "archond stopped");
            record_fatal_stop(&ctx.paths.data_dir, error);
        }
    }
    result
}

/// Keep why archond stopped under the data directory, which outlives the
/// runtime directory's log.
fn record_fatal_stop(data_dir: &Path, error: &ArchonError) {
    let record = archon_core::crash::CrashRecord::new("fatal", &error.to_string(), None);
    if let Err(write_error) = archon_core::crash::write_pending(data_dir, &record) {
        tracing::error!(%write_error, "the crash marker was not written");
    }
}

/// Tell the operator, once, of the crash an earlier run left: in the log and
/// in the chat. The marker moves on only after the chat has it.
pub async fn report_crash(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let data_dir = &ctx.paths.data_dir;
    let pending = match archon_core::crash::read_pending(data_dir) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(%error, "the crash marker of an earlier run cannot be read");
            return Ok(());
        }
    };
    let Some(record) = pending else {
        return Ok(());
    };
    let text = record.describe();
    tracing::error!(kind = %record.kind, at = %record.at, "the previous run left a crash marker: {text}");
    let reported = record.at.to_string();
    let mut store = ctx.store.lock().await;
    if store.kv_get(CRASH_REPORTED_KEY)?.as_deref() != Some(reported.as_str()) {
        let notification = archon_core::notify::Notification {
            id: archon_core::id::NotificationId::new(),
            kind: NotificationKind::Blocker,
            text,
            item: None,
            run: None,
            at: archon_core::now(),
        };
        let entry = archon_core::notify::FeedEntry::Notification(notification);
        let item = store.append_feed_with_kv(
            entry,
            archon_core::now(),
            &[(CRASH_REPORTED_KEY, reported.as_str())],
        )?;
        drop(store);
        feed::publish(ctx, vec![item]);
    }
    archon_core::crash::acknowledge(data_dir)
}

/// `kv`: the time of the crash the chat was last told of, so a start cut off
/// before the marker moved on does not tell it twice.
const CRASH_REPORTED_KEY: &str = "crash_reported";

/// Recover runs, with the disk hold read first so a low disk defers the
/// spawns recovery would issue.
async fn recover(ctx: &DaemonContext) -> Result<(), ArchonError> {
    dispatcher::check_disk(ctx).await?;
    note_legacy_config(ctx, &archon_core::paths::config_dir()).await?;
    report_crash(ctx).await?;
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

/// The daemon's worker tasks, each known by name until it has ended.
#[derive(Default)]
struct Workers {
    tasks: JoinSet<TaskResult>,
    names: HashMap<tokio::task::Id, &'static str>,
}

impl Workers {
    fn spawn<F>(&mut self, name: &'static str, work: F)
    where
        F: Future<Output = Result<(), ArchonError>> + Send + 'static,
    {
        let handle = self.tasks.spawn(async move { (name, work.await) });
        let earlier = self.names.insert(handle.id(), name);
        assert!(earlier.is_none(), "task ids are unique");
    }

    /// The next worker to end; `None` once none is left.
    async fn join_next(&mut self) -> Option<Result<TaskResult, JoinError>> {
        let done = self.tasks.join_next_with_id().await?;
        let id = match &done {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        assert!(self.names.remove(&id).is_some(), "a worker ends once");
        Some(done.map(|(_, result)| result))
    }

    /// Abort every worker and wait up to `limit` for them to end. One busy
    /// outside any `.await` cannot be aborted: it is named, left behind and
    /// returned.
    async fn stop(mut self, limit: Duration) -> Vec<&'static str> {
        self.tasks.abort_all();
        let deadline = tokio::time::Instant::now() + limit;
        while let Ok(Some(_)) = tokio::time::timeout_at(deadline, self.join_next()).await {}
        let left: Vec<&'static str> = self.names.into_values().collect();
        for task in &left {
            tracing::warn!(
                task,
                ?limit,
                "daemon task still running after abort; exiting without it"
            );
        }
        assert_eq!(left.len(), self.tasks.len(), "every running task is named");
        self.tasks.detach_all();
        left
    }
}

/// The server and every worker task; the first to end stops the daemon.
/// Only the server ends cleanly (on `Shutdown`); a worker ending is a
/// failure, and one on a full disk turns the daemon to standby. An
/// upgrade's handed-off restart also stops it cleanly.
async fn supervise(
    ctx: Arc<DaemonContext>,
    mut server: standby::Server,
    endpoint: Endpoint,
    starter: Option<Arc<dyn pump::AetherStarter>>,
    signals: &mut Signals,
) -> Result<(), ArchonError> {
    assert!(ctx.recovery.is_recovered(), "workers start once recovered");
    let mut workers = Workers::default();
    workers.spawn("event pump", pump::run(ctx.clone(), endpoint, starter));
    workers.spawn("dispatcher", dispatcher::run(ctx.clone()));
    workers.spawn("coordination", executor::coordination::run(ctx.clone()));
    workers.spawn("trigger delivery", triggers::deliver_loop(ctx.clone()));
    workers.spawn("store pruning", prune::run(ctx.clone()));
    workers.spawn("repository mirrors", mirror::run(ctx.clone()));
    workers.spawn("learning", learned::run(ctx.clone()));
    workers.spawn("upgrades", ops::run(ctx.clone()));
    let auto_gc = ctx.config.worktrees.auto_gc;
    if auto_gc {
        workers.spawn("worktree gc", gc::run(ctx.clone()));
    }
    let verifies = ctx.config.integration.enabled;
    if verifies {
        workers.spawn("integration verification", integration::run(ctx.clone()));
    }
    assert_eq!(
        workers.names.len(),
        8 + usize::from(auto_gc) + usize::from(verifies),
        "every daemon worker is supervised"
    );
    let result = tokio::select! {
        stopped = signals.stop() => stopped,
        served = &mut server => server_ended(served),
        Some(done) = workers.join_next() => task_ended(done),
        () = ctx.ops.stopping() => {
            tracing::info!("stopping for an upgrade's restart");
            Ok(())
        }
    };
    workers.stop(WORKER_STOP).await;
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
    let DaemonOptions {
        config, archon_bin, ..
    } = options;
    let paths = absolute_paths(DaemonPaths::resolve(&config, archon_bin))?;
    let ops_port = Arc::new(ops::system::SystemOps {
        archon_bin: paths.archon_bin.clone(),
        workdir: paths.scratch.clone(),
        startup_log: archon_core::paths::startup_log_for(&paths.socket),
    });
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
    let store = open_store(&paths, &config)?;
    let operator_settings = settings::SettingsBook::load(&store)?;
    let lavish = Lavish::detect(&config.lavish)?;
    let aether = Arc::new(AetherLink::new());
    let backend = Arc::new(AetherBackend::new(aether.clone(), config.models.policy()));
    let (feed, _) = broadcast::channel(FEED_BROADCAST_CAPACITY);
    let dispatch = Arc::new(Notify::new());
    let ctx = DaemonContext {
        storage: Default::default(),
        status_probes: Default::default(),
        quota: Default::default(),
        push: Default::default(),
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
        authority_changes: Mutex::new(()),
        learning_memory: Mutex::new(()),
        memory_curation: Mutex::new(()),
        memory_feedback_ready: Notify::new(),
        report_files: Arc::new(Mutex::new(())),
        triggers: TriggerQueue::default(),
        mirrors: mirror::MirrorQueue::default(),
        spawn_recorded: Notify::new(),
        operator_settings: std::sync::Mutex::new(operator_settings),
        lavish,
        supervisor: Supervisor::default(),
        github: GithubState::default(),
        started_at: archon_core::now(),
        disk: disk::DiskWatch::system(),
        standby: std::sync::OnceLock::new(),
        recovery: Default::default(),
        gc: Mutex::new(None),
        collecting: gc::Collecting::new(dispatch),
        ops: ops::OpsState::new(ops_port, ops::DRAIN_POLL),
    };
    Ok(ctx)
}

/// The store, under the data dir's exclusive lock taken first and held as long as the store:
/// a second daemon on this data dir stops here, before it reads or migrates anything.
fn open_store(paths: &DaemonPaths, config: &Config) -> Result<Store, ArchonError> {
    assert!(
        paths.data_dir.is_dir(),
        "the data directory is created first"
    );
    assert_eq!(
        paths.store.parent(),
        Some(paths.data_dir.as_path()),
        "the store lives in the data directory its lock guards"
    );
    let lock = archon_queue::DataLock::acquire(&paths.data_dir, &paths.socket)?;
    let mut store = Store::open_owned(&paths.store, lock)?;
    store.configure_learning(&config.learning)?;
    if !config.learning.enabled {
        store.cancel_queued_distillations()?;
    }
    Ok(store)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_leaves_a_worker_that_never_yields_behind() {
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let mut workers = Workers::default();
        workers.spawn("waits", std::future::pending());
        workers.spawn("blocks", async move {
            started.send(()).expect("the test waits for the start");
            // Blocks its thread outside any `.await`, so an abort cannot land.
            let _released = blocked.recv();
            Ok(())
        });
        running.await.expect("the blocking worker started");

        let stop = workers.stop(Duration::from_millis(200));
        let stopped = tokio::time::timeout(Duration::from_secs(2), stop).await;
        release.send(()).expect("the blocked worker still waits");

        let left = stopped.expect("the stop is bounded");
        assert_eq!(left, ["blocks"], "only the blocked worker is left behind");
    }
}
