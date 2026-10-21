//! One Aether subscription followed with a persisted cursor: resubscribe
//! from the cursor whenever the stream ends, until the connection drops.

use std::time::Duration;

use archon_aether::wire::EventQuery;
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
            agent,
            task: None,
            run: None,
            families: Vec::new(),
            types: types.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    /// Without a cursor the fleet starts live; the orchestrator stream is
    /// filtered to one agent, so its whole history is cheap and complete.
    fn start(&self, cursor: Option<u64>) -> Option<u64> {
        match self {
            Self::Fleet => cursor,
            Self::Orchestrator(_) => Some(cursor.unwrap_or(0)),
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
    let cursor = read_cursor(ctx, key).await?;
    let mut subscription = match client.subscribe(stream.start(cursor), stream.query()).await {
        Ok(subscription) => subscription,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => return Ok(PassEnd::stalled(false, error.to_string())),
    };
    assert!(
        cursor.is_none_or(|c| subscription.last_seq >= c),
        "a subscription after the cursor starts at or after it"
    );
    if cursor.is_none() {
        save_cursor(ctx, key, subscription.last_seq).await?;
    }
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

impl PassEnd {
    fn stalled(handled: bool, reason: String) -> Self {
        Self {
            handled,
            trouble: Some(reason),
        }
    }
}

/// Handle one event. `Some(reason)` means Aether was unreachable and the
/// event must be retried; other failures are reported and skipped.
async fn handle_one(
    ctx: &DaemonContext,
    stream: &Stream,
    event: &AetherEvent,
) -> Result<Option<String>, ArchonError> {
    assert!(event.seq > 0, "event seqs start at 1");
    let subject = format!("Aether event {} ({})", event.seq, stream.name());
    let handled = crate::executor::coordination::guarded(&subject, stream.handle(ctx, event));
    match handled.await {
        Ok(()) => Ok(None),
        Err(error) if error.is_store() => Err(error),
        Err(ArchonError::Unavailable(reason)) => Ok(Some(reason)),
        Err(error) => {
            tracing::warn!(seq = event.seq, stream = stream.name(), %error, "event not handled");
            let text = format!(
                "Archon could not act on Aether event {} ({:?}): {error}",
                event.seq, event.kind
            );
            feed::system(ctx, text).await?;
            Ok(None)
        }
    }
}

async fn read_cursor(ctx: &DaemonContext, key: &str) -> Result<Option<u64>, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(key)?;
    match raw.filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(raw) => match raw.parse() {
            Ok(seq) => Ok(Some(seq)),
            Err(error) => {
                tracing::warn!(key, %raw, %error, "unreadable event cursor; replaying from the start");
                Ok(Some(0))
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
