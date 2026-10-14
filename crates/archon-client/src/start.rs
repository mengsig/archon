//! Autostart: spawn `archon daemon run --detach` when nothing listens, then
//! poll the socket until a daemon answers or the deadline passes. Two
//! clients may start one at once; whichever daemon listens serves both.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::protocol::ClientKind;
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::ArchonClient;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How much of the startup log an error carries.
const LOG_TAIL_LINES: usize = 20;
const LOG_TAIL_CHARS: usize = 4_000;
/// After the launcher failed, how long another client's daemon (the winner
/// of a start race) may take to listen.
const RACE_GRACE: Duration = Duration::from_secs(1);

pub(crate) async fn connect_or_start_within(
    socket: &Path,
    kind: ClientKind,
    archon_bin: &Path,
    timeout: Duration,
) -> Result<ArchonClient, ArchonError> {
    assert!(!timeout.is_zero(), "a zero start timeout never polls");
    match ArchonClient::connect(socket, kind).await {
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%why, bin = %archon_bin.display(), "starting archond");
        }
        other => return other,
    }
    let mut child = spawn_daemon(socket, archon_bin)?;
    let mut deadline = Instant::now() + timeout;
    let mut exited: Option<ExitStatus> = None;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match ArchonClient::connect(socket, kind).await {
            Err(ArchonError::Unavailable(_)) => {}
            other => return other,
        }
        if exited.is_none() {
            exited = reap(&mut child)?;
            // A failed launcher may have lost a start race to another
            // client's daemon, which listens within moments if so.
            if exited.is_some_and(|status| !status.success()) {
                deadline = deadline.min(Instant::now() + RACE_GRACE);
            }
        }
        if Instant::now() >= deadline {
            let why = never_listened(&mut child, exited, timeout).await;
            return Err(start_failed(socket, archon_bin, &why));
        }
    }
}

async fn never_listened(
    child: &mut Child,
    exited: Option<ExitStatus>,
    timeout: Duration,
) -> String {
    let waited = timeout.as_secs_f64();
    let Some(status) = exited.filter(|status| !status.success()) else {
        return format!("it did not listen within {waited}s");
    };
    let said = launcher_stderr(child).await;
    if said.is_empty() {
        format!("it exited with {status} and nothing listened")
    } else {
        format!("it exited with {status} and nothing listened: {said}")
    }
}

/// What the exited launcher wrote to stderr, trimmed and bounded.
async fn launcher_stderr(child: &mut Child) -> String {
    let Some(mut pipe) = child.stderr.take() else {
        return String::new();
    };
    let mut bytes = Vec::new();
    // The launcher has exited, so the pipe is at its end; a read error only
    // loses the launcher's own words, and the startup log still follows.
    if let Err(error) = pipe.read_to_end(&mut bytes).await {
        return format!("(its stderr was unreadable: {error})");
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    let total = text.chars().count();
    text.chars()
        .skip(total.saturating_sub(LOG_TAIL_CHARS))
        .collect()
}

/// The detaching launcher. Its stdout is closed, as `archon mcp` owns stdout
/// for JSON-RPC; its stderr is kept to explain a failed start.
fn spawn_daemon(socket: &Path, archon_bin: &Path) -> Result<Child, ArchonError> {
    Command::new(archon_bin)
        .args(["daemon", "run", "--detach"])
        .env("ARCHON_SOCKET", socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ArchonError::Unavailable(format!(
                "archond is not running at {} and `{} daemon run --detach` could not be \
                 started: {e}",
                socket.display(),
                archon_bin.display()
            ))
        })
}

fn reap(child: &mut Child) -> Result<Option<ExitStatus>, ArchonError> {
    child
        .try_wait()
        .map_err(|e| ArchonError::io("waiting for `archon daemon run --detach`", e))
}

fn start_failed(socket: &Path, archon_bin: &Path, why: &str) -> ArchonError {
    let log = archon_core::paths::startup_log_for(socket);
    ArchonError::Unavailable(format!(
        "archond did not start: `{} daemon run --detach` was launched but {why}. Startup log \
         {}:\n{}",
        archon_bin.display(),
        log.display(),
        log_tail(&log)
    ))
}

/// The end of this start's part of the startup log, or a note saying why
/// there is none.
fn log_tail(log: &Path) -> String {
    let text = match std::fs::read(log) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => return format!("(no startup log: {e})"),
    };
    let (marked, text) = since_last_start(&text);
    if text.trim().is_empty() {
        let note = if marked {
            "(nothing since this start)"
        } else {
            "(the startup log is empty)"
        };
        return note.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n");
    let total = tail.chars().count();
    if total <= LOG_TAIL_CHARS {
        return tail;
    }
    tail.chars().skip(total - LOG_TAIL_CHARS).collect()
}

/// What follows the last start mark, and whether there was one; a log
/// written before marks existed is all one start.
fn since_last_start(text: &str) -> (bool, &str) {
    let Some(at) = text.rfind(archon_core::paths::STARTUP_LOG_MARK) else {
        return (false, text);
    };
    let after = text[at..].split_once('\n').map_or("", |(_, rest)| rest);
    assert!(
        !after.contains(archon_core::paths::STARTUP_LOG_MARK),
        "the part kept starts after the last mark"
    );
    (true, after)
}
