//! The Archon socket server (ARCHITECTURE §10). One task per
//! connection: `hello` first, then requests answered in order of arrival
//! (each handled on its own task so a slow request never blocks a feed
//! subscription), feed items pushed as `{"frame":"feed",...}`.

mod subscription;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use archon_client::framing::{encode_frame, read_frame};
use archon_core::error::ArchonError;
use archon_core::protocol::{
    ClientFrame, ClientKind, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, ServerFrame,
    Welcome,
};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::context::DaemonContext;
use crate::handlers;

/// Frames queued per connection before request tasks wait for the writer.
const OUTBOX_FRAMES: usize = 256;
/// Consecutive `accept` failures (e.g. out of descriptors) before giving up.
const MAX_ACCEPT_FAILURES: u32 = 50;
/// How long a `Shutdown` reply may take to reach its client.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(2);

/// An exclusive `flock` on `<socket>.lock`, held for the daemon's lifetime.
/// Two daemons started at once (two autostarting clients) could otherwise
/// both pass the stale-socket check, and both would then drive the store.
#[derive(Debug)]
pub struct DaemonLock {
    _file: std::fs::File,
}

/// Bind the socket with mode 0600 under the [`DaemonLock`], refusing to
/// steal a live daemon's socket (connect first; remove only a stale file).
/// Writes the pid file. Keep the lock until the socket is removed.
pub fn bind(ctx: &DaemonContext) -> Result<(UnixListener, DaemonLock), ArchonError> {
    let socket = &ctx.paths.socket;
    assert!(socket.is_absolute(), "the socket path is resolved absolute");
    let parent = socket.parent().ok_or_else(|| {
        ArchonError::Invalid(format!("socket {} has no directory", socket.display()))
    })?;
    secure_parent(parent)?;
    let lock = lock_daemon(socket)?;
    clear_stale_socket(socket)?;
    let listener = bind_private(socket)?;
    set_mode(socket, 0o600)?;
    let pid_path = archon_core::paths::pid_path_for(socket);
    std::fs::write(&pid_path, format!("{}\n", std::process::id()))
        .map_err(|e| ArchonError::io(format!("writing {}", pid_path.display()), e))?;
    tracing::info!(socket = %socket.display(), "listening");
    Ok((listener, lock))
}

/// The lock file is never removed: a daemon waiting on a removed file's
/// lock and one on its re-created successor would both think they won.
/// `archon gc --offline` takes it too, so no daemon starts while it runs.
pub fn lock_daemon(socket: &Path) -> Result<DaemonLock, ArchonError> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = socket.with_extension("lock");
    assert_ne!(path, socket, "the lock file is not the socket");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| ArchonError::io(format!("opening {}", path.display()), e))?;
    match file.try_lock() {
        Ok(()) => Ok(DaemonLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(ArchonError::Unavailable(format!(
            "another archond holds {} and serves {}",
            path.display(),
            socket.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(ArchonError::io(format!("locking {}", path.display()), e))
        }
    }
}

/// Create the socket's directory with mode 0700. An existing one of ours is
/// tightened to 0700; another user's is refused when others can write in it
/// without the sticky bit (as in `/tmp`).
fn secure_parent(parent: &Path) -> Result<(), ArchonError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(parent) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
            return set_mode(parent, 0o700);
        }
        Err(e) => {
            return Err(ArchonError::io(
                format!("inspecting {}", parent.display()),
                e,
            ));
        }
    };
    if !meta.is_dir() {
        return Err(ArchonError::Invalid(format!(
            "{} is not a directory; the socket needs one",
            parent.display()
        )));
    }
    let mode = meta.mode() & 0o7777;
    if meta.uid() == rustix::process::getuid().as_raw() {
        return if mode == 0o700 {
            Ok(())
        } else {
            tracing::warn!(dir = %parent.display(), mode = format!("{mode:o}"), "tightening the socket directory to 0700");
            set_mode(parent, 0o700)
        };
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(ArchonError::Invalid(format!(
            "{} is owned by another user and writable by others (mode {mode:o}); \
             put the socket in a directory only you can write",
            parent.display()
        )));
    }
    Ok(())
}

/// Bind under umask 0077, so the socket is never visible with wider access.
fn bind_private(socket: &Path) -> Result<UnixListener, ArchonError> {
    let previous = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let bound = UnixListener::bind(socket);
    rustix::process::umask(previous);
    bound.map_err(|e| ArchonError::io(format!("binding {}", socket.display()), e))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ArchonError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| ArchonError::io(format!("setting the mode of {}", path.display()), e))
}

