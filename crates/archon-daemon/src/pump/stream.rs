//! One Aether subscription followed with a persisted cursor: resubscribe
//! from the cursor whenever the stream ends, until the connection drops.

use std::time::Duration;

use archon_aether::client::Subscription;
use archon_aether::wire::{EventQuery, ExpiredSpan};
use archon_aether::{AetherClient, AetherEvent};
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_queue::store::keys;

use super::{BACKOFF_MAX, BACKOFF_MIN, FLEET_EVENT_TYPES, ORCHESTRATOR_EVENT_TYPES};
use crate::context::DaemonContext;
use crate::feed;

/// Passes in a row without handling an event before the operator is told.
const STALLS_BEFORE_NOTE: u32 = 3;
/// How often a pump without an orchestrator checks its connection.
const IDLE_TICK: Duration = Duration::from_secs(1);
/// Attempts at an event whose handling failed for a passing reason (a held
/// git lock, a garbled reply), before it is reported and skipped.
const TRANSIENT_ATTEMPTS: u32 = 3;
const TRANSIENT_RETRY: Duration = Duration::from_secs(1);

/// The stored cursor of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    Unset,
    At(u64),
    /// Not a seq: replaying from 0 would reach expired history, so the
    /// stream starts live.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq)]
enum Stream {
    Fleet,
    Orchestrator(AetherAgentId),
}

