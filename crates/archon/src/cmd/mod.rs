//! One module per command family; [`dispatch`] routes the parsed CLI.

mod autonomy;
mod bare;
mod control;
mod cost;
mod daemon;
mod doctor;
mod first_run;
mod gc;
mod github_watcher;
mod initiatives;
mod instance;
pub(crate) mod learning;
pub(crate) mod lessons;
mod log;
mod merge;
mod pipelines;
mod queue;
mod release;
mod repo;
mod reports;
mod settings;
mod talk;
mod update;

use std::process::ExitCode;

use crate::chat;
use crate::cli::{Cli, Command, DaemonCommand, OrchestratorCommand};

pub async fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    let json = cli.json;
    if json && let Some(why) = json_refusal(cli.command.as_ref()) {
        return Err(archon_core::error::ArchonError::Invalid(format!(
            "--json is not supported here: {why}"
        ))
        .into());
    }
    if launches_ui(cli.command.as_ref()) {
        update::before_launch().await?;
    }
    let Some(command) = cli.command else {
        return bare::run().await;
    };
    match command {
        Command::Chat { progress } => chat::run(progress).await,
        Command::Say { text, images } => talk::say(&text, &images, json).await,
        Command::Answer { question, text } => talk::answer(&question, &text, json).await,
        Command::Questions => talk::questions(json).await,
        Command::ClearNotifications { item, all } => {
            talk::clear_notifications(item.as_deref(), all, json).await
        }
        Command::Merge {
            item,
            ignore_checks,
        } => merge::run(&item, ignore_checks, json).await,
        Command::Diff { item, full } => merge::diff(&item, full, json).await,
        Command::Promote { repo } => merge::promote(&repo, json).await,
        Command::Log {
            limit,
            item,
            kind,
            follow,
        } => log::log(limit, item.as_deref(), kind, follow, json).await,
        Command::Status {
            no_start: _,
            check_updates,
        } => control::status(check_updates, json).await,
        Command::Cost { since, until, by } => {
            cost::run(since.as_deref(), until.as_deref(), by, json).await
        }
        Command::ModelEvidence {
            repo,
            pipeline,
            stage,
            since,
        } => cost::evidence(repo, pipeline, stage, since.as_deref(), json).await,
        Command::Autonomy(args) => autonomy::run(args, json).await,
        Command::Settings { repo, set, reset } => settings::run(repo, set, reset, json).await,
        Command::GithubWatcher { command } => github_watcher::run(command, json).await,
        Command::Pause(args) => control::pause(args, true, json).await,
        Command::Resume(args) => control::pause(args, false, json).await,
        Command::Orchestrator { command } => {
            control::orchestrator(command.unwrap_or(OrchestratorCommand::Status), json).await
        }
        Command::Repo(command) => repo::run(command, json).await,
        Command::Initiatives { command } => initiatives::run(command, json).await,
        Command::Queue(command) => queue::run(command, json).await,
        Command::Lessons(args) => lessons::run(args, json).await,
        Command::Learning(command) => learning::run(command, json).await,
        Command::Pipelines(command) => pipelines::run(command, json).await,
        Command::Reports { command } => reports::run(command, json).await,
        Command::Setup { print, resync } => instance::setup(print, resync, json).await,
        Command::Release(command) => release::run(command, json).await,
        Command::Doctor => doctor::run(json).await,
        Command::Tui(args) => instance::tui(args).await,
        Command::Aether { args } => instance::passthrough(&args),
        Command::Gc {
            older_than,
            force,
            offline,
            preview,
        } => gc::run(older_than.as_deref(), force, offline, preview, json).await,
        Command::Daemon(command) => daemon::run(command, json).await,
        Command::Mcp => daemon::mcp().await,
        Command::Update(args) => update::run(args, json).await,
    }
}

/// Whether `command` (`None`: a bare `archon`) opens the TUI or the chat,
/// where a launch checks for a newer release.
fn launches_ui(command: Option<&Command>) -> bool {
    matches!(command, None | Some(Command::Chat { .. } | Command::Tui(_)))
}

