//! The machinery under [`crate::ArchonClient`]: one writer task, one reader
//! task that routes replies by id and feed frames to their subscription, and
//! the waiters table a dropped connection fails in one sweep.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::notify::FeedItem;
use archon_core::protocol::{ClientFrame, ErrorReply, Reply, Request, ServerFrame};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::framing;

/// Lines queued for the writer; small, since each request awaits its reply.
pub(crate) const WRITE_QUEUE: usize = 64;
/// Feed items buffered per subscription before it is closed as overflowed.
pub(crate) const FEED_CAPACITY: usize = 1024;
const CLOSED_WITHOUT_REASON: &str = "archond closed the connection";

/// A transport failure (outer) or the daemon's own answer, which may be a
/// refusal (inner).
pub(crate) type Answer = Result<Result<Reply, ErrorReply>, ArchonError>;

/// One feed subscription's channel and why it ended. The reason is set
/// before the sender drops, so the consumer drains what was accepted first.
#[derive(Debug)]
pub(crate) struct FeedSink {
    items: mpsc::Sender<FeedItem>,
    closed: Arc<OnceLock<String>>,
    /// Highest seq delivered; a subscription never goes backwards.
    last_seq: u64,
}

impl FeedSink {
    pub fn new(
        items: mpsc::Sender<FeedItem>,
        closed: Arc<OnceLock<String>>,
        after: Option<u64>,
    ) -> Self {
        Self {
            items,
            closed,
            last_seq: after.unwrap_or(0),
        }
    }

    fn close(self, reason: String) {
        // First reason wins: a later disconnect would hide an overflow.
        let _already_closed = self.closed.set(reason);
    }
}

#[derive(Debug)]
enum Pending {
    Reply(oneshot::Sender<Answer>),
    Subscribe(oneshot::Sender<Answer>, FeedSink),
}

#[derive(Debug, Default)]
struct Waiters {
    /// Set once, when the connection is known dead; no request waits after.
    dead: Option<String>,
    pending: HashMap<u64, Pending>,
}

/// State shared by the client handle, the writer and the reader.
#[derive(Debug)]
pub(crate) struct Shared {
    waiters: Mutex<Waiters>,
    next_req: AtomicU64,
    outgoing: mpsc::Sender<Vec<u8>>,
}

/// Removes a request's waiter when its caller stops waiting (timeout or a
/// dropped future), so an abandoned request never leaks an entry.
struct Forget<'a> {
    shared: &'a Shared,
    req: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.shared.lock().pending.remove(&self.req);
    }
}

