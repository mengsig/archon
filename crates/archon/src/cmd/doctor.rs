//! `archon doctor` checks the pieces Archon depends on; in shared mode
//! `archon setup` registers the MCP server and then runs the MCP checks.

mod registry;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use archon_aether::wire::{
    ALLOWLIST_PROTOCOL_VERSION, MIN_PROTOCOL_VERSION, OPERATOR_LOGINS_PROTOCOL_VERSION,
    PROTOCOL_VERSION, Welcome,
};
use archon_aether::{AetherClient, ClientOptions, Endpoint};
use archon_client::ArchonClient;
use archon_core::config::{AetherMode, Config, Toggle};
use archon_core::error::ArchonError;
use archon_core::mcp_tools::SERVER_NAME;
use archon_core::protocol::{McpHealth, gib};
use archon_daemon::context::DaemonPaths;
use archon_daemon::disk::{self, DiskReading};

use crate::connect::{archon_bin, connect_existing, print_json};
use crate::instance::seed::voice::{self, VoiceFile};
use crate::instance::seed::{self, LinkState, calm};
use crate::instance::{Instance, effective_aether, operator_config_dir};
use crate::setup::{self, Registration};
use crate::style::{Palette, Style};
use registry::{COMMAND_TIMEOUT, Registry, run_bounded};

const AETHER_TIMEOUT: Duration = Duration::from_secs(5);
/// The check-name column; the longest name is `mcp registered`.
const NAME_WIDTH: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Check {
    name: &'static str,
    level: Level,
    detail: String,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
    }
}

pub async fn run(json: bool) -> anyhow::Result<ExitCode> {
    let (config_check, config) = check_config();
    let instance = Instance::resolve()?;
    let dedicated = config.aether.mode == AetherMode::Dedicated;
    let mut checks = vec![config_check];
    checks.extend(check_legacy_config(&archon_core::paths::config_dir()));
    checks.push(check_mode(&config, &instance));
    let aether = connect_aether(&effective_aether(&config.aether, &instance)).await;
    checks.push(match &aether {
        Ok(client) => aether_ok(client.welcome(), config.aether.mode),
        Err(_) if dedicated && !crate::instance::lifecycle::is_listening(&instance.socket) => {
            let detail = format!(
                "Archon's aetherd is not running ({}); any archon command starts it",
                instance.socket.display()
            );
            check("aether", Level::Warn, detail)
        }
        Err(error) => check("aether", Level::Fail, error.clone()),
    });
    let registered_in = match config.aether.mode {
        AetherMode::Dedicated => instance.config_path(),
        AetherMode::Shared => setup::aether_config_path(),
    };
    checks.push(check_registration(
        &registered_in,
        &archon_bin()?,
        config.aether.mode,
    ));
    if dedicated {
        checks.push(check_links(&operator_config_dir()?, &instance));
        checks.push(check_calm(&instance));
    }
    checks.push(check_mcp_health(aether.as_ref().ok()).await);
    let archond = connect_existing().await;
    checks.push(check_archond(&archond));
    let registry = Registry::read(&archond, config.aether.mode).await;
    checks.push(check_disk(&config)?);
    checks.push(check_git().await);
    checks.push(registry::check_gh(&config.github.binary, &registry).await);
    checks.push(registry::check_repos(&registry).await);
    checks.push(check_lavish(&config));
    checks.push(check_agent_cli("claude"));
    checks.push(check_agent_cli("codex"));
    report(&checks, json)?;
    Ok(exit_code(&checks))
}

/// Failure when any check failed; warnings alone pass.
fn exit_code(checks: &[Check]) -> ExitCode {
    assert!(!checks.is_empty(), "doctor always runs checks");
    match checks.iter().any(|c| c.level == Level::Fail) {
        true => ExitCode::FAILURE,
        false => ExitCode::SUCCESS,
    }
}

