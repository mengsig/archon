//! `archon daemon run|start|stop|status` and `archon mcp`.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Request};
use archon_daemon::DaemonOptions;

use super::{emit, instance};
use crate::cli::DaemonCommand;
use crate::connect::{archon_bin, connect_existing, connect_or_start};
use crate::instance::{Instance, create_private_dir, effective_aether};

const STOP_WAIT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(100);
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
        } => stop(json, aether, keep_aether).await,
        DaemonCommand::Status => status(json).await,
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
    // Dedicated mode: archond talks only to Archon's own aetherd.
    config.aether = effective_aether(&config.aether, &Instance::resolve()?);
    let options = DaemonOptions {
        config,
        archon_bin: archon_bin()?,
    };
    archon_daemon::run(options).await?;
    Ok(ExitCode::SUCCESS)
}

/// A daemon run directly is the operator's choice even where the locations
/// mix moved and default ones, which autostart refuses; it is still said.
fn warn_if_mixed() {
    if let Some(mix) = archon_core::paths::partial_override(&archon_client::default_socket()) {
        eprintln!("archon: warning: {mix}. Starting archond anyway, as it was asked for directly.");
    }
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
        .stderr(Stdio::null())
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
    loop {
        if let Some(status) = child.try_wait().context("waiting for archond")? {
            anyhow::bail!(
                "archond (pid {}) exited with {status} before it listened; startup log: {}",
                child.id(),
                log.display()
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

/// Stop archond, then in dedicated mode Archon's own aetherd unless `keep_aether`.
/// Shared mode owns no aetherd, so `--aether` is refused there before anything stops.
async fn stop(json: bool, aether: bool, keep_aether: bool) -> anyhow::Result<ExitCode> {
    assert!(!(aether && keep_aether), "clap refuses both");
    if aether && Config::load_default()?.aether.mode != AetherMode::Dedicated {
        return Err(archon_core::ArchonError::Invalid(
            "--aether stops Archon's own aetherd, and [aether] mode is \"shared\": \
             the operator's aetherd is theirs to stop"
                .into(),
        )
        .into());
    }
    let (mut value, mut text) = stop_archond().await?;
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

async fn stop_archond() -> anyhow::Result<(serde_json::Value, String)> {
    let Some(client) = connect_existing().await? else {
        let text = "archond is not running.".to_owned();
        return Ok((serde_json::json!({"stopped": false}), text));
    };
    let pid = client.welcome().pid;
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

/// Serve MCP on stdio for Aether; never prints anything else to stdout.
pub async fn mcp() -> anyhow::Result<ExitCode> {
    let socket = archon_client::default_socket();
    archon_mcp::serve_stdio(&socket, &archon_bin()?).await?;
    Ok(ExitCode::SUCCESS)
}
