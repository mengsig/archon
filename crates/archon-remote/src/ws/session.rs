//! One device's WebSocket session: frames in, replies and feed frames out,
//! until the peer leaves, goes silent, or archond closes it.

use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::protocol::{
    ClientFrame, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, ServerFrame, Welcome,
};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use crate::sessions::Registration;
use crate::{Backend, FeedSubscription};

/// The largest message a device may send; the app sends small requests.
pub const MAX_MESSAGE: usize = 1024 * 1024;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(25);
/// A session that sent nothing, not even a pong, for this long is closed.
const SILENT_AFTER: Duration = Duration::from_secs(75);
/// A peer that takes longer than this to accept one frame is not reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOX_FRAMES: usize = 256;
const MAX_IN_FLIGHT: usize = 16;
const MAX_SUBSCRIPTIONS: u64 = 4;

/// How a session ended, and so how its socket is closed.
#[derive(Debug)]
enum Ended {
    Peer,
    Closed(crate::sessions::Close),
    Violation(CloseCode, &'static str),
}

struct Session {
    backend: Arc<dyn Backend>,
    outbox: mpsc::Sender<ServerFrame>,
    in_flight: Arc<Semaphore>,
    tasks: JoinSet<()>,
    subscriptions: u64,
}

pub async fn carry<S>(io: S, backend: Arc<dyn Backend>, mut registration: Registration)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let mut socket = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await;
    let (outbox, mut frames) = mpsc::channel(OUTBOX_FRAMES);
    let mut session = Session {
        backend,
        outbox,
        in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        tasks: JoinSet::new(),
        subscriptions: 0,
    };
    let ended = match greet(&mut socket, &mut registration).await {
        Ok(()) => run(&mut socket, &mut session, &mut frames, &mut registration).await,
        Err(ended) => ended,
    };
    session.tasks.abort_all();
    tracing::debug!(?ended, "a remote session ended");
    let close = match ended {
        Ended::Peer => return,
        Ended::Closed(why) => (CloseCode::from(why.code()), why.reason()),
        Ended::Violation(code, reason) => (code, reason),
    };
    let frame = CloseFrame {
        code: close.0,
        reason: close.1.into(),
    };
    let _gone =
        tokio::time::timeout(Duration::from_secs(2), close_gracefully(&mut socket, frame)).await;
}

/// Send the close, then read until the device answers it. Dropping a socket
/// with unread input (a late pong) resets it, and the device loses the code.
async fn close_gracefully<S>(
    socket: &mut WebSocketStream<S>,
    frame: CloseFrame,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    socket.close(Some(frame)).await?;
    while socket.next().await.transpose()?.is_some() {}
    Ok(())
}

/// The first message must be `hello` at archond's protocol version.
async fn greet<S>(
    socket: &mut WebSocketStream<S>,
    registration: &mut Registration,
) -> Result<(), Ended>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first = tokio::select! {
        first = tokio::time::timeout(HELLO_TIMEOUT, socket.next()) => first,
        _ = registration.closed.changed() => return Err(closed(registration)),
    };
    let text = match first {
        Err(_) => return Err(Ended::Violation(CloseCode::Policy, "send hello first")),
        Ok(None | Some(Err(_))) => return Err(Ended::Peer),
        Ok(Some(Ok(message))) => text_of(message)?,
    };
    let frame: ClientFrame = serde_json::from_str(&text)
        .map_err(|_| Ended::Violation(CloseCode::Policy, "send hello first"))?;
    let Request::Hello {
        protocol_version, ..
    } = frame.request
    else {
        return Err(Ended::Violation(CloseCode::Policy, "send hello first"));
    };
    if protocol_version != PROTOCOL_VERSION {
        let message = format!("archond speaks protocol {PROTOCOL_VERSION}; reload the app");
        let refusal = error_frame(0, ErrorCode::Incompatible, message);
        send(socket, &refusal).await?;
        return Err(Ended::Violation(CloseCode::Policy, "protocol mismatch"));
    }
    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
    };
    let reply = ServerFrame::Reply {
        id: frame.id,
        result: Ok(Reply::Welcome(welcome)),
    };
    send(socket, &reply).await
}

async fn run<S>(
    socket: &mut WebSocketStream<S>,
    session: &mut Session,
    frames: &mut mpsc::Receiver<ServerFrame>,
    registration: &mut Registration,
) -> Ended
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut heard = tokio::time::Instant::now();
    loop {
        reap(&mut session.tasks);
        // Biased: a revoke or a disable first, then draining replies before
        // reading more, so only a device that stops reading fills the outbox.
        let step = tokio::select! {
            biased;
            _ = registration.closed.changed() => Err(closed(registration)),
            Some(frame) = frames.recv() => unless_closed(registration, send(socket, &frame)).await,
            incoming = socket.next() => match incoming {
                Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                    Err(Ended::Violation(CloseCode::Size, "message too large"))
                }
                None | Some(Err(_)) => Err(Ended::Peer),
                Some(Ok(message)) => {
                    heard = tokio::time::Instant::now();
                    receive(session, message)
                }
            },
            _ = ping.tick() => match heard.elapsed() > SILENT_AFTER {
                true => Err(Ended::Violation(CloseCode::Away, "silent too long")),
                false => unless_closed(registration, send_message(socket, Message::Ping(Default::default()))).await,
            },
        };
        if let Err(ended) = step {
            return ended;
        }
    }
}

/// Drop finished request and feed tasks every turn, so a busy socket that
/// always wins the select cannot pile them up.
fn reap(tasks: &mut JoinSet<()>) {
    while let Some(done) = tasks.try_join_next() {
        if let Err(error) = done {
            assert!(
                !error.is_cancelled(),
                "session tasks are only aborted once it ends"
            );
            tracing::error!(%error, "a remote request task panicked");
        }
    }
}