/// Shared mode: register the MCP server in the operator's Aether config,
/// then check whether their aetherd has loaded it.
pub(super) async fn setup_shared(config: &Config, json: bool) -> anyhow::Result<ExitCode> {
    assert_eq!(config.aether.mode, AetherMode::Shared);
    let bin = archon_bin()?;
    let path = setup::aether_config_path();
    let registration = setup::register(&path, &bin)?;
    let aether = connect_aether(&config.aether).await;
    let health = check_mcp_health(aether.as_ref().ok()).await;
    if json {
        let (state, command) = match &registration {
            Registration::Added => ("added", Some(bin.display().to_string())),
            Registration::AlreadyPresent(server) => ("already_present", server.command.clone()),
        };
        return print_json(&serde_json::json!({
            "mode": "shared",
            "config": path,
            "registration": state,
            "command": command,
            "mcp_health": check_json(&health),
        }))
        .map(|()| ExitCode::SUCCESS);
    }
    match registration {
        Registration::Added => println!("Registered the archon MCP server in {}.", path.display()),
        Registration::AlreadyPresent(server) => {
            println!("{} already declares an archon MCP server.", path.display());
            if let Some((_, problem)) = registration_problem(&server, &bin, AetherMode::Shared) {
                println!("Left unchanged, but {problem}.");
            }
        }
    }
    report(std::slice::from_ref(&health), false)?;
    if health.level != Level::Ok {
        println!("Restart aetherd so it loads the server, then run `archon doctor` to verify.");
    }
    Ok(ExitCode::SUCCESS)
}

fn check_mode(config: &Config, instance: &Instance) -> Check {
    let detail = match config.aether.mode {
        AetherMode::Dedicated => format!(
            "dedicated: Archon's own aetherd ({}, socket {})",
            instance.config_dir.display(),
            instance.socket.display()
        ),
        AetherMode::Shared => {
            "shared: the operator's aetherd, whose agents all see Archon's tools".to_owned()
        }
    };
    check("mode", Level::Ok, detail)
}

/// The instance's links to the operator's Aether files, its own `voice.toml`,
/// and whether their stored credentials (never shared, §4.7) exist without its.
fn check_links(operator: &Path, instance: &Instance) -> Check {
    const NAME: &str = "aether links";
    let mut level = Level::Ok;
    let mut notes = Vec::new();
    for name in seed::LINKED {
        let state = match seed::inspect_link(operator, &instance.config_dir, name) {
            Ok(state) => state,
            Err(e) => return check(NAME, Level::Fail, format!("{e:#}")),
        };
        let this = match &state {
            LinkState::Linked | LinkState::NoSource => Level::Ok,
            LinkState::OwnFile if name != "providers.toml" => Level::Ok,
            LinkState::Dangling => Level::Fail,
            _ => Level::Warn,
        };
        level = worst(level, this);
        if state != LinkState::NoSource {
            let text = super::instance::link_text(&state, &operator.join(name));
            notes.push(format!("{name} {text}"));
        }
    }
    match voice_note(operator, instance) {
        Ok((this, note)) => {
            level = worst(level, this);
            notes.extend(note);
        }
        Err(e) => return check(NAME, Level::Fail, format!("{e:#}")),
    }
    let credentials = seed::CREDENTIALS;
    if operator.join(credentials).is_file() && !instance.config_dir.join(credentials).is_file() {
        level = worst(level, Level::Warn);
        notes.push(format!(
            "{credentials} is not shared; `archon aether login <provider>` if Archon needs it"
        ));
    }
    assert!(
        level != Level::Fail || !notes.is_empty(),
        "a failure says why"
    );
    let detail = if notes.is_empty() {
        format!("nothing to share in {}", operator.display())
    } else {
        notes.join("; ")
    };
    check(NAME, level, detail)
}

/// The instance's `voice.toml`, which must be its own regular file (§4.7):
/// the level, and a note unless neither side has one.
fn voice_note(operator: &Path, instance: &Instance) -> anyhow::Result<(Level, Option<String>)> {
    let source = operator.join(voice::FILE);
    let dest = instance.config_dir.join(voice::FILE);
    let file = voice::inspect(operator, &instance.config_dir)?;
    let has_source = source
        .try_exists()
        .with_context(|| format!("checking {}", source.display()))?;
    let (level, text) = match (&file, has_source) {
        (VoiceFile::Absent, false) => return Ok((Level::Ok, None)),
        (VoiceFile::Absent, true) => (
            Level::Warn,
            format!("not copied from {}; run `archon setup`", source.display()),
        ),
        (VoiceFile::OwnFile, _) => (Level::Ok, "the instance's own file".to_owned()),
        (other, _) => (
            Level::Warn,
            super::instance::voice_file_text(other, &source, &dest),
        ),
    };
    Ok((level, Some(format!("{} {text}", voice::FILE))))
}

