//! `archon daemon run|start|stop|status|restart|upgrade` and `archon mcp`.

use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::ops::{UpgradeRequest, UpgradeTarget};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::DaemonOptions;

use super::{emit, instance};
use crate::cli::{DaemonCommand, DrainArgs};
use crate::connect::{archon_bin, connect_existing, connect_or_start, connect_running, unexpected};
use crate::instance::{AETHER_PROGRAM, Instance, create_private_dir, effective_aether, lifecycle};
use crate::views::fetch_status;

const STOP_WAIT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(100);
/// How long the restart helper waits for the archond that started it to exit.
const RELAUNCH_WAIT: Duration = Duration::from_secs(120);
/// How long `--detach` waits for its daemon to listen. Shorter than the
/// client's autostart timeout, so the client still hears how it ended.
const DETACH_WAIT: Duration = Duration::from_secs(8);
/// `daemon status` exit code when nothing is running (as `systemctl status`).
const NOT_RUNNING: u8 = 3;

pub async fn run(command: DaemonCommand, json: bool) -> anyhow::Result<ExitCode> {
    match command {
        DaemonCommand::Run { detach: false } => foreground().await,
        DaemonCommand::Run { detach: true } => detach(json).await,
        DaemonCommand::Start => {
            let client = connect_or_start().await?;
            report_running(&client, json)?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonCommand::Stop {
            aether,
            keep_aether,
            force,
        } => {
            refuse_mixed_locations("daemon stop")?;
            stop(json, aether, keep_aether, force).await
        }
        DaemonCommand::Status => status(json).await,
        DaemonCommand::Restart { drain, keep_aether } => {
            refuse_mixed_locations("daemon restart")?;
            upgrade(upgrade_request(None, None, drain, keep_aether)?, json).await
        }
        DaemonCommand::Upgrade {
            archon,
            aether,
            drain,
        } => {
            refuse_mixed_locations("daemon upgrade")?;
            upgrade(upgrade_request(archon, aether, drain, false)?, json).await
        }
        DaemonCommand::Relaunch { keep_aether } => relaunch(keep_aether).await,
    }
}

/// archond's state, and in dedicated mode Archon's own aetherd's.
async fn status(json: bool) -> anyhow::Result<ExitCode> {
    // A broken config must not hide whether archond runs.
    let aether = Config::load_default()
        .map_err(anyhow::Error::from)
        .and_then(|config| instance::aether_status(&config));
    let (aether_value, aether_text) = aether.unwrap_or_else(|error| {
        let text = format!("Aether: unknown ({error:#}).");
        (serde_json::json!({"error": format!("{error:#}")}), text)
    });
    let client = connect_existing().await?;
    let (mut value, text) = match &client {
        Some(client) => running(client),
        None => (
            serde_json::json!({"running": false}),
            "archond is not running.".to_owned(),
        ),
    };
    value["aether"] = aether_value;
    emit(json, || Ok(value), &format!("{text}\n{aether_text}"))?;
    Ok(match client {
        Some(_) => ExitCode::SUCCESS,
        None => ExitCode::from(NOT_RUNNING),
    })
}

async fn foreground() -> anyhow::Result<ExitCode> {
    warn_if_mixed();
    let mut config = Config::load_default()?;
    let instance = Instance::resolve()?;
    // Dedicated mode: archond talks only to Archon's own aetherd, and starts it when it is gone.
    config.aether = effective_aether(&config.aether, &instance);
    let aether_starter = (config.aether.mode == AetherMode::Dedicated).then(|| {
        let starter = lifecycle::Starter {
            instance,
            aether: AETHER_PROGRAM.into(),
        };
        Arc::new(starter) as Arc<dyn archon_daemon::pump::AetherStarter>
    });
    let options = DaemonOptions {
        config,
        archon_bin: archon_bin()?,
        aether_starter,
    };
    archon_daemon::run(options).await?;
    Ok(ExitCode::SUCCESS)
}

/// A daemon run directly is the operator's choice even where the locations
/// mix moved and default ones, which autostart refuses; it is still said.
fn warn_if_mixed() {
    if let Some(mix) = archon_core::paths::partial_override(&archon_client::default_socket()) {
        if std::env::var_os(crate::daemon_log::DETACHED_ENV).is_some() {
            tracing::warn!("{mix}. Starting archond anyway, as it was asked for directly.");
        } else {
            eprintln!(
                "archon: warning: {mix}. Starting archond anyway, as it was asked for directly."
            );
        }
    }
}

/// Where the locations mix moved and default ones, the archond and aetherd
/// found there may be another setup's, so `command` stops and restarts nothing.
pub(super) fn refuse_mixed_locations(command: &str) -> anyhow::Result<()> {
    let socket = archon_client::default_socket();
    let Some(mix) = archon_core::paths::partial_override(&socket) else {
        return Ok(());
    };
    Err(ArchonError::Invalid(format!(
        "`archon {command}` stops and restarts nothing here: {mix}"
    ))
    .into())
}

/// Start `archon daemon run` in its own process group, appending to the startup log
/// beside the socket, and wait until it listens; fail as soon as it exits first.
async fn detach(json: bool) -> anyhow::Result<ExitCode> {
    warn_if_mixed();
    let socket = archon_client::default_socket();
    let socket =
        std::path::absolute(&socket).with_context(|| format!("resolving {}", socket.display()))?;
    let log_path = archon_core::paths::startup_log_for(&socket);
    assert_ne!(
        log_path, socket,
        "the startup log never replaces the socket"
    );
    create_private_dir(
        socket
            .parent()
            .context("the socket path has no directory")?,
    )?;
    // Validate before detaching: a child with invalid config has no terminal
    // on which to explain why it could not initialize its log.
    Config::load_default()?;
    let mut child = daemon_command(&socket)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env(crate::daemon_log::DETACHED_ENV, "1")
        // Its own process group: a Ctrl-C in the operator's terminal must
        // not reach the daemon.
        .process_group(0)
        .spawn()
        .context("starting archond")?;
    let pid = child.id();
    let listening = await_listening(&mut child, &socket, &log_path).await?;
    // Not waited for: the daemon outlives this process and is reparented.
    drop(child);
    let state = if listening { "started" } else { "starting" };
    let text = format!(
        "archond {state} (pid {pid}); startup log: {}",
        log_path.display()
    );
    emit(
        json,
        || Ok(serde_json::json!({"pid": pid, "listening": listening, "log": log_path})),
        &text,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `archon daemon run` in the data directory, so the daemon never holds the
/// operator's working directory. Its directories are passed as absolute
/// paths, so relative overrides keep the meaning they have here (§9).
fn daemon_command(socket: &Path) -> anyhow::Result<Command> {
    let absolute = |path: &Path| {
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))
    };
    let data = absolute(&archon_core::paths::data_dir())?;
    let config = absolute(&archon_core::paths::config_dir())?;
    let socket = absolute(socket)?;
    create_private_dir(&data)?;
    assert!(data.is_dir(), "the daemon starts in an existing directory");
    let mut command = Command::new(archon_bin()?);
    command
        .args(["daemon", "run"])
        .current_dir(&data)
        .env("ARCHON_DATA_DIR", &data)
        .env("ARCHON_CONFIG_DIR", &config)
        .env("ARCHON_SOCKET", &socket);
    Ok(command)
}

/// True once something answers on the socket, false when `DETACH_WAIT`
/// passes first; an error when the daemon exits before either.
async fn await_listening(child: &mut Child, socket: &Path, log: &Path) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + DETACH_WAIT;
    let mut stderr = child.stderr.take();
    if let Some(stderr) = &stderr {
        rustix::fs::fcntl_setfl(stderr, rustix::fs::OFlags::NONBLOCK)
            .context("making archond startup diagnostics nonblocking")?;
    }
    let mut diagnostics = Vec::new();
    loop {
        read_startup_error(stderr.as_mut(), &mut diagnostics)?;
        if let Some(status) = child.try_wait().context("waiting for archond")? {
            read_startup_error(stderr.as_mut(), &mut diagnostics)?;
            let detail = String::from_utf8_lossy(&diagnostics);
            anyhow::bail!(
                "archond (pid {}) exited with {status} before it listened; startup log: {}\n{}",
                child.id(),
                log.display(),
                detail.trim()
            );
        }
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// Drain during startup so even a large diagnostic cannot fill the pipe and
/// block the child. Bound both retained bytes and work per readiness poll;
/// descendants may keep stderr open, so EOF is never required.
fn read_startup_error(stderr: Option<&mut ChildStderr>, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
    let Some(stderr) = stderr else {
        return Ok(());
    };
    const LIMIT: usize = 64 * 1024;
    let mut buffer = [0; 8192];
    for _ in 0..LIMIT / buffer.len() {
        match stderr.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => bytes.extend_from_slice(&buffer[..count.min(LIMIT - bytes.len())]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading archond startup diagnostics"),
        }
    }
    Ok(())
}

/// Stop archond, then in dedicated mode Archon's own aetherd unless `keep_aether`.
/// Shared mode owns no aetherd, so `--aether` is refused there before anything stops.
async fn stop(
    json: bool,
    aether: bool,
    keep_aether: bool,
    force: bool,
) -> anyhow::Result<ExitCode> {
    assert!(!(aether && keep_aether), "clap refuses both");
    if aether && Config::load_default()?.aether.mode != AetherMode::Dedicated {
        return Err(archon_core::ArchonError::Invalid(
            "--aether stops Archon's own aetherd, and [aether] mode is \"shared\": \
             the operator's aetherd is theirs to stop"
                .into(),
        )
        .into());
    }
    let (mut value, mut text) = stop_archond(force).await?;
    if !keep_aether && owns_aetherd()? {
        let (aether_value, aether_text) = instance::stop_aether().await?;
        value["aether"] = aether_value;
        text = format!("{text}\n{aether_text}");
    }
    emit(json, || Ok(value), &text)?;
    Ok(ExitCode::SUCCESS)
}

/// Whether Archon runs its own aetherd (`[aether] mode = "dedicated"`), asked once
/// archond is stopped: a config that no longer loads never keeps archond up.
fn owns_aetherd() -> anyhow::Result<bool> {
    let config = Config::load_default()
        .context("archond stopped, but the config naming Archon's aetherd did not load")?;
    Ok(config.aether.mode == AetherMode::Dedicated)
}

async fn stop_archond(force: bool) -> anyhow::Result<(serde_json::Value, String)> {
    let Some(client) = connect_existing().await? else {
        let text = "archond is not running.".to_owned();
        return Ok((serde_json::json!({"stopped": false}), text));
    };
    let pid = client.welcome().pid;
    let active = fetch_status(&client).await?.active_runs;
    refuse_stop_with_runs(active, force)?;
    match client.call(Request::Shutdown).await {
        Ok(Ok(_)) => {}
        Ok(Err(refusal)) => return Err(archon_client::refusal_error(refusal).into()),
        // The daemon may close the connection before the reply arrives.
        Err(ArchonError::Unavailable(_) | ArchonError::Io { .. }) => {}
        Err(e) => return Err(e).context("asking archond to shut down"),
    }
    drop(client);
    wait_until_gone().await?;
    let value = serde_json::json!({"stopped": true, "pid": pid});
    Ok((value, format!("Stopped archond (pid {pid}).")))
}

/// Stopping kills the agents of active runs and fails their items, so it needs `--force`.
/// A guard on one status snapshot: a run dispatched after it is not covered.
fn refuse_stop_with_runs(active_runs: u32, force: bool) -> anyhow::Result<()> {
    if active_runs == 0 || force {
        return Ok(());
    }
    anyhow::bail!(
        "{active_runs} run(s) are active, and stopping archond would kill their agents and \
         fail their items. `archon daemon restart` waits for them to finish; \
         `archon daemon stop --force` stops anyway."
    )
}

async fn wait_until_gone() -> anyhow::Result<()> {
    let socket = archon_client::default_socket();
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    loop {
        match ArchonClient::connect(&socket, ClientKind::ArchonCli).await {
            Err(ArchonError::Unavailable(_)) => return Ok(()),
            Ok(_) | Err(_) if tokio::time::Instant::now() >= deadline => {
                anyhow::bail!(
                    "archond still answers on {} after {STOP_WAIT:?}",
                    socket.display()
                )
            }
            _ => tokio::time::sleep(STOP_POLL).await,
        }
    }
}

fn report_running(client: &ArchonClient, json: bool) -> anyhow::Result<()> {
    let (value, text) = running(client);
    emit(json, || Ok(value), &text)
}

fn running(client: &ArchonClient) -> (serde_json::Value, String) {
    let welcome = client.welcome();
    assert!(welcome.pid > 0, "archond reports its pid");
    let text = format!(
        "archond {} is running (pid {}, protocol {}).",
        welcome.daemon_version, welcome.pid, welcome.protocol_version
    );
    (
        serde_json::json!({"running": true, "welcome": welcome}),
        text,
    )
}

fn upgrade_request(
    archon: Option<String>,
    aether: Option<String>,
    drain: DrainArgs,
    keep_aether: bool,
) -> anyhow::Result<UpgradeRequest> {
    let target = |text: Option<String>| text.as_deref().map(UpgradeTarget::parse).transpose();
    let request = UpgradeRequest {
        archon: target(archon)?,
        aether: target(aether)?,
        drain_secs: drain.wait,
        requeue: drain.requeue,
        keep_aether,
    };
    request.check()?;
    Ok(request)
}

/// Hand `request` to the running archond, which drains and restarts itself
/// through a detached helper, so this command returns at once.
async fn upgrade(request: UpgradeRequest, json: bool) -> anyhow::Result<ExitCode> {
    // archond leaves for good when the new one cannot load the config.
    Config::load_default().context("not restarting archond: the config does not load")?;
    let client = connect_running().await?;
    let text = match client.call(Request::Upgrade(request)).await? {
        Ok(Reply::UpgradeStarted { text }) => text,
        Ok(other) => return Err(unexpected(&other, "upgrade_started")),
        Err(refusal) => return Err(archon_client::refusal_error(refusal).into()),
    };
    emit(json, || Ok(serde_json::json!({"started": text})), &text)?;
    Ok(ExitCode::SUCCESS)
}

/// The helper archond starts for an upgrade's restart: once archond has let
/// go of its data directory, stop Archon's own aetherd unless kept, then
/// start both again. Its output goes to archond's startup log.
async fn relaunch(keep_aether: bool) -> anyhow::Result<ExitCode> {
    wait_for_data_dir(RELAUNCH_WAIT).await?;
    if !keep_aether && owns_aetherd()? {
        let (_, text) = instance::stop_aether().await?;
        println!("{text}");
    }
    let client = connect_or_start().await?;
    report_running(&client, false)?;
    Ok(ExitCode::SUCCESS)
}

/// Wait until no archond holds the data directory: it has exited then,
/// even if its parent has not reaped it yet.
async fn wait_for_data_dir(limit: Duration) -> anyhow::Result<()> {
    let data = std::path::absolute(archon_core::paths::data_dir())
        .context("resolving the data directory")?;
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match archon_queue::DataLock::share(&data) {
            Ok(lock) => {
                drop(lock);
                return Ok(());
            }
            Err(ArchonError::Unavailable(held)) if tokio::time::Instant::now() >= deadline => {
                anyhow::bail!("nothing was restarted: after {limit:?}, {held}")
            }
            Err(ArchonError::Unavailable(_)) => tokio::time::sleep(STOP_POLL).await,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Serve MCP on stdio for Aether; never prints anything else to stdout.
pub async fn mcp() -> anyhow::Result<ExitCode> {
    let socket = archon_client::default_socket();
    archon_mcp::serve_stdio(&socket, &archon_bin()?).await?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_with_active_runs_needs_force() {
        let refused = refuse_stop_with_runs(2, false).unwrap_err().to_string();
        assert!(refused.contains("2 run(s)") && refused.contains("daemon restart"));
        refuse_stop_with_runs(2, true).unwrap();
        refuse_stop_with_runs(0, false).unwrap();
    }

    #[tokio::test]
    async fn startup_diagnostics_are_returned_after_child_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "printf 'cannot initialize daemon log\\n' >&2; exit 1"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let error = await_listening(
            &mut child,
            &dir.path().join("absent.sock"),
            &dir.path().join("archond.log"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("cannot initialize daemon log"));
        assert!(
            child
                .try_wait()
                .unwrap()
                .unwrap()
                .code()
                .is_some_and(|code| code != 0)
        );
    }

    #[tokio::test]
    async fn large_startup_diagnostic_is_drained_and_bounded() {
        let mut child = Command::new("sh")
            .args(["-c", "i=0; while [ \"$i\" -lt 2000 ]; do printf 'initialization failed with a long explanation that exceeds the pipe capacity\\n' >&2; i=$((i + 1)); done; exit 1"])
            .stderr(Stdio::piped()).spawn().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let error = await_listening(
            &mut child,
            &dir.path().join("absent.sock"),
            &dir.path().join("archond.log"),
        )
        .await
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("initialization failed"));
        assert!(text.len() < 64 * 1024 + 1024);
    }
}
