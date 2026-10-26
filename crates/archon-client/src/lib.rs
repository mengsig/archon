//! Client for the Archon daemon socket (ARCHITECTURE §10), used by the CLI,
//! `archon mcp` and the benchmarks, plus the NDJSON framing the daemon's
//! server shares.
//!
//! Contract: one reader task routes replies to waiting requests by id and
//! feed frames to their subscription's channel; a dropped connection, or an
//! id-0 notice from the daemon, fails every waiter and closes every
//! subscription with the disconnect reason.

mod connection;
pub mod framing;
mod start;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::FeedItem;
use archon_core::protocol::{
    ClientKind, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, RequestClass, Welcome,
};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::connection::{FEED_CAPACITY, FeedSink, Router, Shared, WRITE_QUEUE, run_writer};

/// How long `connect_or_start` waits for a freshly spawned daemon.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// A daemon that accepts but never answers `hello` is as good as absent.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Background tasks of one connection, aborted together when it is dropped.
#[derive(Debug, Default)]
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub struct ArchonClient {
    welcome: Welcome,
    shared: Arc<Shared>,
    principal: Principal,
    _tasks: Tasks,
}

/// Whom a client's requests are made for. archond applies an agent's rules
/// to a request forwarded for it, whichever client sends it (ARCHITECTURE §11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Principal {
    /// The operator: each request goes as it is.
    #[default]
    Operator,
    /// An Aether agent: each request is forwarded for it.
    Agent(AetherAgentId),
    /// An Aether agent that is not named: only requests any client may make are sent.
    UnnamedAgent,
}

impl Principal {
    /// `request` as this principal sends it; refused when no agent is named
    /// for a request that is more than an ordinary read.
    pub fn address(&self, request: Request) -> Result<Request, ArchonError> {
        assert!(
            !matches!(
                request,
                Request::Hello { .. } | Request::SubscribeFeed { .. }
            ),
            "hello and subscribe_feed are never forwarded"
        );
        match self {
            Self::Operator => Ok(request),
            Self::Agent(caller) => {
                assert!(
                    !matches!(request, Request::ForAgent { .. }),
                    "a request is forwarded for one agent only"
                );
                Ok(Request::ForAgent {
                    caller: caller.clone(),
                    request: Box::new(request),
                })
            }
            Self::UnnamedAgent if request.class() == RequestClass::Read => Ok(request),
            Self::UnnamedAgent => Err(ArchonError::Forbidden(
                "this client runs for an Aether agent that is not named, so it sends only \
                 reads; the operator makes this request from their own shell"
                    .into(),
            )),
        }
    }
}

/// A live feed subscription; closes when the connection drops.
#[derive(Debug)]
pub struct FeedSubscription {
    pub id: u64,
    pub last_seq: u64,
    pub items: mpsc::Receiver<FeedItem>,
    closed: Arc<OnceLock<String>>,
}

impl FeedSubscription {
    /// Why the stream ended (overflow, disconnect, ...); a generic reason
    /// while it is still open or when the client was dropped.
    pub fn close_reason(&self) -> String {
        self.closed
            .get()
            .cloned()
            .unwrap_or_else(|| "the archond connection closed".to_owned())
    }
}

impl ArchonClient {
    /// Connect and send `hello`; `Unavailable` when nothing listens.
    pub async fn connect(socket: &Path, kind: ClientKind) -> Result<Self, ArchonError> {
        let stream = open(socket).await?;
        let (read, write) = stream.into_split();
        let (lines_tx, lines_rx) = mpsc::channel(WRITE_QUEUE);
        let shared = Arc::new(Shared::new(lines_tx));
        let tasks = Tasks(vec![
            tokio::spawn(run_writer(write, lines_rx, shared.clone())),
            tokio::spawn(Router::new(shared.clone()).run(read)),
        ]);
        let welcome = handshake(&shared, kind).await?;
        Ok(Self {
            welcome,
            shared,
            principal: Principal::Operator,
            _tasks: tasks,
        })
    }

    /// This client with its later requests made for `principal`.
    pub fn acting_for(mut self, principal: Principal) -> Self {
        self.principal = principal;
        self
    }