fn check_calm(instance: &Instance) -> Check {
    match calm::inspect(&instance.config_dir) {
        Ok(state) => check(
            "calm mode",
            if matches!(state, calm::State::Builtin | calm::State::Edited) {
                Level::Ok
            } else {
                Level::Warn
            },
            format!(
                "{}: {}",
                instance.config_dir.join(calm::FILE).display(),
                state.description()
            ),
        ),
        Err(error) => check("calm mode", Level::Fail, format!("{error:#}")),
    }
}

fn worst(a: Level, b: Level) -> Level {
    match (a, b) {
        (Level::Fail, _) | (_, Level::Fail) => Level::Fail,
        (Level::Warn, _) | (_, Level::Warn) => Level::Warn,
        _ => Level::Ok,
    }
}

pub(super) fn report(checks: &[Check], json: bool) -> anyhow::Result<()> {
    assert!(!checks.is_empty(), "doctor always runs checks");
    if json {
        let value: Vec<_> = checks.iter().map(check_json).collect();
        return print_json(&serde_json::Value::Array(value));
    }
    let palette = Palette::detect();
    for c in checks {
        let (style, label) = match c.level {
            Level::Ok => (Style::Green, "ok  "),
            Level::Warn => (Style::Yellow, "warn"),
            Level::Fail => (Style::Red, "FAIL"),
        };
        println!(
            "{}  {:<NAME_WIDTH$} {}",
            palette.paint(style, label),
            c.name,
            c.detail
        );
    }
    Ok(())
}

pub(super) fn check_json(c: &Check) -> serde_json::Value {
    serde_json::json!({"check": c.name, "level": level_name(c.level), "detail": c.detail})
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "fail",
    }
}

/// The config check, and the config to use for the rest (defaults when it
/// does not load, so the other checks still run).
fn check_config() -> (Check, Config) {
    let path = archon_core::paths::config_dir().join(archon_core::config::CONFIG_FILE_NAME);
    match Config::load_default().and_then(|c| c.validate().map(|()| c)) {
        Ok(config) => {
            let origin = if path.exists() {
                ""
            } else {
                " (missing: defaults)"
            };
            (
                check("config", Level::Ok, format!("{}{origin}", path.display())),
                config,
            )
        }
        Err(e) => (
            check("config", Level::Fail, format!("{}: {e}", path.display())),
            Config::default(),
        ),
    }
}

/// An `archon.toml` from before 1.0, which the operator may believe
/// configures Archon. `None` when there is none.
fn check_legacy_config(config_dir: &Path) -> Option<Check> {
    archon_core::config_files::legacy_config_notice(config_dir)
        .map(|notice| check("legacy config", Level::Warn, notice))
}

pub(super) async fn connect_aether(
    config: &archon_core::config::AetherConfig,
) -> Result<AetherClient, String> {
    let endpoint = Endpoint::from_config(config).map_err(|e| e.to_string())?;
    let options = ClientOptions {
        request_timeout: AETHER_TIMEOUT,
        ..ClientOptions::default()
    };
    let where_ = match &endpoint {
        Endpoint::Unix(path) => path.display().to_string(),
        Endpoint::Tcp { address, .. } => address.clone(),
    };
    match tokio::time::timeout(
        AETHER_TIMEOUT,
        AetherClient::connect_with(&endpoint, options),
    )
    .await
    {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(e)) => Err(format!("{where_}: {e}")),
        Err(_) => Err(format!("{where_}: no welcome within {AETHER_TIMEOUT:?}")),
    }
}

