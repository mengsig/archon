//! Starting and stopping the dedicated `aetherd` through the `aether` CLI,
//! always with the instance's environment. A lock file serializes starts,
//! so concurrent archon commands launch one daemon.

use std::fs::{File, OpenOptions};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    AlreadyRunning,
    Started,
    /// It ran with a registration Archon has since replaced, and was
    /// restarted to load the current one.
    Restarted,
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
        return Ok(Start::AlreadyRunning);
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
        let store_held = format!("{error:#}").contains(STORE_HELD);
        if !store_held || Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(RELEASE_POLL);
    }
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