/// A socket another daemon answers on is refused; a dead one is removed.
fn clear_stale_socket(socket: &Path) -> Result<(), ArchonError> {
    let meta = match std::fs::symlink_metadata(socket) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(ArchonError::io(
                format!("inspecting {}", socket.display()),
                e,
            ));
        }
    };
    if !meta.file_type().is_socket() {
        return Err(ArchonError::Invalid(format!(
            "{} exists and is not a socket; move it away",
            socket.display()
        )));
    }
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        return Err(ArchonError::Unavailable(format!(
            "another archond is already listening on {}",
            socket.display()
        )));
    }
    tracing::info!(socket = %socket.display(), "removing a stale socket");
    std::fs::remove_file(socket)
        .map_err(|e| ArchonError::io(format!("removing the stale {}", socket.display()), e))
}

/// Accept connections until a `Shutdown` request; returns `Ok` then.
pub async fn serve(ctx: Arc<DaemonContext>, listener: UnixListener) -> Result<(), ArchonError> {
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut connections = JoinSet::new();
    let mut failures = 0u32;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    failures = 0;
                    connections.spawn(connection(ctx.clone(), stream, stop_tx.clone()));
                }
                Err(error) => {
                    failures += 1;
                    if failures >= MAX_ACCEPT_FAILURES {
                        return Err(ArchonError::io("accepting connections", error));
                    }
                    tracing::warn!(%error, "accept failed; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = stop_rx.changed() => break,
            Some(done) = connections.join_next() => {
                if let Err(error) = done {
                    tracing::error!(%error, "connection task panicked");
                }
            }
        }
    }
    assert!(*stop_rx.borrow(), "the server only stops on shutdown");
    tracing::info!("shutdown requested");
    Ok(())
}

/// A frame for the connection's writer; `flushed` fires once it is sent.
pub(crate) struct Outgoing {
    frame: ServerFrame,
    flushed: Option<oneshot::Sender<()>>,
}

pub(crate) type Outbox = mpsc::Sender<Outgoing>;

pub(crate) async fn send(outbox: &Outbox, frame: ServerFrame) -> bool {
    let out = Outgoing {
        frame,
        flushed: None,
    };
    outbox.send(out).await.is_ok()
}

fn reply(id: u64, result: Result<Reply, ArchonError>) -> ServerFrame {
    ServerFrame::Reply {
        id,
        result: result.map_err(|e| ErrorReply::from(&e)),
    }
}

async fn connection(ctx: Arc<DaemonContext>, stream: UnixStream, stop: watch::Sender<bool>) {
    let (read, write) = stream.into_split();
    let (outbox, rx) = mpsc::channel(OUTBOX_FRAMES);
    let writer = tokio::spawn(write_loop(write, rx));
    let mut reader = BufReader::new(read);
    if let Err(error) = session(&ctx, &mut reader, outbox, &stop).await {
        tracing::debug!(%error, "connection ended");
    }
    match writer.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "connection writer ended"),
        Err(error) => tracing::error!(%error, "connection writer panicked"),
    }
}

/// Writes queued frames in order. A reply too large for one frame becomes
/// an error reply, so the client is never left waiting.
async fn write_loop(
    mut write: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Outgoing>,
) -> Result<(), ArchonError> {
    while let Some(out) = rx.recv().await {
        let line = match encode_frame(&out.frame) {
            Ok(line) => line,
            Err(error) => {
                let ServerFrame::Reply { id, .. } = &out.frame else {
                    tracing::warn!(%error, "feed item too large to send; skipped");
                    continue;
                };
                encode_frame(&reply(*id, Err(error)))?
            }
        };
        write
            .write_all(&line)
            .await
            .map_err(|e| ArchonError::io("writing a frame", e))?;
        write
            .flush()
            .await
            .map_err(|e| ArchonError::io("flushing a frame", e))?;
        if let Some(flushed) = out.flushed {
            // The waiter may have timed out already; nothing is lost then.
            let _ = flushed.send(());
        }
    }
    Ok(())
}