/// Whether Archon can use this aetherd. Only a dedicated one runs Archon's
/// GitHub watcher, so only there does a list it cannot store matter.
fn aether_ok(welcome: &Welcome, mode: AetherMode) -> Check {
    let served = MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION;
    if !served.contains(&welcome.protocol_version) {
        let detail = format!(
            "aetherd {} speaks protocol {}; Archon needs {}..{}",
            welcome.daemon_version,
            welcome.protocol_version,
            MIN_PROTOCOL_VERSION,
            PROTOCOL_VERSION
        );
        return check("aether", Level::Fail, detail);
    }
    let detail = format!(
        "aetherd {} (protocol {}, instance {})",
        welcome.daemon_version, welcome.protocol_version, welcome.instance_id
    );
    let gap = match mode {
        AetherMode::Dedicated => watcher_gap(welcome.protocol_version),
        AetherMode::Shared => None,
    };
    match gap {
        Some(gap) => check("aether", Level::Warn, format!("{detail}; {gap}")),
        None => check("aether", Level::Ok, detail),
    }
}

/// What Archon's GitHub watcher cannot store at `protocol`, with the fix.
fn watcher_gap(protocol: u32) -> Option<String> {
    let lacking = if protocol < ALLOWLIST_PROTOCOL_VERSION {
        format!(
            "GitHub watcher allowlists need protocol {ALLOWLIST_PROTOCOL_VERSION} and operator \
             logins {OPERATOR_LOGINS_PROTOCOL_VERSION}"
        )
    } else if protocol < OPERATOR_LOGINS_PROTOCOL_VERSION {
        format!(
            "GitHub operator logins (`archon github-watcher operators`) need protocol \
             {OPERATOR_LOGINS_PROTOCOL_VERSION}"
        )
    } else {
        return None;
    };
    Some(format!(
        "{lacking}; to set them, install a newer `aether`, then `archon daemon stop --aether` \
         (the next archon command starts it)"
    ))
}

fn check_registration(path: &Path, archon_bin: &Path, mode: AetherMode) -> Check {
    const NAME: &str = "mcp registered";
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let detail = format!("{} is missing; run `archon setup`", path.display());
            return check(NAME, Level::Fail, detail);
        }
        Err(e) => return check(NAME, Level::Fail, format!("{}: {e}", path.display())),
    };
    let server = match setup::find_archon_server(&text) {
        Ok(Some(server)) => server,
        Ok(None) => {
            let detail = format!(
                "{} declares no `archon` server; run `archon setup`",
                path.display()
            );
            return check(NAME, Level::Fail, detail);
        }
        Err(e) => {
            let detail = format!("{} is not valid TOML: {e}", path.display());
            return check(NAME, Level::Fail, detail);
        }
    };
    match registration_problem(&server, archon_bin, mode) {
        None => check(NAME, Level::Ok, path.display().to_string()),
        Some((level, problem)) => check(NAME, level, format!("{}: {problem}", path.display())),
    }
}

/// What is wrong with the registered server, if anything. A command that
/// cannot run comes first: nothing else matters until it does.
fn registration_problem(
    server: &setup::ArchonServer,
    archon_bin: &Path,
    mode: AetherMode,
) -> Option<(Level, String)> {
    let command = command_problem(server.command.as_deref(), archon_bin);
    if server.names_callers || matches!(command, Some((Level::Fail, _))) {
        return command;
    }
    let fix = match mode {
        AetherMode::Dedicated => {
            "the next archon command that starts Archon (`archon status`) repairs it and \
             restarts Archon's aetherd"
        }
        AetherMode::Shared => {
            "add it to that server, then restart aetherd, which reads it only as it starts"
        }
    };
    let problem = format!(
        "its `archon` server lacks `send_caller_meta = true`, so archond cannot tell its \
         orchestrator from other agents: {fix}"
    );
    Some((Level::Fail, problem))
}

/// What is wrong with the registered `command`, if anything: a command that
/// cannot run fails; another archon than this one is worth a warning.
fn command_problem(command: Option<&str>, archon_bin: &Path) -> Option<(Level, String)> {
    assert!(archon_bin.is_absolute(), "this archon's path is canonical");
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        let problem = "its `archon` server has no `command`".to_owned();
        return Some((Level::Fail, problem));
    };
    let Some(found) = find_program(Path::new(command)) else {
        let problem = format!("its `archon` command {command} is not an executable file");
        return Some((Level::Fail, problem));
    };
    let same = found.canonicalize().is_ok_and(|path| path == archon_bin);
    if same {
        return None;
    }
    let problem = format!(
        "its `archon` command is {command}, not this archon ({})",
        archon_bin.display()
    );
    Some((Level::Warn, problem))
}

