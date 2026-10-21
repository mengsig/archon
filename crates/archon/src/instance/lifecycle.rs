//! Starting and stopping the dedicated `aetherd` through the `aether` CLI,
//! always with the instance's environment. A lock file serializes starts,
//! so concurrent archon commands launch one daemon.

use std::fs::{File, OpenOptions};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use archon_core::error::ArchonError;

use super::{Instance, create_private_dir, seed};

/// How long a daemon `aether daemon` reported as started may take to answer.
const LISTEN_WAIT: Duration = Duration::from_secs(10);
/// How long `aether stop` may take to close the socket.
const STOP_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);
/// What `aether daemon` says while another aetherd holds the instance's store.
const STORE_HELD: &str = "another aether daemon is already running";
/// How long an aetherd that stopped answering may take to release its store.
const RELEASE_WAIT: Duration = Duration::from_secs(15);
const RELEASE_POLL: Duration = Duration::from_millis(250);
/// What `aether daemon` says when it gave up waiting for a daemon that lives.
const STILL_STARTING: [&str; 2] = ["did not answer", "it is still running"];
/// How long a slow-starting aetherd may stay silent in its startup log.
const SLOW_START_QUIET: Duration = Duration::from_secs(120);
/// The longest wait for an aetherd that keeps logging while it starts.
const SLOW_START_MAX: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    AlreadyRunning,
    Started,
    /// It ran with a registration Archon has since replaced, and was
    /// restarted to load the current one.
    Restarted,
    /// Another archon started or restarted it while this one waited its turn.
    StartedMeanwhile,
}