impl Stream {
    fn name(&self) -> &'static str {
        match self {
            Self::Fleet => "fleet",
            Self::Orchestrator(_) => "orchestrator",
        }
    }

    fn cursor_key(&self) -> &'static str {
        match self {
            Self::Fleet => keys::CURSOR_FLEET,
            Self::Orchestrator(_) => keys::CURSOR_ORCHESTRATOR,
        }
    }

    fn query(&self) -> EventQuery {
        let (agent, types) = match self {
            // EventQuery accepts one agent, not a dynamic set of stage/node
            // recipients. The fleet must also discover new descendants; inbox
            // handling checks recorded ownership before routing a message.
            Self::Fleet => (None, FLEET_EVENT_TYPES),
            Self::Orchestrator(id) => (Some(id.clone()), ORCHESTRATOR_EVENT_TYPES),
        };
        EventQuery {
            before: None,
            agent,
            task: None,
            run: None,
            families: Vec::new(),
            types: types.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    /// Without a cursor the fleet starts live; the orchestrator stream is
    /// filtered to one agent, so its whole history is cheap and complete.
    fn start(&self, cursor: Cursor) -> Option<u64> {
        match (self, cursor) {
            (_, Cursor::At(seq)) => Some(seq),
            (_, Cursor::Unreadable) | (Self::Fleet, Cursor::Unset) => None,
            (Self::Orchestrator(_), Cursor::Unset) => Some(0),
        }
    }

    async fn handle(&self, ctx: &DaemonContext, event: &AetherEvent) -> Result<(), ArchonError> {
        match self {
            Self::Fleet => super::handle_fleet_event(ctx, event).await,
            Self::Orchestrator(_) => super::handle_orchestrator_event(ctx, event).await,
        }
    }
}

/// How one pass over a subscription ended.
#[derive(Debug)]
struct PassEnd {
    handled: bool,
    /// Why the stream stopped; `None` when the caller ended it on purpose.
    trouble: Option<String>,
}

pub(super) async fn follow_fleet(
    ctx: &DaemonContext,
    client: &AetherClient,
) -> Result<(), ArchonError> {
    let mut stalls = Stalls::default();
    while client.is_connected() {
        let end = pass(ctx, client, &Stream::Fleet, std::future::pending()).await?;
        stalls.after(ctx, client, "fleet", end).await?;
    }
    Ok(())
}

/// Follows whichever agent the supervisor currently names, switching
/// subscriptions when it adopts a new one.
pub(super) async fn follow_orchestrator(
    ctx: &DaemonContext,
    client: &AetherClient,
) -> Result<(), ArchonError> {
    let mut stalls = Stalls::default();
    let mut current = ctx.supervisor.watch();
    while client.is_connected() {
        let Some(agent) = current.borrow_and_update().clone() else {
            let _ = tokio::time::timeout(IDLE_TICK, current.changed()).await;
            continue;
        };
        let stream = Stream::Orchestrator(agent.clone());
        let mut watch = ctx.supervisor.watch();
        let replaced = async move {
            // The sender lives in `ctx`, so `wait_for` cannot fail here.
            let _ = watch.wait_for(|now| now.as_ref() != Some(&agent)).await;
        };
        let end = pass(ctx, client, &stream, replaced).await?;
        stalls.after(ctx, client, "orchestrator", end).await?;
    }
    Ok(())
}

/// Subscribe after the stored cursor and handle events until the stream
/// ends or `until` resolves. Only a store failure is an `Err`.
async fn pass(
    ctx: &DaemonContext,
    client: &AetherClient,
    stream: &Stream,
    until: impl Future<Output = ()>,
) -> Result<PassEnd, ArchonError> {
    let key = stream.cursor_key();
    let mut subscription = match open(ctx, client, stream).await? {
        Ok(subscription) => subscription,
        Err(trouble) => return Ok(PassEnd::stalled(false, trouble)),
    };
    tokio::pin!(until);
    let mut handled = false;
    loop {
        let next = tokio::select! {
            next = subscription.next() => next,
            () = &mut until => return Ok(PassEnd { handled, trouble: None }),
        };
        let event = match next {
            Ok(event) => event,
            Err(closed) => return Ok(PassEnd::stalled(handled, closed.to_string())),
        };
        if let Some(reason) = handle_one(ctx, stream, &event).await? {
            return Ok(PassEnd::stalled(handled, reason));
        }
        save_cursor(ctx, key, event.seq).await?;
        handled = true;
    }
}

/// Subscribe after the stored cursor, first stepping past history Aether
/// deleted. `Ok(Err(reason))` is a subscribe Aether did not grant.
async fn open(
    ctx: &DaemonContext,
    client: &AetherClient,
    stream: &Stream,
) -> Result<Result<Subscription, String>, ArchonError> {
    let key = stream.cursor_key();
    let cursor = read_cursor(ctx, key).await?;
    let mut after = stream.start(cursor);
    if let Some(from) = after {
        after = Some(skip_expired(ctx, client, stream, from).await?);
    }
    let subscription = match client.subscribe(after, stream.query()).await {
        Ok(subscription) => subscription,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => return Ok(Err(error.to_string())),
    };
    // An Aether whose history was reset reports a sequence before ours:
    // follow it from its own end rather than from a cursor it never had.
    let reset = after.is_some_and(|seq| subscription.last_seq < seq);
    if reset {
        tracing::warn!(
            key,
            last_seq = subscription.last_seq,
            "Aether's event history is behind the stored cursor; following from its end"
        );
    }
    if reset || !matches!(cursor, Cursor::At(_)) {
        save_cursor(ctx, key, subscription.last_seq).await?;
    }
    Ok(Ok(subscription))
}

/// Where a replay after `after` can start: past the span Aether's history
/// retention deleted when it reaches one. The skip is saved, reconciled from
/// Aether's current state and told to the operator once.
async fn skip_expired(
    ctx: &DaemonContext,
    client: &AetherClient,
    stream: &Stream,
    after: u64,
) -> Result<u64, ArchonError> {
    let span = match client.expired_history(after, stream.query()).await {
        Ok(None) => return Ok(after),
        Ok(Some(span)) => span,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            // The subscription reports its own trouble; this check only helps.
            tracing::warn!(stream = stream.name(), %error, "could not check for expired event history");
            return Ok(after);
        }
    };
    assert!(span.last > after, "the client checks the span is ahead");
    save_cursor(ctx, stream.cursor_key(), span.last).await?;
    tracing::warn!(stream = stream.name(), after, %span, "Aether's event history expired; skipping it");
    match super::reconcile(ctx).await {
        Err(error) if error.is_store() => return Err(error),
        Err(error) => tracing::warn!(%error, "reconcile after expired history failed"),
        Ok(()) => {}
    }
    feed::system(ctx, expired_note(stream, after, span)).await?;
    Ok(span.last)
}