/// Aether's own view of the `archon` server (`mcp_servers`).
pub(super) async fn check_mcp_health(aether: Option<&AetherClient>) -> Check {
    const NAME: &str = "mcp health";
    let Some(client) = aether else {
        return check(NAME, Level::Warn, "not checked: Aether is unreachable");
    };
    match client.mcp_health(SERVER_NAME).await {
        Ok(health) => mcp_verdict(&health),
        Err(e) => check(NAME, Level::Fail, format!("mcp_servers failed: {e}")),
    }
}

fn mcp_verdict(health: &McpHealth) -> Check {
    const NAME: &str = "mcp health";
    match health {
        McpHealth::Ready { tools } => {
            check(NAME, Level::Ok, format!("archon is ready ({tools} tools)"))
        }
        McpHealth::Failed { error } => check(
            NAME,
            Level::Fail,
            format!("archon failed in aetherd: {error}"),
        ),
        McpHealth::Unknown => check(
            NAME,
            Level::Warn,
            "loaded, not started yet (starts when an agent first uses it)",
        ),
        McpHealth::NotLoaded => check(
            NAME,
            Level::Fail,
            "aetherd has not loaded the archon server; restart aetherd after `archon setup`",
        ),
        McpHealth::Unreachable { error } => check(NAME, Level::Fail, error.clone()),
    }
}

fn check_archond(archond: &anyhow::Result<Option<ArchonClient>>) -> Check {
    match archond {
        Ok(Some(client)) => {
            let welcome = client.welcome();
            check(
                "archond",
                Level::Ok,
                format!("{} running (pid {})", welcome.daemon_version, welcome.pid),
            )
        }
        Ok(None) => check(
            "archond",
            Level::Warn,
            "not running; any archon command starts it",
        ),
        Err(e) => check("archond", Level::Fail, e.to_string()),
    }
}

/// Free space on the filesystems of the data dir and the worktree root,
/// against `limits.min_free_gb`, below which archond holds dispatch.
fn check_disk(config: &Config) -> anyhow::Result<Check> {
    let paths = DaemonPaths::resolve(config, archon_bin()?);
    let watched = [paths.data_dir, paths.worktrees];
    let probe: disk::FreeSpaceProbe = std::sync::Arc::new(disk::free_bytes);
    let reading = disk::lowest(&probe, &watched);
    Ok(disk_verdict(reading, config.limits.min_free_bytes()))
}

fn disk_verdict(reading: Result<DiskReading, ArchonError>, min_free_bytes: u64) -> Check {
    let reading = match reading {
        Ok(reading) => reading,
        Err(error) => return check("disk", Level::Warn, format!("free space unknown: {error}")),
    };
    let free = format!(
        "{} free at {}",
        gib(reading.free_bytes),
        reading.path.display()
    );
    if min_free_bytes == 0 {
        return check(
            "disk",
            Level::Ok,
            format!("{free}; the low-disk hold is off"),
        );
    }
    let floor = gib(min_free_bytes);
    if reading.free_bytes < min_free_bytes {
        let detail = format!(
            "{free}, below limits.min_free_gb ({floor}): dispatch is held; run `archon gc`"
        );
        return check("disk", Level::Warn, detail);
    }
    check(
        "disk",
        Level::Ok,
        format!("{free}; dispatch holds below {floor}"),
    )
}

async fn check_git() -> Check {
    match run_bounded(Path::new("git"), &["--version"], None, COMMAND_TIMEOUT).await {
        Ok(out) if out.status.success() => check(
            "git",
            Level::Ok,
            String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        ),
        Ok(out) => check(
            "git",
            Level::Fail,
            format!("`git --version` exited with {}", out.status),
        ),
        Err(e) => check("git", Level::Fail, e),
    }
}

