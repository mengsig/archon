//! Commands for Archon's own Aether (ARCHITECTURE §4.7): `setup`, `tui`
//! (which a bare `archon` opens too, see `bare`), `aether …`, and the aetherd
//! half of `daemon status` and `daemon stop`.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use anyhow::Context as _;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::{AetherMode, Config};
use archon_core::config_files;
use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, Request};
use archon_core::{operator_commands, paths};

use super::doctor;
use crate::cli::TuiArgs;
use crate::connect::{archon_bin, print_json};
use crate::instance::lifecycle::{self, Start, Stop};
use crate::instance::seed::voice::{self, VoiceFile, VoiceOutcome};
use crate::instance::seed::{self, ConfigOutcome, LinkState, Seeding, calm};
use crate::instance::{AETHER_PROGRAM, Instance, effective_aether, operator_config_dir};
use crate::setup;

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const HOST_COMMANDS: &str = "--host-commands";
const TOO_OLD: &str =
    "the installed Aether is too old for Archon's terminal UI; update it with `aether update`";

pub async fn setup(print_only: bool, resync: bool, json: bool) -> anyhow::Result<ExitCode> {
    if print_only {
        let block = setup::mcp_block(&archon_bin()?);
        if json {
            print_json(&serde_json::json!({ "block": block }))?;
        } else {
            print!("{block}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    note_config_files(&archon_core::paths::config_dir())?;
    let config = Config::load_default()?;
    match config.aether.mode {
        AetherMode::Shared => {
            if resync {
                return Err(archon_core::ArchonError::Invalid(
                    "--resync re-seeds Archon's own Aether, and [aether] mode is \"shared\"".into(),
                )
                .into());
            }
            doctor::setup_shared(&config, json).await
        }
        AetherMode::Dedicated => setup_dedicated(&config, resync, json).await,
    }
}

/// Give an install without a `config.toml` a commented one to edit, and
/// say so on stderr, as for an `archon.toml` that Archon no longer reads.
fn note_config_files(config_dir: &Path) -> anyhow::Result<()> {
    if let Some(written) = config_files::write_starter(config_dir)? {
        eprintln!(
            "archon: wrote {}. Every key in it is commented out, so nothing changes until \
             you edit it.",
            written.display()
        );
    }
    if let Some(notice) = config_files::legacy_config_notice(config_dir) {
        eprintln!("archon: {notice}");
    }
    Ok(())
}

/// Seed the instance, then say whether its running aetherd (if any) has
/// loaded the archon server.
async fn setup_dedicated(config: &Config, resync: bool, json: bool) -> anyhow::Result<ExitCode> {
    let instance = Instance::resolve()?;
    let operator = operator_config_dir()?;
    let seeding = seed::seed(&instance, &operator, &archon_bin()?, resync)?;
    assert!(instance.config_path().is_file(), "setup leaves a config");
    let running = lifecycle::is_listening(&instance.socket);
    let reloaded = running && reload_if_owed(&instance).await?;
    let aether = doctor::connect_aether(&effective_aether(&config.aether, &instance)).await;
    let health = doctor::check_mcp_health(aether.as_ref().ok()).await;
    if json {
        let links: serde_json::Map<_, _> = seeding
            .links
            .iter()
            .map(|(name, state)| ((*name).to_owned(), link_json(state)))
            .collect();
        return print_json(&serde_json::json!({
            "mode": "dedicated",
            "config": instance.config_path(),
            "source": operator.join(archon_core::config::CONFIG_FILE_NAME),
            "outcome": outcome_name(seeding.config),
            "links": links,
            "voice_toml": voice_json(&seeding.voice),
            "calm_mode": seeding.calm,
            "running": running,
            "reloaded": reloaded,
            "mcp_health": doctor::check_json(&health),
        }))
        .map(|()| ExitCode::SUCCESS);
    }
    print_seeding(&instance, &operator, &seeding);
    doctor::report(std::slice::from_ref(&health), false)?;
    let changed = seeding.config != ConfigOutcome::Unchanged;
    if reloaded {
        println!(
            "Archon's aetherd was restarted to load the updated archon server: \
             Aether reads it only as it starts."
        );
    } else if running && changed {
        println!(
            "Archon's aetherd is running with the old config: `archon daemon stop --aether`, \
             then any archon command starts it again."
        );
    } else if !running {
        println!(
            "Next: `archon` opens the terminal UI, starting Archon and its own aetherd. \
             `aether tui` stays plain Aether."
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Restart a running aetherd whose archon server setup just replaced with
/// one Archon needs. `false` when it had the current one loaded.
async fn reload_if_owed(instance: &Instance) -> anyhow::Result<bool> {
    if !seed::reload_is_owed(instance)? {
        return Ok(false);
    }
    let instance = instance.clone();
    let start = tokio::task::spawn_blocking(move || {
        lifecycle::ensure_running(&instance, Path::new(AETHER_PROGRAM))
    })
    .await
    .context("restarting Archon's own aetherd")??;
    Ok(start == Start::Restarted)
}

fn print_seeding(instance: &Instance, operator: &Path, seeding: &Seeding) {
    let source = operator.join(archon_core::config::CONFIG_FILE_NAME);
    let config = match seeding.config {
        ConfigOutcome::Seeded => format!("seeded from {}", source.display()),
        ConfigOutcome::Resynced => format!("re-seeded from {}", source.display()),
        ConfigOutcome::Unchanged => "already set up; unchanged (`--resync` re-seeds it)".to_owned(),
        ConfigOutcome::BlockAdded => "added the archon MCP server".to_owned(),
        ConfigOutcome::Repaired => {
            "brought its archon MCP server up to date (`send_caller_meta = true`)".to_owned()
        }
    };
    println!("Archon's own Aether: {}", instance.config_dir.display());
    println!("  config.toml     {config}");
    for (name, state) in &seeding.links {
        println!("  {name:<15} {}", link_text(state, &operator.join(name)));
    }
    let source = operator.join(voice::FILE);
    let dest = instance.config_dir.join(voice::FILE);
    let text = voice_text(&seeding.voice, &source, &dest);
    println!("  {:<15} {text}", voice::FILE);
    println!("  {:<15} {}", calm::FILE, seeding.calm);
}

fn outcome_name(outcome: ConfigOutcome) -> &'static str {
    match outcome {
        ConfigOutcome::Seeded => "seeded",
        ConfigOutcome::Resynced => "resynced",
        ConfigOutcome::Unchanged => "unchanged",
        ConfigOutcome::BlockAdded => "block_added",
        ConfigOutcome::Repaired => "repaired",
    }
}

pub(super) fn link_text(state: &LinkState, source: &Path) -> String {
    match state {
        LinkState::Linked => format!("linked to {}", source.display()),
        LinkState::Missing => format!("not linked to {}; run `archon setup`", source.display()),
        LinkState::NoSource => format!("none ({} does not exist)", source.display()),
        LinkState::Dangling => format!("link broken: {} is gone", source.display()),
        LinkState::Elsewhere(target) => {
            format!("links to {}; run `archon setup`", target.display())
        }
        LinkState::OwnFile => "its own copy, not a link".to_owned(),
    }
}

fn link_json(state: &LinkState) -> serde_json::Value {
    let name = match state {
        LinkState::Linked => "linked",
        LinkState::Missing => "missing",
        LinkState::NoSource => "no_source",
        LinkState::Dangling => "dangling",
        LinkState::Elsewhere(_) => "elsewhere",
        LinkState::OwnFile => "own_file",
    };
    serde_json::Value::String(name.to_owned())
}

/// What seeding or healing did to the instance's `voice.toml`.
fn voice_text(outcome: &VoiceOutcome, source: &Path, dest: &Path) -> String {
    match outcome {
        VoiceOutcome::Copied => format!("copied from {}; the instance's own", source.display()),
        VoiceOutcome::Healed => format!(
            "was a link; now the instance's own copy of {}",
            source.display()
        ),
        VoiceOutcome::DanglingRemoved(target) => format!(
            "removed a broken link to {}; `/voice setup` in `archon tui` creates the file",
            target.display()
        ),
        VoiceOutcome::Unchanged(file) => voice_file_text(file, source, dest),
        VoiceOutcome::SetupRunning(file) => format!(
            "{}; not changed now, because a `/voice setup` is running on this instance: \
             run `archon setup` when it has finished",
            voice_pending_text(file, source)
        ),
    }
}

/// What a running `/voice setup` kept seeding from changing.
fn voice_pending_text(file: &VoiceFile, source: &Path) -> String {
    match file {
        VoiceFile::Absent => format!("none yet ({} is not copied)", source.display()),
        VoiceFile::OperatorLink => format!("links to {}", source.display()),
        VoiceFile::Dangling(target) => format!("broken link to {}", target.display()),
        VoiceFile::OwnFile | VoiceFile::Elsewhere(_) | VoiceFile::NotAFile => {
            unreachable!("{file:?} needs no change, so no lock is asked for")
        }
    }
}

/// What the instance's `voice.toml` is, and the fix where it needs one.
pub(super) fn voice_file_text(file: &VoiceFile, source: &Path, dest: &Path) -> String {
    match file {
        VoiceFile::Absent => format!("none ({} does not exist)", source.display()),
        VoiceFile::OwnFile => "the instance's own file; left as it is".to_owned(),
        VoiceFile::OperatorLink => format!(
            "links to {}, which Aether's `/voice setup` refuses; \
             `archon setup` or `archon tui` replaces the link with a copy",
            source.display()
        ),
        VoiceFile::Dangling(target) => format!(
            "broken link to {}; `archon setup` or `archon tui` removes it",
            target.display()
        ),
        VoiceFile::Elsewhere(target) => format!(
            "links to {}, not {}; left alone, but Aether's `/voice setup` refuses a link: \
             remove {}, then run `archon setup`",
            target.display(),
            source.display(),
            dest.display()
        ),
        VoiceFile::NotAFile => format!(
            "{} is not a regular file; left alone: move it away, then run `archon setup`",
            dest.display()
        ),
    }
}

fn voice_json(outcome: &VoiceOutcome) -> &'static str {
    match outcome {
        VoiceOutcome::Copied => "copied",
        VoiceOutcome::Healed => "healed",
        VoiceOutcome::DanglingRemoved(_) => "dangling_removed",
        VoiceOutcome::Unchanged(VoiceFile::Absent) => "no_source",
        VoiceOutcome::Unchanged(VoiceFile::OwnFile) => "own_file",
        VoiceOutcome::Unchanged(VoiceFile::OperatorLink) => "linked",
        VoiceOutcome::Unchanged(VoiceFile::Dangling(_)) => "dangling",
        VoiceOutcome::Unchanged(VoiceFile::Elsewhere(_)) => "elsewhere",
        VoiceOutcome::Unchanged(VoiceFile::NotAFile) => "not_a_file",
        VoiceOutcome::SetupRunning(_) => "setup_running",
    }
}

/// Before the TUI starts: give the instance its own `voice.toml` where an older
/// setup linked it. What cannot be healed is reported and the TUI still starts.
fn heal_voice(config: &Config) -> anyhow::Result<()> {
    if config.aether.mode != AetherMode::Dedicated {
        return Ok(());
    }
    let instance = Instance::resolve()?;
    let operator = operator_config_dir()?;
    let source = operator.join(voice::FILE);
    let dest = instance.config_dir.join(voice::FILE);
    let outcome = voice::heal(&operator, &instance.config_dir).with_context(|| {
        format!(
            "{} could not be made the instance's own file, which Aether's `/voice setup` \
             needs; remove it or fix the cause, then run `archon tui` again",
            dest.display()
        )
    })?;
    let untouched = matches!(
        outcome,
        VoiceOutcome::Unchanged(VoiceFile::Absent | VoiceFile::OwnFile)
    );
    if !untouched {
        eprintln!(
            "archon: {} {}",
            voice::FILE,
            voice_text(&outcome, &source, &dest)
        );
    }
    Ok(())
}

/// `aether tui` on Archon's own Aether, started first if needed.
pub async fn tui(args: TuiArgs) -> anyhow::Result<ExitCode> {
    exec_aether(tui_command(args, false).await?)
}

/// As [`tui`], but an `aether` that does not take the options is an error
/// here: once it has replaced this process, Archon can add nothing to its own.
pub async fn tui_checked(args: TuiArgs) -> anyhow::Result<ExitCode> {
    exec_aether(tui_command(args, true).await?)
}

/// The `aether tui` to run, with the orchestrator attached and the instance
/// ready; `checked` refuses an `aether` that lacks an option it passes.
async fn tui_command(mut args: TuiArgs, checked: bool) -> anyhow::Result<Command> {
    let config = Config::load_default()?;
    anyhow::ensure!(
        config.aether.address.is_none(),
        "`archon tui` does not support [aether] address (TCP); use a shared Unix socket or dedicated mode"
    );
    heal_voice(&config)?;
    if config.aether.mode == AetherMode::Dedicated {
        let instance = Instance::resolve()?;
        let calm = calm::seed(&instance.config_dir).and_then(|_| {
            let state = calm::inspect(&instance.config_dir)?;
            anyhow::ensure!(
                matches!(state, calm::State::Builtin | calm::State::Edited),
                "{}",
                state.description()
            );
            Ok(())
        });
        if let Err(error) = calm {
            eprintln!("Warning: calm view unavailable ({error:#}); using standard view mode.");
            args.normal = true;
        }
    }
    let client = crate::connect::connect_or_start().await?;
    let agent = attach(&client).await.context(
        "attaching to Archon's orchestrator; try `archon tui` again after Aether reconnects",
    )?;
    drop(client);
    let mut command = aether_command(&config, &tui_args(&agent, &args, config.aether.mode))?;
    if let Some(socket) = &config.aether.socket {
        command.env("AETHER_SOCKET", socket);
    }
    let dedicated = config.aether.mode == AetherMode::Dedicated;
    if !checked && !dedicated {
        return Ok(command);
    }
    let help = match tui_help(&command).await {
        Ok(help) => help,
        Err(error) if !checked => {
            eprintln!("Warning: Archon's commands are not in the terminal UI's / menu: {error:#}");
            return Ok(command);
        }
        Err(error) => return Err(error),
    };
    if checked {
        ensure_tui_options(&command, &help)?;
    }
    if dedicated {
        offer_host_commands(&mut command, &help, &paths::tui_host_commands_path());
    }
    Ok(command)
}

/// Write Archon's operator commands to `path` and pass it as
/// `--host-commands`, so `/answer` and the rest join the TUI's / menu. An
/// `aether` that predates the option, or a failed write, costs only the menu.
fn offer_host_commands(command: &mut Command, help: &str, path: &Path) {
    if !missing_options(help, [OsStr::new(HOST_COMMANDS)].into_iter()).is_empty() {
        eprintln!(
            "Warning: this Aether's terminal UI takes no {HOST_COMMANDS}, so /answer and \
             Archon's other commands are not in its / menu; update it with `aether update`."
        );
        return;
    }
    if let Err(error) = write_host_commands(path) {
        eprintln!("Warning: Archon's commands are not in the terminal UI's / menu: {error:#}");
        return;
    }
    command.arg(HOST_COMMANDS).arg(path);
}

fn write_host_commands(path: &Path) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("the host commands path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = serde_json::to_string_pretty(&operator_commands::host_commands_json())?;
    seed::write_atomically(path, &text)
}

/// The `aether tui --help` of the `aether` that `command` runs.
async fn tui_help(command: &Command) -> anyhow::Result<String> {
    let mut probe = Command::new(command.get_program());
    probe.args(["tui", "--help"]).stdin(Stdio::null());
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => probe.env(name, value),
            None => probe.env_remove(name),
        };
    }
    let mut probe = tokio::process::Command::from(probe);
    let asked = format!("`{AETHER_PROGRAM} tui --help`");
    let output = tokio::time::timeout(PROBE_TIMEOUT, probe.kill_on_drop(true).output())
        .await
        .map_err(|_| anyhow::anyhow!("{asked} did not finish within {PROBE_TIMEOUT:?}"))?
        .map_err(aether_not_run)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let said = stderr.lines().map(str::trim).find(|line| !line.is_empty());
        let said = said.unwrap_or("it said nothing");
        anyhow::bail!("{asked} failed ({}): {said}; {TOO_OLD}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Refuse an `aether` whose `tui --help` lacks an option `command` passes.
fn ensure_tui_options(command: &Command, help: &str) -> anyhow::Result<()> {
    let mut args = command.get_args();
    assert_eq!(args.next(), Some(OsStr::new("tui")), "an `aether tui`");
    let missing = missing_options(help, args);
    anyhow::ensure!(
        missing.is_empty(),
        "`{AETHER_PROGRAM} tui --help` lists no {}; {TOO_OLD}",
        missing.join(" or ")
    );
    Ok(())
}

/// The `--options` among `args` that `help` (clap's option list) does not name.
fn missing_options<'a>(help: &str, args: impl Iterator<Item = &'a OsStr>) -> Vec<&'a str> {
    let listed = |option: &str| {
        help.lines()
            .any(|line| line.split_whitespace().take(2).any(|word| word == option))
    };
    args.filter_map(OsStr::to_str)
        .filter(|arg| arg.starts_with("--") && !listed(arg))
        .collect()
}

async fn attach(client: &archon_client::ArchonClient) -> anyhow::Result<AetherAgentId> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let timeout = remaining.max(std::time::Duration::from_millis(1));
        match client
            .request_within(Request::OrchestratorAttach, timeout)
            .await
        {
            Ok(Reply::Orchestrator(info)) => return Ok(info.agent),
            Ok(other) => return Err(crate::connect::unexpected(&other, "orchestrator")),
            // A freshly started archond may serve before its Aether link is ready.
            Err(ArchonError::Unavailable(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn tui_args(agent: &AetherAgentId, args: &TuiArgs, mode: AetherMode) -> Vec<OsString> {
    let mut argv = vec!["tui".into(), "--agent".into(), agent.as_str().into()];
    if args.normal || mode == AetherMode::Dedicated {
        argv.extend([
            "--view-mode".into(),
            if args.normal { "standard" } else { "calm" }.into(),
        ]);
    }
    if let Some(theme) = &args.theme {
        argv.extend(["--theme".into(), theme.into()]);
    }
    for (enabled, flag) in [
        (args.ascii, "--ascii"),
        (args.reduced_motion, "--reduced-motion"),
        (args.plain, "--plain"),
    ] {
        if enabled {
            argv.push(flag.into());
        }
    }
    argv
}

/// Replace this process with `aether <args>`, against Archon's own Aether
/// in dedicated mode, or plainly in shared mode. Returns only on failure.
pub fn passthrough(args: &[OsString]) -> anyhow::Result<ExitCode> {
    let config = Config::load_default()?;
    exec_aether(aether_command(&config, args)?)
}

fn aether_command(config: &Config, args: &[OsString]) -> anyhow::Result<Command> {
    let aether = Path::new(AETHER_PROGRAM);
    let command = match config.aether.mode {
        AetherMode::Shared => {
            let mut command = Command::new(aether);
            command.args(args);
            command
        }
        AetherMode::Dedicated => {
            let instance = Instance::resolve()?;
            // Aether would write its own templates into an unseeded directory.
            anyhow::ensure!(
                instance.config_path().is_file(),
                "Archon's own Aether is not set up ({} is missing); run `archon setup`",
                instance.config_path().display()
            );
            instance.aether_command(aether, args)
        }
    };
    Ok(command)
}

fn exec_aether(mut command: Command) -> anyhow::Result<ExitCode> {
    Err(aether_not_run(command.exec()))
}

fn aether_not_run(error: std::io::Error) -> anyhow::Error {
    anyhow::Error::new(error).context(format!("running `{AETHER_PROGRAM}`; is Aether installed?"))
}

/// The aetherd half of `daemon status`: JSON, and one line.
pub fn aether_status(config: &Config) -> anyhow::Result<(serde_json::Value, String)> {
    if config.aether.mode == AetherMode::Shared {
        let text = "Aether: shared mode, the operator's aetherd.".to_owned();
        return Ok((serde_json::json!({"mode": "shared"}), text));
    }
    let instance = Instance::resolve()?;
    let running = lifecycle::is_listening(&instance.socket);
    let state = if running {
        "is running"
    } else {
        "is not running"
    };
    let text = format!(
        "Archon's own aetherd {state} (socket {}).",
        instance.socket.display()
    );
    let value = serde_json::json!({
        "mode": "dedicated",
        "running": running,
        "socket": instance.socket,
        "config_dir": instance.config_dir,
    });
    Ok((value, text))
}

/// Stop Archon's own aetherd with `aether stop`.
pub async fn stop_aether() -> anyhow::Result<(serde_json::Value, String)> {
    let instance = Instance::resolve()?;
    let socket = instance.socket.clone();
    let stopped =
        tokio::task::spawn_blocking(move || lifecycle::stop(&instance, Path::new(AETHER_PROGRAM)))
            .await
            .context("stopping Archon's own aetherd")??;
    let text = match stopped {
        Stop::Stopped => format!(
            "Stopped Archon's own aetherd (socket {}).",
            socket.display()
        ),
        Stop::NotRunning => "Archon's own aetherd is not running.".to_owned(),
    };
    let value = serde_json::json!({"stopped": stopped == Stop::Stopped, "socket": socket});
    Ok((value, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELP: &str = "Attach the terminal UI (starts the daemon if needed)\n\n\
        Usage: aether tui [OPTIONS]\n\nOptions:\n      \
        --agent <AGENT>          Focus this agent on start\n      \
        --plain                  Lines instead of a screen. See also --ascii\n      \
        --view-mode <VIEW_MODE>  Named transcript presentation\n  \
        -h, --help                   Print help\n";

    fn missing(help: &str, args: &[&str]) -> Vec<String> {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let missing = missing_options(help, args.into_iter());
        missing.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn only_the_options_an_aether_lists_are_taken_as_supported() {
        let passed = ["tui", "--agent", "agt_1", "--view-mode", "calm", "--help"];
        assert!(missing(HELP, &passed).is_empty());
        let older = HELP.replace("--view-mode <VIEW_MODE>", "--mouse");
        assert_eq!(missing(&older, &passed), ["--view-mode"]);
        assert_eq!(missing("", &passed), ["--agent", "--view-mode", "--help"]);
    }

    #[test]
    fn an_option_named_only_in_another_description_is_not_listed() {
        assert_eq!(missing(HELP, &["tui", "--plain", "--ascii"]), ["--ascii"]);
    }

    fn tui() -> Command {
        let mut command = Command::new(AETHER_PROGRAM);
        command.args(["tui", "--agent", "agt_1"]);
        command
    }

    #[test]
    fn the_tui_gets_the_operator_commands_when_its_aether_takes_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data/tui-host-commands.json");
        let help = format!("{HELP}      --host-commands <PATH>  A host's slash commands\n");
        let mut command = tui();
        offer_host_commands(&mut command, &help, &path);
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(&args[3..], [OsStr::new(HOST_COMMANDS), path.as_os_str()]);
        assert!(ensure_tui_options(&command, &help).is_ok());
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, operator_commands::host_commands_json());
        let names: Vec<&str> = written["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        let listed: Vec<&str> = operator_commands::OPERATOR_COMMANDS
            .iter()
            .filter(|c| c.in_tui)
            .map(|c| c.name)
            .collect();
        assert_eq!(names, listed);
        assert!(names.contains(&"answer") && !names.contains(&"pause"));
    }

    #[test]
    fn an_older_aether_gets_no_operator_commands_and_still_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui-host-commands.json");
        let mut command = tui();
        offer_host_commands(&mut command, HELP, &path);
        assert_eq!(command.get_args().count(), 3, "no --host-commands");
        assert!(!path.exists());
        assert!(ensure_tui_options(&command, HELP).is_ok());
    }
}