impl Start {
    /// Whether archond, if it runs, has to connect to this aetherd anew.
    pub fn is_fresh(self) -> bool {
        self != Self::AlreadyRunning
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    NotRunning,
    Stopped,
}

/// Something accepts connections on the socket.
pub fn is_listening(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Start the instance's `aetherd` unless it already answers. A registration an
/// older setup wrote is repaired first, and an aetherd that loaded it restarts.
pub fn ensure_running(instance: &Instance, aether: &Path) -> anyhow::Result<Start> {
    if is_listening(&instance.socket) && !must_reload(instance)? {
        return Ok(Start::AlreadyRunning);
    }
    let config = instance.config_path();
    anyhow::ensure!(
        config.is_file(),
        "Archon's own Aether is not set up ({} is missing); run `archon setup`",
        config.display()
    );
    prepare_dirs(instance)?;
    let _lock = lock_start(instance)?;
    // Another archon may have repaired or restarted it while this one waited.
    seed::repair_registration(instance)?;
    let stale_daemon = seed::reload_is_owed(instance)? && is_listening(&instance.socket);
    if stale_daemon {
        stop_listening(instance, aether)?;
    }
    seed::reload_settled(instance)?;
    if is_listening(&instance.socket) {
        return Ok(Start::StartedMeanwhile);
    }
    start_daemon(instance, aether)?;
    anyhow::ensure!(
        wait_until(&instance.socket, true, LISTEN_WAIT),
        "`aether daemon` reported success, but nothing answers on {} after {LISTEN_WAIT:?}",
        instance.socket.display()
    );
    Ok(match stale_daemon {
        true => Start::Restarted,
        false => Start::Started,
    })
}

/// How archond starts the instance's aetherd again when it is gone: as any
/// archon command does, under the same start lock.
#[derive(Debug, Clone)]
pub struct Starter {
    pub instance: Instance,
    pub aether: PathBuf,
}

impl archon_daemon::pump::AetherStarter for Starter {
    fn ensure_running(&self) -> Result<(), ArchonError> {
        let start = ensure_running(&self.instance, &self.aether)
            .map_err(|error| ArchonError::Unavailable(format!("{error:#}")))?;
        tracing::info!(?start, "Archon's own aetherd answers");
        Ok(())
    }
}

/// Whether [`ensure_running`] is about to restart an aetherd that answers.
pub fn restart_is_due(instance: &Instance) -> anyhow::Result<bool> {
    Ok(is_listening(&instance.socket) && must_reload(instance)?)
}

/// Whether the aetherd that answers has a registration to reload.
fn must_reload(instance: &Instance) -> anyhow::Result<bool> {
    Ok(seed::reload_is_owed(instance)? || seed::registration_is_stale(instance)?)
}

/// `aether daemon`, retried while the aetherd before it still holds the
/// store: it stops answering first and lets go of the store as it exits.
fn start_daemon(instance: &Instance, aether: &Path) -> anyhow::Result<()> {
    assert!(RELEASE_POLL < RELEASE_WAIT, "the wait allows a retry");
    let deadline = Instant::now() + RELEASE_WAIT;
    loop {
        let error = match run_aether(instance, aether, "daemon") {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let said = format!("{error:#}");
        if STILL_STARTING.iter().all(|part| said.contains(part)) {
            tracing::info!(socket = %instance.socket.display(), "aetherd is still starting");
            return wait_for_slow_start(&instance.socket, SLOW_START_QUIET, SLOW_START_MAX);
        }
        let store_held = said.contains(STORE_HELD);
        if !store_held || Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(RELEASE_POLL);
    }
}

/// Wait for an aetherd that `aether daemon` left running but unanswering,
/// as it does while startup recovery of a big store runs past its own 10 s.
/// It is given up on only once its startup log stops growing for `quiet`, or
/// after `max`: a healthy daemon that is slow never fails the start.
fn wait_for_slow_start(socket: &Path, quiet: Duration, max: Duration) -> anyhow::Result<()> {
    assert!(quiet <= max, "the silence limit fits in the total");
    let log = socket.with_extension("startup.log");
    let started = Instant::now();
    let mut seen = log_state(&log);
    let mut last_progress = started;
    loop {
        if is_listening(socket) {
            return Ok(());
        }
        let now = Instant::now();
        let state = log_state(&log);
        if state != seen {
            seen = state;
            last_progress = now;
        }
        if now.duration_since(last_progress) >= quiet {
            anyhow::bail!(
                "Archon's aetherd was started but did not answer on {} and wrote nothing to {} \
                 for {quiet:?}",
                socket.display(),
                log.display()
            );
        }
        if now.duration_since(started) >= max {
            anyhow::bail!(
                "Archon's aetherd was still starting on {} after {max:?}; see {}",
                socket.display(),
                log.display()
            );
        }
        std::thread::sleep(POLL);
    }
}

/// Size and modification time of the startup log, `None` while it is absent.
fn log_state(log: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(log).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Stop the instance's `aetherd` with `aether stop`, and wait until it is
/// gone. Blocking.
pub fn stop(instance: &Instance, aether: &Path) -> anyhow::Result<Stop> {
    if !is_listening(&instance.socket) {
        return Ok(Stop::NotRunning);
    }
    prepare_dirs(instance)?;
    stop_listening(instance, aether)?;
    Ok(Stop::Stopped)
}

/// `aether stop` for a daemon that answers, waiting until it is gone.
fn stop_listening(instance: &Instance, aether: &Path) -> anyhow::Result<()> {
    run_aether(instance, aether, "stop")?;
    anyhow::ensure!(
        wait_until(&instance.socket, false, STOP_WAIT),
        "`aether stop` returned, but Archon's aetherd still answers on {} after {STOP_WAIT:?}",
        instance.socket.display()
    );
    Ok(())
}

/// The instance's data directory and the socket's directory, private.
fn prepare_dirs(instance: &Instance) -> anyhow::Result<()> {
    let socket_dir = instance
        .socket
        .parent()
        .context("the instance socket has no directory")?;
    create_private_dir(&instance.data_dir)?;
    create_private_dir(socket_dir)
}

/// The start lock, held until the returned file is dropped.
fn lock_start(instance: &Instance) -> anyhow::Result<File> {
    let path = instance.start_lock_path();
    assert!(
        path.starts_with(&instance.archon_data_dir),
        "the lock is Archon's file"
    );
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// `aether <verb>` with the instance environment, run in the instance's data
/// directory so the daemon never holds the operator's working directory.
fn run_aether(instance: &Instance, aether: &Path, verb: &str) -> anyhow::Result<()> {
    assert!(
        instance.data_dir.is_dir(),
        "the data directory exists first"
    );
    let output = instance
        .aether_command(aether, [verb])
        .current_dir(&instance.data_dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "running `{} {verb}` for Archon's own Aether; is Aether installed?",
                aether.display()
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "`{} {verb}` for Archon's own Aether (socket {}) exited with {}: {}",
        aether.display(),
        instance.socket.display(),
        output.status,
        stderr.trim()
    )
}

/// Whether the socket reached the wanted state (`listening` or not) in time.
fn wait_until(socket: &Path, listening: bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if is_listening(socket) == listening {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests;
