//! `archon`: the operator's CLI, the terminal UI launcher, the quiet chat,
//! and the entry points for `archon daemon run` and `archon mcp`
//! (ARCHITECTURE §2, §12).

mod chat;
mod cli;
mod cmd;
mod connect;
mod daemon_log;
mod format;
mod instance;
mod ops;
mod questions;
mod render;
mod setup;
mod style;
mod views;
mod wrap;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use crate::cli::{Cli, Command, DaemonCommand};

/// Most the runtime waits at exit for blocking work still running, such as
/// a daemon git that may take minutes; the process exits without it.
const RUNTIME_STOP: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return usage_error(&error),
    };
    let json = cli.json;
    let logger = match detached_logger(&cli) {
        Ok(logger) => logger,
        Err(error) => {
            eprintln!("archon: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(logger) = &logger {
        logger.install_panic_hook();
    }
    // The daemon logs its work; everything else stays quiet unless RUST_LOG
    // asks. `archon mcp` speaks JSON-RPC on stdout, so logs go to stderr.
    let default_level = match &cli.command {
        Some(Command::Daemon(DaemonCommand::Run { detach: false })) => LevelFilter::INFO,
        _ => LevelFilter::WARN,
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default_level.into())
        .from_env_lossy();
    let writer = match &logger {
        Some(logger) => BoxMakeWriter::new(logger.clone()),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    let subscriber = if logger.is_some() {
        subscriber.with_ansi(false)
    } else {
        subscriber
    };
    subscriber.init();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            let result = runtime.block_on(async {
                tokio::select! {
                    result = cmd::dispatch(cli) => result,
                    result = monitor_log(logger.as_ref()) => result,
                }
            });
            stop_runtime(runtime);
            result
        });
    match result {
        Ok(code) => code,
        Err(error) => {
            if let Some(logger) = &logger {
                if let Err(log_error) =
                    logger.write_message(format!("archon: {error:#}\n").as_bytes())
                {
                    use std::io::Write as _;
                    // The detach caller closes this startup pipe on success.
                    let _closed_startup_pipe = writeln!(std::io::stderr(), "archon: {log_error}");
                }
                return ExitCode::FAILURE;
            }
            if json {
                let code = error
                    .downcast_ref::<archon_core::error::ArchonError>()
                    .map(|e| archon_core::protocol::ErrorReply::from(e).code)
                    .unwrap_or(archon_core::protocol::ErrorCode::Internal);
                let envelope =
                    serde_json::json!({"error": {"code": code, "message": format!("{error:#}")}});
                println!("{envelope}");
            } else {
                eprintln!("archon: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn detached_logger(cli: &Cli) -> anyhow::Result<Option<daemon_log::Logger>> {
    if !matches!(
        cli.command,
        Some(Command::Daemon(DaemonCommand::Run { detach: false }))
    ) || std::env::var_os(daemon_log::DETACHED_ENV).as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return Ok(None);
    }
    let config = archon_core::config::Config::load_default()?;
    let path = archon_core::paths::startup_log_for(&archon_client::default_socket());
    daemon_log::Logger::open(&path, &config.storage).map(Some)
}

async fn monitor_log(logger: Option<&daemon_log::Logger>) -> anyhow::Result<ExitCode> {
    match logger {
        Some(logger) => match logger.monitor().await {
            Ok(never) => match never {},
            Err(error) => Err(error),
        },
        None => std::future::pending().await,
    }
}

/// Dropping a runtime would wait for its blocking tasks without limit.
fn stop_runtime(runtime: tokio::runtime::Runtime) {
    let started = Instant::now();
    runtime.shutdown_timeout(RUNTIME_STOP);
    if started.elapsed() >= RUNTIME_STOP {
        tracing::warn!(limit = ?RUNTIME_STOP, "blocking work still running at exit; exiting without it");
    }
}

/// A command line that does not parse: what clap prints, or under `--json`
/// the usual error envelope on stdout. Either way with clap's exit status.
fn usage_error(error: &clap::Error) -> ExitCode {
    let status = u8::try_from(error.exit_code()).unwrap_or(2);
    let wants_json = std::env::args_os().skip(1).any(|arg| arg == "--json");
    // Help and the version are no error: clap prints them to stdout.
    if !error.use_stderr() || !wants_json {
        return match error.print() {
            Ok(()) => ExitCode::from(status),
            Err(_) => ExitCode::FAILURE,
        };
    }
    assert_ne!(status, 0, "a command line that does not parse fails");
    let rendered = error.render().to_string();
    let message = rendered.trim().trim_start_matches("error: ");
    let code = archon_core::protocol::ErrorCode::InvalidArgument;
    let envelope = serde_json::json!({"error": {"code": code, "message": message}});
    println!("{envelope}");
    ExitCode::from(status)
}