/// A write to the device, abandoned as soon as archond closes the session.
async fn unless_closed(
    registration: &mut Registration,
    sending: impl std::future::Future<Output = Result<(), Ended>>,
) -> Result<(), Ended> {
    tokio::select! {
        biased;
        _ = registration.closed.changed() => Err(closed(registration)),
        sent = sending => sent,
    }
}

fn closed(registration: &Registration) -> Ended {
    let why = registration
        .closed
        .borrow()
        .expect("a close is only signalled with a reason");
    Ended::Closed(why)
}

/// One message from the device; requests run on their own tasks.
fn receive(session: &mut Session, message: Message) -> Result<(), Ended> {
    let text = match message {
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(()),
        Message::Close(_) => return Err(Ended::Peer),
        other => text_of(other)?,
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| Ended::Violation(CloseCode::Invalid, "each message is one JSON frame"))?;
    let id = value
        .get("req")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let request = match serde_json::from_value::<ClientFrame>(value) {
        Ok(frame) => frame.request,
        Err(error) => {
            let message = format!("not a request: {error}");
            return queue(
                session,
                error_frame(id, ErrorCode::InvalidArgument, message),
            );
        }
    };
    dispatch(session, id, request)
}

fn dispatch(session: &mut Session, id: u64, request: Request) -> Result<(), Ended> {
    if !crate::allow::allowed(&request) || matches!(request, Request::Hello { .. }) {
        let name = crate::allow::command_name(&request);
        let message = format!("{name} is not available from the web app; use the archon CLI");
        return queue(session, error_frame(id, ErrorCode::Forbidden, message));
    }
    if let Request::SubscribeFeed { after, verbose } = request {
        return subscribe(session, id, after, verbose);
    }
    let Ok(permit) = session.in_flight.clone().try_acquire_owned() else {
        let message =
            format!("at most {MAX_IN_FLIGHT} requests may be in flight; wait for a reply");
        return queue(session, error_frame(id, ErrorCode::Unavailable, message));
    };
    let (backend, outbox) = (session.backend.clone(), session.outbox.clone());
    session.tasks.spawn(async move {
        let result = backend.handle(request).await;
        let frame = ServerFrame::Reply {
            id,
            result: result.map_err(|e| ErrorReply::from(&e)),
        };
        let _gone = outbox.send(frame).await;
        drop(permit);
    });
    Ok(())
}

fn subscribe(
    session: &mut Session,
    id: u64,
    after: Option<u64>,
    verbose: bool,
) -> Result<(), Ended> {
    if session.subscriptions >= MAX_SUBSCRIPTIONS {
        let message = format!("at most {MAX_SUBSCRIPTIONS} feed subscriptions per connection");
        return queue(
            session,
            error_frame(id, ErrorCode::InvalidArgument, message),
        );
    }
    session.subscriptions += 1;
    let subscription = FeedSubscription {
        id: session.subscriptions,
        request: id,
        after,
        verbose,
    };
    let (backend, outbox) = (session.backend.clone(), session.outbox.clone());
    session
        .tasks
        .spawn(async move { backend.feed(subscription, outbox).await });
    Ok(())
}

/// Never waits: the loop that drains the outbox is the caller, so a full
/// outbox means the device sends faster than it reads.
fn queue(session: &Session, frame: ServerFrame) -> Result<(), Ended> {
    session.outbox.try_send(frame).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => {
            Ended::Violation(CloseCode::Policy, "too many replies are waiting to be read")
        }
        mpsc::error::TrySendError::Closed(_) => Ended::Peer,
    })
}

fn text_of(message: Message) -> Result<String, Ended> {
    match message {
        Message::Text(text) => Ok(text.as_str().to_owned()),
        _ => Err(Ended::Violation(CloseCode::Unsupported, "text frames only")),
    }
}

fn error_frame(id: u64, code: ErrorCode, message: String) -> ServerFrame {
    ServerFrame::Reply {
        id,
        result: Err(ErrorReply { code, message }),
    }
}

async fn send<S>(socket: &mut WebSocketStream<S>, frame: &ServerFrame) -> Result<(), Ended>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let text = match serde_json::to_string(frame) {
        Ok(text) => text,
        Err(error) => {
            let error = ArchonError::Invalid(format!("a reply could not be encoded: {error}"));
            tracing::error!(%error, "remote frame dropped");
            let ServerFrame::Reply { id, .. } = frame else {
                return Ok(());
            };
            // The device still learns its request failed.
            let refusal = error_frame(*id, ErrorCode::Internal, error.to_string());
            serde_json::to_string(&refusal).expect("an error frame encodes")
        }
    };
    send_message(socket, Message::text(text)).await
}

/// One message to the device, given up on after [`SEND_TIMEOUT`].
async fn send_message<S>(socket: &mut WebSocketStream<S>, message: Message) -> Result<(), Ended>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await {
        Ok(sent) => sent.map_err(|_| Ended::Peer),
        Err(_) => Err(Ended::Violation(CloseCode::Away, "not reading")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finished_tasks_are_reaped_without_waiting_on_running_ones() {
        let mut tasks = JoinSet::new();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        tasks.spawn(async move {
            let _released = held.await;
        });
        for _ in 0..8 {
            tasks.spawn(async {});
        }
        tokio::task::yield_now().await;
        while tasks.len() > 1 {
            reap(&mut tasks);
            tokio::task::yield_now().await;
        }
        assert_eq!(tasks.len(), 1, "only the running task is left");
        release.send(()).unwrap();
        tasks.join_next().await.unwrap().unwrap();
        reap(&mut tasks);
        assert!(tasks.is_empty());
    }
}