impl Shared {
    pub fn new(outgoing: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            waiters: Mutex::new(Waiters::default()),
            next_req: AtomicU64::new(1),
            outgoing,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Waiters> {
        // The table holds only senders; a panic elsewhere leaves it coherent.
        self.waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn disconnect_reason(&self) -> Option<String> {
        self.lock().dead.clone()
    }

    /// Send one request and wait at most `timeout` for its answer. `sink` is
    /// set only for `subscribe_feed`, whose reply registers the stream.
    pub async fn call(
        &self,
        request: Request,
        sink: Option<FeedSink>,
        timeout: Duration,
    ) -> Answer {
        assert!(!timeout.is_zero(), "a zero timeout fails every request");
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        assert!(req > 0, "request id 0 is reserved for daemon notices");
        let (cmd, line) = encode_request(req, request)?;
        let (tx, rx) = oneshot::channel();
        self.register(req, tx, sink)?;
        let _forget = Forget { shared: self, req };
        // Queueing counts against the timeout: a daemon that stopped reading
        // stalls the writer, and then the queue.
        let exchange = async {
            if self.outgoing.send(line).await.is_err() {
                return Err(self.dead_error());
            }
            rx.await.unwrap_or_else(|_dropped| Err(self.dead_error()))
        };
        match tokio::time::timeout(timeout, exchange).await {
            Ok(answer) => answer,
            Err(_elapsed) => Err(ArchonError::Unavailable(format!(
                "archond did not answer `{cmd}` within {}s",
                timeout.as_secs_f64()
            ))),
        }
    }

    fn register(
        &self,
        req: u64,
        tx: oneshot::Sender<Answer>,
        sink: Option<FeedSink>,
    ) -> Result<(), ArchonError> {
        let mut waiters = self.lock();
        if let Some(reason) = &waiters.dead {
            return Err(ArchonError::Unavailable(reason.clone()));
        }
        let pending = match sink {
            Some(sink) => Pending::Subscribe(tx, sink),
            None => Pending::Reply(tx),
        };
        let previous = waiters.pending.insert(req, pending);
        assert!(previous.is_none(), "request id {req} was issued twice");
        Ok(())
    }

    fn take(&self, req: u64) -> Option<Pending> {
        self.lock().pending.remove(&req)
    }

    /// Whether a `subscribe_feed` reply is still outstanding, i.e. whether a
    /// feed frame for an unknown subscription may yet find its owner.
    fn subscribe_pending(&self) -> bool {
        self.lock()
            .pending
            .values()
            .any(|p| matches!(p, Pending::Subscribe(..)))
    }

    fn dead_error(&self) -> ArchonError {
        let reason = self.disconnect_reason();
        ArchonError::Unavailable(reason.unwrap_or_else(|| CLOSED_WITHOUT_REASON.to_owned()))
    }

    /// Mark the connection dead and fail every waiter. Idempotent: the first
    /// reason is the one everybody sees.
    pub fn fail_all(&self, reason: &str) {
        let (reason, drained) = {
            let mut waiters = self.lock();
            let reason = waiters
                .dead
                .get_or_insert_with(|| reason.to_owned())
                .clone();
            let drained: Vec<Pending> = waiters.pending.drain().map(|(_, p)| p).collect();
            (reason, drained)
        };
        for pending in drained {
            let tx = match pending {
                Pending::Reply(tx) => tx,
                Pending::Subscribe(tx, sink) => {
                    sink.close(reason.clone());
                    tx
                }
            };
            let _caller_gone = tx.send(Err(ArchonError::Unavailable(reason.clone())));
        }
    }
}

/// The frame's bytes and its `cmd` name, for error messages.
fn encode_request(req: u64, request: Request) -> Result<(String, Vec<u8>), ArchonError> {
    #[derive(serde::Deserialize)]
    struct Tag<'a> {
        cmd: &'a str,
    }
    let line = framing::encode_frame(&ClientFrame { id: req, request }).map_err(|e| match e {
        ArchonError::Protocol(why) => ArchonError::Invalid(format!("request refused: {why}")),
        other => other,
    })?;
    let tag: Tag<'_> = serde_json::from_slice(&line)?;
    assert!(!tag.cmd.is_empty(), "every request has a `cmd` tag");
    Ok((tag.cmd.to_owned(), line))
}

/// Writes queued lines in order. Exits when every sender is gone or the
/// socket fails, failing all waiters in the latter case.
pub(crate) async fn run_writer<W>(
    mut write: W,
    mut lines: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(line) = lines.recv().await {
        debug_assert_eq!(line.last(), Some(&b'\n'), "frames end in a newline");
        let written = async {
            write.write_all(&line).await?;
            write.flush().await
        };
        if let Err(error) = written.await {
            shared.fail_all(&format!("writing to archond failed: {error}"));
            return;
        }
    }
}

/// Feed frames that arrived before their subscription's reply.
#[derive(Debug, Default)]
struct EarlyItems {
    items: Vec<FeedItem>,
    overflowed: bool,
}

/// The reader task's routing state. Owned by that task alone, so registering
/// a subscription and routing its next frame cannot race.
#[derive(Debug)]
pub(crate) struct Router {
    shared: Arc<Shared>,
    subs: HashMap<u64, FeedSink>,
    early: HashMap<u64, EarlyItems>,
}

impl Router {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            subs: HashMap::new(),
            early: HashMap::new(),
        }
    }

    /// Route frames until the connection ends, then fail everything. A frame
    /// that does not decode is fatal: which reply is which can't be trusted.
    /// So is an id-0 notice, even when archond leaves the socket open.
    pub async fn run<R: AsyncRead + Unpin>(mut self, read: R) {
        let mut reader = BufReader::new(read);
        let reason = loop {
            match framing::read_frame::<_, ServerFrame>(&mut reader).await {
                Ok(Some(frame)) => {
                    if let ControlFlow::Break(notice) = self.route(frame) {
                        break notice;
                    }
                }
                Ok(None) => break CLOSED_WITHOUT_REASON.to_owned(),
                Err(error) => break format!("reading from archond failed: {error}"),
            }
        };
        self.shutdown(&reason);
    }

    fn shutdown(&mut self, reason: &str) {
        assert!(!reason.is_empty(), "a closed connection says why");
        self.shared.fail_all(reason);
        // The first reason wins, e.g. a writer failure that raced this one.
        let reason = self
            .shared
            .disconnect_reason()
            .expect("fail_all marks the connection dead");
        for (_, sink) in self.subs.drain() {
            sink.close(reason.clone());
        }
        self.early.clear();
    }

    /// Deliver one frame; `Break` with the reason when it ends the connection.
    fn route(&mut self, frame: ServerFrame) -> ControlFlow<String> {
        match frame {
            ServerFrame::Reply { id: 0, result } => return ControlFlow::Break(notice_text(result)),
            ServerFrame::Reply { id, result } => self.answer(id, result),
            ServerFrame::Feed { sub, item } => self.feed(sub, item),
        }
        ControlFlow::Continue(())
    }

    fn answer(&mut self, id: u64, result: Result<Reply, ErrorReply>) {
        match self.shared.take(id) {
            None => tracing::debug!(id, "reply to a request nobody waits for (timed out?)"),
            Some(Pending::Reply(tx)) => {
                let _caller_gone = tx.send(Ok(result));
            }
            Some(Pending::Subscribe(tx, sink)) => {
                let answer = match result {
                    Ok(reply) => self.register(reply, sink),
                    Err(refusal) => Ok(Err(refusal)),
                };
                if !self.shared.subscribe_pending() {
                    self.early.clear();
                }
                let _caller_gone = tx.send(answer);
            }
        }
    }

    /// Install a subscription the moment its reply is read, then deliver the
    /// frames that raced ahead of the reply.
    fn register(&mut self, reply: Reply, sink: FeedSink) -> Answer {
        let Reply::Subscribed { id, last_seq } = reply else {
            return Err(ArchonError::Protocol(format!(
                "archond answered subscribe_feed with `{}`, not `subscribed`",
                reply_kind(&reply)
            )));
        };
        if self.subs.contains_key(&id) {
            return Err(ArchonError::Protocol(format!(
                "archond reused feed subscription id {id}"
            )));
        }
        let early = self.early.remove(&id).unwrap_or_default();
        self.subs.insert(id, sink);
        if early.overflowed {
            self.close(id, overflow_reason(id));
        }
        for item in early.items {
            self.deliver(id, item);
        }
        Ok(Ok(Reply::Subscribed { id, last_seq }))
    }

    fn feed(&mut self, sub: u64, item: FeedItem) {
        if self.subs.contains_key(&sub) {
            self.deliver(sub, item);
            return;
        }
        if !self.shared.subscribe_pending() {
            tracing::debug!(sub, seq = item.seq, "feed frame for a closed subscription");
            return;
        }
        let early = self.early.entry(sub).or_default();
        if early.items.len() >= FEED_CAPACITY {
            early.overflowed = true;
            early.items.clear();
        }
        if !early.overflowed {
            early.items.push(item);
        }
    }

    fn deliver(&mut self, sub: u64, item: FeedItem) {
        let Some(sink) = self.subs.get_mut(&sub) else {
            return;
        };
        if item.seq <= sink.last_seq {
            let reason = format!(
                "feed subscription {sub} went backwards: seq {} after {}",
                item.seq, sink.last_seq
            );
            self.close(sub, reason);
            return;
        }
        let seq = item.seq;
        match sink.items.try_send(item) {
            Ok(()) => sink.last_seq = seq,
            Err(mpsc::error::TrySendError::Full(_)) => self.close(sub, overflow_reason(sub)),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The consumer dropped its receiver; nobody is left to tell.
                tracing::debug!(sub, "feed subscription dropped by its consumer");
                self.subs.remove(&sub);
            }
        }
    }

    fn close(&mut self, sub: u64, reason: String) {
        let Some(sink) = self.subs.remove(&sub) else {
            return;
        };
        tracing::warn!(sub, %reason, "closing a feed subscription");
        sink.close(reason);
    }
}

