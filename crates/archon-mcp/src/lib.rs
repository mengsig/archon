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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
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
    let served = match pipe_stdin()? {
        Some(input) => serve(input, tokio::io::stdout(), socket, archon_bin).await,
        None => serve(tokio::io::stdin(), tokio::io::stdout(), socket, archon_bin).await,
    };
    restore_blocking_stdin()?;
    if served? == Served::InputEnded {
        return Ok(());
    }
    Err(exec_current(archon_bin))
}

/// Stdin as a pipe read that can be dropped without a thread still reading
/// behind it, so a restart never loses a request. `None` when stdin is not a
/// pipe (a terminal or file), where tokio's own stdin serves.
fn pipe_stdin() -> Result<Option<tokio::net::unix::pipe::Receiver>, ArchonError> {
    use std::os::fd::AsFd as _;
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| ArchonError::io("duplicating stdin for the MCP bridge", e))?;
    Ok(tokio::net::unix::pipe::Receiver::from_file(std::fs::File::from(fd)).ok())
}

/// The pipe reader switched the shared stdin to non-blocking; the process
/// started in this one's place expects a blocking stdin.
fn restore_blocking_stdin() -> Result<(), ArchonError> {
    use std::os::fd::AsFd as _;
    let stdin = std::io::stdin();
    let flags = rustix::fs::fcntl_getfl(stdin.as_fd())
        .map_err(|e| ArchonError::io("reading stdin flags", e.into()))?;
    rustix::fs::fcntl_setfl(stdin.as_fd(), flags - rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| ArchonError::io("restoring blocking stdin", e.into()))
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
    let mut framer = Framer::default();
    let mut served = Served::InputEnded;
    let mut restart_deferred = false;
    loop {
        let read = tokio::select! {
            read = framer.next(&mut reader) => read?,
            () = server.restart_requested(), if !restart_deferred => {
                // Bytes already consumed from stdin would be lost to the new
                // process, so restart only between messages.
                if framer.is_idle() && reader.buffer().is_empty() {
                    served = Served::Restart;
                    break;
                }
                restart_deferred = true;
                continue;
            }
        };
        let dispatch = match read {
            Line::End => break,
            Line::Blank => Dispatch::Now(None),
            Line::Oversized => Dispatch::Now(Some(Response::error(
                serde_json::Value::Null,
                PARSE_ERROR,
                format!("message exceeds the {MAX_MESSAGE_BYTES}-byte limit"),
            ))),
            Line::Message => server::intake(framer.message()),
        };
        framer.done();
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
        if restart_deferred && reader.buffer().is_empty() {
            served = Served::Restart;
            break;
        }
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

/// Splits stdin into lines. Waiting for input never loses a byte, so a read
/// may be dropped and started again, and `is_idle` says whether nothing of a
/// message has been taken yet.
#[derive(Debug, Default)]
struct Framer {
    line: Vec<u8>,
    skipping: bool,
}

impl Framer {
    fn is_idle(&self) -> bool {
        self.line.is_empty() && !self.skipping
    }

    /// The bytes of the last [`Line::Message`]; call [`Framer::done`] after.
    fn message(&self) -> &[u8] {
        &self.line
    }

    fn done(&mut self) {
        self.line.clear();
        self.skipping = false;
    }

    async fn next<R>(&mut self, reader: &mut R) -> Result<Line, ArchonError>
    where
        R: AsyncBufRead + Unpin,
    {
        assert!(
            self.line.len() <= MAX_MESSAGE_BYTES,
            "a kept line is bounded"
        );
        loop {
            let chunk = reader
                .fill_buf()
                .await
                .map_err(|e| ArchonError::io("reading MCP messages from stdin", e))?;
            if chunk.is_empty() {
                return Ok(self.at_end());
            }
            let newline = chunk.iter().position(|b| *b == b'\n');
            let content = newline.unwrap_or(chunk.len());
            if !self.skipping && self.line.len() + content > MAX_MESSAGE_BYTES {
                self.line.clear();
                self.skipping = true;
            }
            if !self.skipping {
                self.line.extend_from_slice(&chunk[..content]);
            }
            let taken = newline.map_or(chunk.len(), |n| n + 1);
            reader.consume(taken);
            if newline.is_some() {
                return Ok(self.finished());
            }
        }
    }

    /// Input ended: a last line without a newline is still a message.
    fn at_end(&mut self) -> Line {
        match (self.skipping, self.line.is_empty()) {
            (true, _) => Line::Oversized,
            (false, true) => Line::End,
            (false, false) => self.finished(),
        }
    }

    fn finished(&self) -> Line {
        match (self.skipping, self.line.iter().all(u8::is_ascii_whitespace)) {
            (true, _) => Line::Oversized,
            (false, true) => Line::Blank,
            (false, false) => Line::Message,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dropped_read_loses_nothing_and_the_last_line_needs_no_newline() {
        let (mut writer, server_in) = tokio::io::duplex(64);
        let mut reader = tokio::io::BufReader::new(server_in);
        let mut framer = Framer::default();
        writer.write_all(b"{\"a\":").await.unwrap();
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            framer.next(&mut reader),
        )
        .await;
        assert!(waiting.is_err(), "no whole line yet");
        assert!(!framer.is_idle(), "part of a message was taken");
        writer.write_all(b"1}").await.unwrap();
        drop(writer);
        assert!(matches!(framer.next(&mut reader).await, Ok(Line::Message)));
        assert_eq!(framer.message(), b"{\"a\":1}");
        framer.done();
        assert!(matches!(framer.next(&mut reader).await, Ok(Line::End)));
    }

    #[tokio::test]
    async fn the_size_cap_counts_bytes_across_dropped_reads() {
        let mut input = vec![b' '; MAX_MESSAGE_BYTES];
        input.extend_from_slice(b" {}\nnext\n");
        let mut reader = tokio::io::BufReader::with_capacity(1024, &input[..]);
        let mut framer = Framer::default();
        assert!(matches!(
            framer.next(&mut reader).await,
            Ok(Line::Oversized)
        ));
        framer.done();
        assert!(matches!(framer.next(&mut reader).await, Ok(Line::Message)));
        assert_eq!(framer.message(), b"next");
    }
}