    /// Connect, or spawn `<archon_bin> daemon run --detach` and retry until `START_TIMEOUT`,
    /// reporting its startup log on failure; never over a mix of moved and default locations.
    pub async fn connect_or_start(
        socket: &Path,
        kind: ClientKind,
        archon_bin: &Path,
    ) -> Result<Self, ArchonError> {
        let Some(mix) = archon_core::paths::partial_override(socket) else {
            return start::connect_or_start_within(socket, kind, archon_bin, START_TIMEOUT).await;
        };
        match Self::connect(socket, kind).await {
            Err(ArchonError::Unavailable(why)) => Err(ArchonError::Unavailable(format!(
                "{why}, and it is not started for you: {mix}"
            ))),
            connected => connected,
        }
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// Whether the connection is known dead; every later request would fail.
    pub fn is_closed(&self) -> bool {
        self.shared.disconnect_reason().is_some()
    }

    /// One request, its reply, or the daemon's `ErrorReply` as
    /// `ArchonError` with the code preserved in the message.
    pub async fn request(&self, request: Request) -> Result<Reply, ArchonError> {
        self.call(request).await?.map_err(refusal_error)
    }

    /// Like [`Self::request`], waiting up to `timeout` instead of
    /// [`REQUEST_TIMEOUT`], for work such as a gc pass that may take longer.
    pub async fn request_within(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<Reply, ArchonError> {
        assert!(!timeout.is_zero(), "a request needs time to be answered");
        assert!(
            !matches!(
                request,
                Request::Hello { .. } | Request::SubscribeFeed { .. }
            ),
            "hello and subscribe_feed have their own methods"
        );
        let request = self.principal.address(request)?;
        let answer = self.shared.call(request, None, timeout).await?;
        answer.map_err(refusal_error)
    }

    /// Like [`Self::request`], but keeps the daemon's refusal as data: the
    /// outer error is the transport, the inner one is archond (or the
    /// client's [`Principal`]) saying no.
    pub async fn call(&self, request: Request) -> Result<Result<Reply, ErrorReply>, ArchonError> {
        assert!(
            !matches!(
                request,
                Request::Hello { .. } | Request::SubscribeFeed { .. }
            ),
            "hello and subscribe_feed have their own methods"
        );
        let request = match self.principal.address(request) {
            Ok(request) => request,
            Err(refused) => return Ok(Err(ErrorReply::from(&refused))),
        };
        self.shared.call(request, None, REQUEST_TIMEOUT).await
    }

    pub async fn subscribe_feed(
        &self,
        after: Option<u64>,
        verbose: bool,
    ) -> Result<FeedSubscription, ArchonError> {
        let (items_tx, items) = mpsc::channel(FEED_CAPACITY);
        let closed = Arc::new(OnceLock::new());
        let sink = FeedSink::new(items_tx, closed.clone(), after);
        let request = Request::SubscribeFeed { after, verbose };
        let answer = self
            .shared
            .call(request, Some(sink), REQUEST_TIMEOUT)
            .await?;
        match answer.map_err(refusal_error)? {
            Reply::Subscribed { id, last_seq } => Ok(FeedSubscription {
                id,
                last_seq,
                items,
                closed,
            }),
            other => Err(ArchonError::Protocol(format!(
                "archond answered subscribe_feed with `{}`",
                connection::reply_kind(&other)
            ))),
        }
    }
}

async fn open(socket: &Path) -> Result<UnixStream, ArchonError> {
    UnixStream::connect(socket)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ArchonError::Unavailable(format!(
                    "archond is not running: nothing listens at {} ({e})",
                    socket.display()
                ))
            }
            _ => ArchonError::io(format!("connecting to {}", socket.display()), e),
        })
}

async fn handshake(shared: &Shared, kind: ClientKind) -> Result<Welcome, ArchonError> {
    let hello = Request::Hello {
        protocol_version: PROTOCOL_VERSION,
        client: kind,
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    match shared.call(hello, None, HELLO_TIMEOUT).await? {
        Ok(Reply::Welcome(welcome)) if welcome.protocol_version == PROTOCOL_VERSION => Ok(welcome),
        Ok(Reply::Welcome(welcome)) => Err(incompatible(&format!(
            "it answered with protocol {} (daemon {})",
            welcome.protocol_version, welcome.daemon_version
        ))),
        Ok(other) => Err(ArchonError::Protocol(format!(
            "archond answered hello with `{}`, not `welcome`",
            connection::reply_kind(&other)
        ))),
        Err(refusal) if refusal.code == ErrorCode::Incompatible => {
            Err(incompatible(&refusal.message))
        }
        Err(refusal) => Err(refusal_error(refusal)),
    }
}

/// What every [`incompatible`] error says first.
const INCOMPATIBLE_MARK: &str = "this archon speaks daemon protocol";

fn incompatible(detail: &str) -> ArchonError {
    ArchonError::Protocol(format!(
        "{INCOMPATIBLE_MARK} {PROTOCOL_VERSION} but archond does not: {detail}. \
         Restart the daemon from the same build (`archon daemon stop --keep-aether`, then retry)"
    ))
}

/// Whether `error` is archond refusing this client's protocol version.
pub fn is_incompatible(error: &ArchonError) -> bool {
    matches!(error, ArchonError::Protocol(text) if text.starts_with(INCOMPATIBLE_MARK))
}

/// A daemon refusal as an error. archond sends its own error's text, so the
/// matching variant takes that text back without repeating its prefix.
pub fn refusal_error(refusal: ErrorReply) -> ArchonError {
    let message = refusal.message;
    let bare = |prefix: &str| message.strip_prefix(prefix).unwrap_or(&message).to_owned();
    match refusal.code {
        ErrorCode::InvalidArgument => ArchonError::Invalid(bare("invalid argument: ")),
        ErrorCode::NotFound => ArchonError::NotFound(
            message
                .strip_suffix(" not found")
                .unwrap_or(&message)
                .to_owned(),
        ),
        ErrorCode::Forbidden => ArchonError::Forbidden(bare("forbidden: ")),
        ErrorCode::Policy => ArchonError::Policy(bare("model policy: ")),
        ErrorCode::InvalidTransition => {
            ArchonError::InvalidTransition(bare("state transition refused: "))
        }
        ErrorCode::Unavailable => ArchonError::Unavailable(bare("unavailable: ")),
        ErrorCode::Recovering => ArchonError::Recovering(bare("recovering: ")),
        ErrorCode::Incompatible | ErrorCode::Internal => {
            ArchonError::Protocol(format!("archond failed: {message}"))
        }
    }
}

/// The socket path the CLI and MCP proxy use.
pub fn default_socket() -> PathBuf {
    archon_core::paths::socket_path()
}

#[cfg(test)]
mod tests;