/// `hello`, then requests until the client leaves. Its feed subscriptions
/// end with it; they hold the outbox, so the writer would otherwise linger.
async fn session(
    ctx: &Arc<DaemonContext>,
    reader: &mut BufReader<OwnedReadHalf>,
    outbox: Outbox,
    stop: &watch::Sender<bool>,
) -> Result<(), ArchonError> {
    let Some(client) = handshake(reader, &outbox).await? else {
        return Ok(());
    };
    let mut subscriptions = 0u64;
    let mut feeds = JoinSet::new();
    loop {
        let frame = match read_client_frame(reader, &outbox).await? {
            Some(Some(frame)) => frame,
            Some(None) => continue,
            None => return Ok(()),
        };
        match frame.request {
            Request::SubscribeFeed { after, verbose } => {
                subscriptions += 1;
                let sub = subscription::Subscription {
                    id: subscriptions,
                    request: frame.id,
                    after,
                    verbose,
                };
                feeds.spawn(subscription::run(ctx.clone(), sub, outbox.clone()));
            }
            Request::Shutdown => return shutdown(ctx, client, frame.id, &outbox, stop).await,
            request => {
                let (ctx, outbox) = (ctx.clone(), outbox.clone());
                tokio::spawn(async move {
                    let result = handlers::handle(&ctx, client, request).await;
                    send(&outbox, reply(frame.id, result)).await
                });
            }
        }
    }
}

/// Reply first, wait until the reply is written, then stop the server.
async fn shutdown(
    ctx: &DaemonContext,
    client: ClientKind,
    id: u64,
    outbox: &Outbox,
    stop: &watch::Sender<bool>,
) -> Result<(), ArchonError> {
    let result = handlers::handle(ctx, client, Request::Shutdown).await;
    let refused = result.is_err();
    let (flushed, written) = oneshot::channel();
    let out = Outgoing {
        frame: reply(id, result),
        flushed: Some(flushed),
    };
    if outbox.send(out).await.is_ok() {
        // A client that vanished must not hold the shutdown up.
        let _ = tokio::time::timeout(SHUTDOWN_FLUSH, written).await;
    }
    if !refused {
        stop.send_replace(true);
    }
    Ok(())
}

/// The first frame must be `hello` at our protocol version; anything else
/// gets an id-0 error and the connection is closed.
async fn handshake(
    reader: &mut BufReader<OwnedReadHalf>,
    outbox: &Outbox,
) -> Result<Option<ClientKind>, ArchonError> {
    let Some(Some(frame)) = read_client_frame(reader, outbox).await? else {
        return Ok(None);
    };
    let Request::Hello {
        protocol_version,
        client,
        client_version,
    } = frame.request
    else {
        refuse(
            outbox,
            ErrorCode::InvalidArgument,
            "send hello first".into(),
        )
        .await;
        return Ok(None);
    };
    if protocol_version != PROTOCOL_VERSION {
        let message = format!(
            "archond speaks protocol {PROTOCOL_VERSION}, this client {client_version} speaks {protocol_version}; upgrade the older one"
        );
        refuse(outbox, ErrorCode::Incompatible, message).await;
        return Ok(None);
    }
    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
    };
    send(outbox, reply(frame.id, Ok(Reply::Welcome(welcome)))).await;
    tracing::debug!(?client, %client_version, "client connected");
    Ok(Some(client))
}

async fn refuse(outbox: &Outbox, code: ErrorCode, message: String) {
    let frame = ServerFrame::Reply {
        id: 0,
        result: Err(ErrorReply { code, message }),
    };
    send(outbox, frame).await;
}

/// `None` at end of stream; `Some(None)` for a well-formed line that is not
/// a request, which is answered with an error under its `req` when known.
async fn read_client_frame(
    reader: &mut BufReader<OwnedReadHalf>,
    outbox: &Outbox,
) -> Result<Option<Option<ClientFrame>>, ArchonError> {
    let value: serde_json::Value = match read_frame(reader).await {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(None),
        Err(error) => {
            // Framing is lost (oversized or truncated line): say why and close.
            refuse(outbox, ErrorCode::InvalidArgument, error.to_string()).await;
            return Err(error);
        }
    };
    let id = value
        .get("req")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    match serde_json::from_value::<ClientFrame>(value) {
        Ok(frame) => Ok(Some(Some(frame))),
        Err(error) => {
            let error = ArchonError::Invalid(format!("not a request: {error}"));
            send(outbox, reply(id, Err(error))).await;
            Ok(Some(None))
        }
    }
}