fn overflow_reason(sub: u64) -> String {
    format!(
        "feed subscription {sub} overflowed its {FEED_CAPACITY}-item buffer; resubscribe with \
         `after` set to the last handled seq"
    )
}

fn notice_text(result: Result<Reply, ErrorReply>) -> String {
    match result {
        Err(refusal) => format!("archond closed the connection: {}", refusal.message),
        Ok(reply) => format!("archond sent an unexpected `{}` notice", reply_kind(&reply)),
    }
}

/// The wire name of a reply (`item`, `subscribed`, ...), for messages.
pub(crate) fn reply_kind(reply: &Reply) -> String {
    serde_json::to_value(reply)
        .ok()
        .and_then(|v| v.get("reply").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_encode_with_req_and_cmd_on_one_line() {
        let (cmd, line) = encode_request(3, Request::Status(Default::default())).unwrap();
        assert_eq!(cmd, "status");
        assert_eq!(line, b"{\"req\":3,\"cmd\":\"status\"}\n");
    }

    #[test]
    fn reply_kinds_are_the_wire_tags() {
        assert_eq!(reply_kind(&Reply::Ok), "ok");
        let subscribed = Reply::Subscribed { id: 1, last_seq: 0 };
        assert_eq!(reply_kind(&subscribed), "subscribed");
    }
}