/// Why `command` (`None`: a bare `archon`) cannot print JSON, or `None` when
/// it honours `--json`. Deliberately exhaustive, so a new command must decide.
fn json_refusal(command: Option<&Command>) -> Option<&'static str> {
    let Some(command) = command else {
        return Some(
            "a bare `archon` opens the terminal UI, or the line chat, and both are interactive",
        );
    };
    match command {
        Command::Chat { .. } => Some("the chat is interactive"),
        Command::Tui(_) => Some("the TUI is interactive"),
        Command::Aether { .. } => Some(
            "`archon aether` prints what aether prints; for aether's own JSON put --json \
             after its command, e.g. `archon aether agents --json`",
        ),
        Command::Mcp => Some("`archon mcp` speaks JSON-RPC on stdout for Aether"),
        Command::Daemon(DaemonCommand::Run { detach: false }) => Some(
            "a foreground archond writes logs; `archon daemon run --detach --json` reports the start",
        ),
        Command::Daemon(
            DaemonCommand::Run { detach: true }
            | DaemonCommand::Start
            | DaemonCommand::Stop { .. }
            | DaemonCommand::Status
            | DaemonCommand::Restart { .. }
            | DaemonCommand::Upgrade { .. }
            | DaemonCommand::Relaunch { .. },
        )
        | Command::Say { .. }
        | Command::Answer { .. }
        | Command::Questions
        | Command::ClearNotifications { .. }
        | Command::Merge { .. }
        | Command::Diff { .. }
        | Command::Promote { .. }
        | Command::Status { .. }
        | Command::Cost { .. }
        | Command::ModelEvidence { .. }
        | Command::Log { .. }
        | Command::Repo(_)
        | Command::Initiatives { .. }
        | Command::Queue(_)
        | Command::Lessons(_)
        | Command::Learning(_)
        | Command::Pipelines(_)
        | Command::Reports { .. }
        | Command::Settings { .. }
        | Command::Autonomy(_)
        | Command::GithubWatcher { .. }
        | Command::Pause(_)
        | Command::Resume(_)
        | Command::Setup { .. }
        | Command::Doctor
        | Command::Release(_)
        | Command::Gc { .. }
        | Command::Orchestrator { .. }
        | Command::Update(_) => None,
    }
}

/// Print `text`, or `value` as JSON under `--json`.
fn emit(
    json: bool,
    value: impl FnOnce() -> anyhow::Result<serde_json::Value>,
    text: &str,
) -> anyhow::Result<()> {
    if json {
        return crate::connect::print_json(&value()?);
    }
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// The text of `path`, the `what` it holds; `-` reads stdin.
fn read_text(path: &std::path::Path, what: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use std::io::Read as _;
    if path == std::path::Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .with_context(|| format!("reading {what} from stdin"))?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {what} {}", path.display()))
}

/// `path` decoded by its extension, `.json` or `.toml`.
fn read_map<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> anyhow::Result<T> {
    use anyhow::Context as _;
    use archon_core::error::ArchonError;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match path.extension().and_then(|s| s.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|e| ArchonError::Invalid(format!("{}: {e}", path.display())).into()),
        Some("toml") => toml::from_str(&text)
            .map_err(|e| ArchonError::Invalid(format!("{}: {e}", path.display())).into()),
        _ => Err(ArchonError::Invalid(format!(
            "{} must have a .json or .toml extension",
            path.display()
        ))
        .into()),
    }
}

fn to_json(value: &impl serde::Serialize) -> anyhow::Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| anyhow::anyhow!("encoding JSON output: {e}"))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    fn refusal(args: &[&str]) -> Option<&'static str> {
        let cli = Cli::try_parse_from([&["archon", "--json"], args].concat()).unwrap();
        json_refusal(cli.command.as_ref())
    }

    #[test]
    fn only_the_commands_that_open_the_ui_check_for_a_release() {
        for (args, opens) in [
            (&[][..], true),
            (&["chat"], true),
            (&["tui"], true),
            (&["status"], false),
            (&["update"], false),
            (&["say", "hi"], false),
            (&["mcp"], false),
        ] {
            let cli = Cli::try_parse_from([&["archon"], args].concat()).unwrap();
            assert_eq!(launches_ui(cli.command.as_ref()), opens, "{args:?}");
        }
    }

    #[test]
    fn only_interactive_and_pass_through_commands_refuse_json() {
        for args in [
            &[][..],
            &["chat"],
            &["tui"],
            &["aether", "agents"],
            &["mcp"],
            &["daemon", "run"],
        ] {
            assert!(refusal(args).is_some(), "{args:?} accepts --json");
        }
        for args in [
            &["daemon", "run", "--detach"][..],
            &["daemon", "status"],
            &["setup", "--print"],
            &["say", "hi"],
            &["queue", "list"],
            &["reports"],
            &["reports", "reopen", "rpt_1"],
            &["doctor"],
            &["github-watcher"],
            &["github-watcher", "add", "o/r"],
            &["update", "--check"],
        ] {
            assert_eq!(refusal(args), None, "{args:?}");
        }
    }
}