fn expired_note(stream: &Stream, after: u64, span: ExpiredSpan) -> String {
    format!(
        "Aether deleted old event history (events {span}) before Archon read it. The {} stream \
         skipped events {}..={} and follows on from event {}; Archon reconciled its stages \
         with Aether's current state.",
        stream.name(),
        after + 1,
        span.last,
        span.last + 1
    )
}

impl PassEnd {
    fn stalled(handled: bool, reason: String) -> Self {
        Self {
            handled,
            trouble: Some(reason),
        }
    }
}

/// Handle one event. `Some(reason)` means Aether was unreachable and the
/// event must be retried; a passing failure is retried a few times here,
/// then reported and skipped like any other.
async fn handle_one(
    ctx: &DaemonContext,
    stream: &Stream,
    event: &AetherEvent,
) -> Result<Option<String>, ArchonError> {
    assert!(event.seq > 0, "event seqs start at 1");
    let subject = format!("Aether event {} ({})", event.seq, stream.name());
    let mut attempt = 1;
    let error = loop {
        let handled = crate::executor::coordination::guarded(&subject, stream.handle(ctx, event));
        match handled.await {
            Ok(()) => return Ok(None),
            Err(error) if error.is_store() => return Err(error),
            Err(ArchonError::Unavailable(reason)) => return Ok(Some(reason)),
            Err(error) if is_transient(&error) && attempt < TRANSIENT_ATTEMPTS => {
                tracing::info!(seq = event.seq, attempt, %error, "event handling failed; retrying");
                tokio::time::sleep(TRANSIENT_RETRY * attempt).await;
                attempt += 1;
            }
            Err(error) => break error,
        }
    };
    assert!(attempt <= TRANSIENT_ATTEMPTS, "retries are capped");
    tracing::warn!(seq = event.seq, stream = stream.name(), %error, "event not handled");
    let text = format!(
        "Archon could not act on Aether event {} ({:?}): {error}",
        event.seq, event.kind
    );
    feed::system(ctx, text).await?;
    Ok(None)
}

/// A failure that may pass on its own: local I/O (a held `.git/index.lock`),
/// a garbled reply, or an Aether error, which a later try may not repeat.
fn is_transient(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Io { .. } | ArchonError::Protocol(_) | ArchonError::Aether(_)
    )
}

async fn read_cursor(ctx: &DaemonContext, key: &str) -> Result<Cursor, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(key)?;
    match raw.filter(|v| !v.is_empty()) {
        None => Ok(Cursor::Unset),
        Some(raw) => match raw.parse() {
            Ok(seq) => Ok(Cursor::At(seq)),
            Err(error) => {
                tracing::warn!(key, %raw, %error, "unreadable event cursor; following the stream live");
                Ok(Cursor::Unreadable)
            }
        },
    }
}

async fn save_cursor(ctx: &DaemonContext, key: &str, seq: u64) -> Result<(), ArchonError> {
    ctx.store.lock().await.kv_set(key, &seq.to_string())
}

/// Backs off between passes that made no progress, and tells the operator
/// once when a stream stays stuck (e.g. an event Archon cannot decode).
#[derive(Debug)]
struct Stalls {
    count: u32,
    backoff: Duration,
}

impl Default for Stalls {
    fn default() -> Self {
        Self {
            count: 0,
            backoff: BACKOFF_MIN,
        }
    }
}

impl Stalls {
    async fn after(
        &mut self,
        ctx: &DaemonContext,
        client: &AetherClient,
        name: &str,
        end: PassEnd,
    ) -> Result<(), ArchonError> {
        let Some(reason) = end.trouble.filter(|_| !end.handled) else {
            *self = Self::default();
            return Ok(());
        };
        if !client.is_connected() {
            return Ok(());
        }
        self.count += 1;
        tracing::warn!(stream = name, %reason, stalls = self.count, "event stream stalled");
        if self.count == STALLS_BEFORE_NOTE {
            let text = format!(
                "Archon is stuck reading Aether's {name} events ({reason}); it keeps retrying."
            );
            feed::system(ctx, text).await?;
        }
        tokio::time::sleep(self.backoff).await;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        Ok(())
    }
}
