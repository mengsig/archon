//! `archon`: the operator's CLI, the terminal UI launcher, the quiet chat,
//! and the entry points for `archon daemon run` and `archon mcp`
//! (ARCHITECTURE §2, §12).

mod chat;
mod cli;
mod cmd;
mod connect;
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
    // The daemon logs its work; everything else stays quiet unless RUST_LOG
    // asks. `archon mcp` speaks JSON-RPC on stdout, so logs go to stderr.
    let default_level = match &cli.command {
        Some(Command::Daemon(DaemonCommand::Run { detach: false })) => LevelFilter::INFO,
        _ => LevelFilter::WARN,
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default_level.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            let result = runtime.block_on(cmd::dispatch(cli));
            stop_runtime(runtime);
            result
        });
    match result {
        Ok(code) => code,
        Err(error) => {
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
