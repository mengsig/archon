//! `archon mcp`: a stateless stdio MCP server that Aether spawns as the
//! `archon` server. Each tool call becomes one Archon daemon request
//! (ARCHITECTURE §4.7, §11). Only `initialize`, `notifications/initialized`,
//! `ping`, `tools/list` and `tools/call` are implemented.

pub mod jsonrpc;
mod restart;
mod server;
pub mod tools;

use std::path::Path;
use std::sync::Arc;

use archon_core::error::ArchonError;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio::sync::Mutex;
use tokio::task::{JoinError, JoinSet};

use crate::jsonrpc::{PARSE_ERROR, Response};
use crate::server::{DaemonLink, Dispatch, Server};

pub use crate::restart::exec_current;
pub use crate::server::CALL_BUDGET;

/// Why [`serve`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    /// The input ended.
    InputEnded,
    /// Archond is newer than this bridge and its binary was replaced:
    /// start `archon mcp` again in this process's place.
    Restart,
}

/// Longest accepted message, matching Aether's own MCP cap. Responses are
/// held to it too, newline included.
pub const MAX_MESSAGE_BYTES: usize = 4 << 20;

/// Tool calls running at once. One more is answered at once with a busy
/// tool error rather than queued, where its deadline would run out unseen.
pub const MAX_CONCURRENT_CALLS: usize = 64;

/// Serve stdin/stdout until stdin closes. The daemon connection is opened on first use, starting
/// archond only for a call that changes something, and re-opened after a drop. When Archon
/// was upgraded under it, the bridge starts the new binary in its place and does not return.
pub async fn serve_stdio(socket: &Path, archon_bin: &Path) -> Result<(), ArchonError> {
    let served = serve(tokio::io::stdin(), tokio::io::stdout(), socket, archon_bin).await?;
    if served == Served::InputEnded {
        return Ok(());
    }
    Err(exec_current(archon_bin))
}

/// [`serve_stdio`] over any byte streams: one JSON-RPC message per line in,
/// one response per request line out. Tool calls are answered as they
/// finish, so responses may leave in a different order than their requests;
/// each carries its request's id. When the input ends, the calls in flight
/// are answered (each within [`CALL_BUDGET`]) before this returns.
pub async fn serve<R, W>(
    input: R,
    output: W,
    socket: &Path,
    archon_bin: &Path,
) -> Result<Served, ArchonError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tracing::info!(socket = %socket.display(), "archon mcp serving on stdio");
    let mut reader = tokio::io::BufReader::new(input);
    let server = Arc::new(Server::new(DaemonLink::new(socket, archon_bin)));
    let output = Arc::new(Mutex::new(output));
    let mut calls = JoinSet::new();
    let mut line = Vec::new();
    let mut served = Served::InputEnded;
    loop {
        let read = tokio::select! {
            read = read_line(&mut reader, &mut line) => read?,
            () = server.restart_requested() => {
                served = Served::Restart;
                break;
            }
        };
        let dispatch = match read {
            Line::End => break,
            Line::Blank => continue,
            Line::Oversized => Dispatch::Now(Some(Response::error(
                serde_json::Value::Null,
                PARSE_ERROR,
                format!("message exceeds the {MAX_MESSAGE_BYTES}-byte limit"),
            ))),
            Line::Message => server::intake(&line),
        };
        while let Some(finished) = calls.try_join_next() {
            call_ended(finished)?;
        }
        match dispatch {
            Dispatch::Now(None) => {}
            Dispatch::Now(Some(response)) => write_response(&output, &response).await?,
            Dispatch::Call(call) if calls.len() >= MAX_CONCURRENT_CALLS => {
                write_response(&output, &server::busy(call)).await?;
            }
            Dispatch::Call(call) => {
                let (server, output) = (Arc::clone(&server), Arc::clone(&output));
                calls.spawn(async move {
                    let response = server.call(call).await;
                    write_response(&output, &response).await
                });
            }
        }
        assert!(
            calls.len() <= MAX_CONCURRENT_CALLS,
            "calls in flight are bounded"
        );
    }
    while let Some(finished) = calls.join_next().await {
        call_ended(finished)?;
    }
    assert!(
        served == Served::InputEnded || server.restart_wanted(),
        "a restart is served only when one was asked for"
    );
    Ok(served)
}

/// A finished call task: its write error, if any. A panic in a handler is
/// a bug and is re-raised here, as it would have been on the read loop.
fn call_ended(finished: Result<Result<(), ArchonError>, JoinError>) -> Result<(), ArchonError> {
    match finished {
        Ok(written) => written,
        Err(error) => {
            assert!(error.is_panic(), "call tasks are never aborted: {error}");
            std::panic::resume_unwind(error.into_panic())
        }
    }
}

enum Line {
    Message,
    Blank,
    /// Too long; skipped up to its newline so the next message still frames.
    Oversized,
    End,
}

async fn read_line<R>(reader: &mut R, line: &mut Vec<u8>) -> Result<Line, ArchonError>
where
    R: AsyncBufRead + Unpin,
{
    let failed = |e| ArchonError::io("reading MCP messages from stdin", e);
    line.clear();
    let limit = MAX_MESSAGE_BYTES as u64 + 1;
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', line)
        .await
        .map_err(failed)?;
    if read == 0 {
        return Ok(Line::End);
    }
    let complete = line.last() == Some(&b'\n');
    if !complete && line.len() > MAX_MESSAGE_BYTES {
        let mut rest = Vec::new();
        loop {
            rest.clear();
            let n = (&mut *reader)
                .take(64 * 1024)
                .read_until(b'\n', &mut rest)
                .await
                .map_err(failed)?;
            if n == 0 || rest.last() == Some(&b'\n') {
                break;
            }
        }
        line.clear();
        return Ok(Line::Oversized);
    }
    match line.iter().all(u8::is_ascii_whitespace) {
        true => Ok(Line::Blank),
        false => Ok(Line::Message),
    }
}

async fn write_response<W>(output: &Mutex<W>, response: &Response) -> Result<(), ArchonError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = response.to_line()?;
    assert!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "to_line holds the message cap"
    );
    let failed = |e| ArchonError::io("writing an MCP response to stdout", e);
    let mut output = output.lock().await;
    output.write_all(&bytes).await.map_err(failed)?;
    output.flush().await.map_err(failed)
}