fn check_lavish(config: &Config) -> Check {
    let lavish = &config.lavish;
    if lavish.enabled == Toggle::Off {
        return check("lavish", Level::Ok, "disabled in config");
    }
    match (find_program(&lavish.binary), lavish.enabled) {
        (Some(path), _) => check("lavish", Level::Ok, path.display().to_string()),
        (None, Toggle::On) => check(
            "lavish",
            Level::Fail,
            format!("{} not found (enabled = \"on\")", lavish.binary.display()),
        ),
        (None, _) => check(
            "lavish",
            Level::Warn,
            format!(
                "{} not found; reports open without it",
                lavish.binary.display()
            ),
        ),
    }
}

fn check_agent_cli(name: &'static str) -> Check {
    match find_program(Path::new(name)) {
        Some(path) => check(name, Level::Ok, path.display().to_string()),
        None => check(name, Level::Warn, "not on PATH; agents using it cannot run"),
    }
}

/// An executable named `program`: a path as given, or a bare name on PATH.
pub(super) fn find_program(program: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.components().count() > 1 {
        return executable(program).then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_states_map_to_levels() {
        let ready = McpHealth::Ready { tools: 2 };
        assert_eq!(mcp_verdict(&ready).level, Level::Ok);
        assert!(mcp_verdict(&ready).detail.contains("2 tools"));
        let failed = McpHealth::Failed {
            error: "exit 1".into(),
        };
        assert_eq!(mcp_verdict(&failed).level, Level::Fail);
        assert!(mcp_verdict(&failed).detail.contains("exit 1"));
        assert_eq!(mcp_verdict(&McpHealth::Unknown).level, Level::Warn);
        assert_eq!(mcp_verdict(&McpHealth::NotLoaded).level, Level::Fail);
    }

    #[test]
    fn low_or_unknown_disk_space_warns_and_zero_turns_the_check_off() {
        let gib = 1u64 << 30;
        let reading = |free_bytes| {
            Ok(DiskReading {
                path: PathBuf::from("/data"),
                free_bytes,
            })
        };
        let low = disk_verdict(reading(2 * gib), 10 * gib);
        assert_eq!(low.level, Level::Warn);
        assert!(
            low.detail.contains("2.0 GiB free at /data"),
            "{}",
            low.detail
        );
        assert!(low.detail.contains("archon gc"), "{}", low.detail);
        assert_eq!(disk_verdict(reading(20 * gib), 10 * gib).level, Level::Ok);
        assert_eq!(disk_verdict(reading(0), 0).level, Level::Ok);
        let unknown = disk_verdict(Err(ArchonError::Invalid("x".into())), 10 * gib);
        assert_eq!(unknown.level, Level::Warn);
    }

    fn welcome(protocol_version: u32) -> Welcome {
        serde_json::from_value(serde_json::json!({
            "protocol_version": protocol_version, "daemon_version": "0.9.0",
            "instance_id": "i-1", "schema_version": 1, "pid": 7,
        }))
        .unwrap()
    }

    #[test]
    fn a_dedicated_aetherd_on_protocol_14_warns_that_allowlists_need_15() {
        let dedicated = AetherMode::Dedicated;
        let current = aether_ok(&welcome(17), dedicated);
        assert_eq!(current.level, Level::Ok);
        assert!(current.detail.ends_with("(protocol 17, instance i-1)"));
        let older = aether_ok(&welcome(14), dedicated);
        assert_eq!(older.level, Level::Warn);
        assert!(
            older
                .detail
                .contains("allowlists need protocol 15 and operator logins 17"),
            "{}",
            older.detail
        );
        assert_eq!(aether_ok(&welcome(13), dedicated).level, Level::Fail);
        assert_eq!(aether_ok(&welcome(18), dedicated).level, Level::Fail);
    }

    #[test]
    fn a_dedicated_aetherd_below_protocol_17_warns_that_operator_logins_need_it() {
        for protocol in [15, 16] {
            let older = aether_ok(&welcome(protocol), AetherMode::Dedicated);
            assert_eq!(older.level, Level::Warn, "protocol {protocol}");
            let detail = &older.detail;
            assert!(
                detail.contains(&format!("(protocol {protocol}, instance i-1)")),
                "{detail}"
            );
            assert!(
                detail.contains(
                    "GitHub operator logins (`archon github-watcher operators`) need protocol 17"
                ),
                "{detail}"
            );
            assert!(detail.contains("archon daemon stop --aether"), "{detail}");
            assert!(!detail.contains("allowlist"), "allowlists work: {detail}");
        }
    }

    /// Archon runs no GitHub watcher on a shared aetherd, so 14 is fine there.
    #[test]
    fn a_shared_aetherd_on_protocol_14_is_ok() {
        let shared = AetherMode::Shared;
        let older = aether_ok(&welcome(14), shared);
        assert_eq!(older.level, Level::Ok);
        assert!(!older.detail.contains("allowlist"), "{}", older.detail);
        assert!(older.detail.contains("protocol 14"), "{}", older.detail);
        for protocol in [15, 16, 17] {
            let check = aether_ok(&welcome(protocol), shared);
            assert_eq!(check.level, Level::Ok, "protocol {protocol}");
            assert!(
                !check.detail.contains("operator logins"),
                "{}",
                check.detail
            );
        }
        assert_eq!(aether_ok(&welcome(13), shared).level, Level::Fail);
    }

    #[test]
    fn only_a_failed_check_fails_the_doctor() {
        let ok = check("git", Level::Ok, "");
        let warn = check("archond", Level::Warn, "not running");
        let fail = check("config", Level::Fail, "broken");
        assert_eq!(exit_code(std::slice::from_ref(&ok)), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[ok.clone(), warn.clone()]), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[ok, warn, fail.clone()]), ExitCode::FAILURE);
        assert_eq!(exit_code(&[fail]), ExitCode::FAILURE);
    }

    #[test]
    fn registered_commands_must_run_and_should_be_this_archon() {
        let sh = Path::new("/bin/sh").canonicalize().unwrap();
        assert_eq!(command_problem(Some("/bin/sh"), &sh), None);
        let (level, problem) = command_problem(Some("/bin/sh"), Path::new("/opt/archon")).unwrap();
        assert_eq!(level, Level::Warn);
        assert!(problem.contains("/opt/archon"), "{problem}");
        let missing = command_problem(Some("/nonexistent/archon"), &sh).unwrap();
        assert_eq!(missing.0, Level::Fail);
        assert_eq!(command_problem(None, &sh).unwrap().0, Level::Fail);
    }

    #[test]
    fn a_registration_that_does_not_name_callers_fails_with_the_fix_for_its_mode() {
        let sh = Path::new("/bin/sh").canonicalize().unwrap();
        let server = |names_callers| setup::ArchonServer {
            command: Some("/bin/sh".into()),
            names_callers,
        };
        for mode in [AetherMode::Dedicated, AetherMode::Shared] {
            assert_eq!(registration_problem(&server(true), &sh, mode), None);
        }
        let (level, own) =
            registration_problem(&server(false), &sh, AetherMode::Dedicated).unwrap();
        assert_eq!(level, Level::Fail);
        assert!(
            own.contains("send_caller_meta = true") && own.contains("repairs it"),
            "{own}"
        );
        let (level, shared) =
            registration_problem(&server(false), &sh, AetherMode::Shared).unwrap();
        assert_eq!(level, Level::Fail);
        assert!(shared.contains("restart aetherd"), "{shared}");
        let broken = setup::ArchonServer {
            command: None,
            names_callers: false,
        };
        let (_, first) = registration_problem(&broken, &sh, AetherMode::Shared).unwrap();
        assert!(first.contains("no `command`"), "{first}");
    }

    #[test]
    fn a_legacy_config_file_is_a_warning_that_names_the_file_read() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_legacy_config(dir.path()).is_none());
        std::fs::write(dir.path().join("archon.toml"), "[steward]\n").unwrap();
        let found = check_legacy_config(dir.path()).expect("a finding");
        assert_eq!(found.level, Level::Warn);
        assert!(found.detail.contains("archon.toml is ignored"), "{found:?}");
        let read = dir.path().join("config.toml").display().to_string();
        assert!(found.detail.contains(&read), "{found:?}");
    }

    #[test]
    fn programs_are_found_by_path_or_name() {
        assert!(find_program(Path::new("/bin/sh")).is_some());
        assert!(find_program(Path::new("sh")).is_some());
        assert!(find_program(Path::new("archon-no-such-program")).is_none());
        assert!(find_program(Path::new("/nonexistent/sh")).is_none());
    }
}
