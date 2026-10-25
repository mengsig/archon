//! Tracked changes (`archon_core::tracked`). A change `archon mcp` forwards
//! under a request id is known from the moment archond reads it: in memory
//! while it runs, then in the store with its outcome, so `request_status`
//! can report it after the bridge stopped waiting. Under an idempotency key
//! it is claimed in the store before it runs, and a repeat of the same call
//! returns the first outcome instead of running again.

use std::collections::HashMap;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::RequestId;
use archon_core::protocol::{ErrorReply, Reply, Request};
use archon_core::tracked::{HELD_MAX, RequestStatus, TrackedState, check_key};
use archon_queue::store::{Claim, TrackedRequest};

use crate::context::DaemonContext;

/// The request id and key a `tracked` request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tracking {
    id: RequestId,
    key: Option<String>,
}

/// The tracked requests running now, by id, with the caller each is for,
/// and those whose outcome the store refused, until it records them.
#[derive(Debug, Default)]
pub struct InFlight {
    running: std::sync::Mutex<HashMap<RequestId, (String, RequestStatus)>>,
    unrecorded: std::sync::Mutex<Vec<TrackedRequest>>,
}

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RequestId, (String, RequestStatus)>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unrecorded(&self) -> std::sync::MutexGuard<'_, Vec<TrackedRequest>> {
        self.unrecorded.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A tracked request from the moment archond read it until it is recorded;
/// dropping it forgets the request in memory, unless its outcome could not
/// be recorded and memory keeps it instead.
#[derive(Debug)]
pub(crate) struct Ticket<'a> {
    ctx: &'a DaemonContext,
    record: TrackedRequest,
    kept: bool,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.kept {
            self.ctx.tracked.lock().remove(&self.record.id);
        }
    }
}

/// What a claim decided: run the request, or answer with the first outcome.
#[derive(Debug)]
pub(crate) enum Begun<'a> {
    Run(Ticket<'a>),
    Replay(Box<Reply>),
}

/// Peel a `tracked` wrapper off `request`, refusing one around a read, a
/// `for_agent` or another `tracked`, and a malformed key.
pub(crate) fn unwrap(request: Request) -> Result<(Option<Tracking>, Request), ArchonError> {
    let Request::Tracked {
        request_id,
        idempotency_key,
        request,
    } = request
    else {
        return Ok((None, request));
    };
    if matches!(*request, Request::Tracked { .. } | Request::ForAgent { .. }) {
        return Err(ArchonError::Invalid(
            "tracked carries one plain request, not a tracked or for_agent one".into(),
        ));
    }
    if request.is_read_only() {
        return Err(ArchonError::Invalid(
            "only a change is tracked; a read is always safe to repeat".into(),
        ));
    }
    if let Some(key) = &idempotency_key {
        check_key(key)?;
    }
    let tracking = Tracking {
        id: request_id,
        key: idempotency_key,
    };
    Ok((Some(tracking), *request))
}

/// Make `request` known as pending for `caller`, before anything else can
/// refuse it; refused here, unapplied, while memory holds [`HELD_MAX`].
pub(crate) fn enter<'a>(
    ctx: &'a DaemonContext,
    caller: Option<&AetherAgentId>,
    tracking: Tracking,
    request: &Request,
) -> Result<Ticket<'a>, ArchonError> {
    let canonical = serde_json::to_value(request)
        .and_then(|value| serde_json::to_string(&value))
        .map_err(|e| ArchonError::Invalid(format!("the request does not encode: {e}")))?;
    let record = TrackedRequest {
        id: tracking.id,
        caller: caller_key(caller),
        command: crate::handlers::command_name(request),
        idempotency_key: tracking.key,
        request: canonical,
        started_at: archon_core::now(),
    };
    let pending = RequestStatus {
        request: record.id.clone(),
        command: record.command.clone(),
        state: TrackedState::Pending,
        idempotency_key: record.idempotency_key.clone(),
        reply: None,
        error: None,
        started_at: record.started_at,
        finished_at: None,
    };
    let mut running = ctx.tracked.lock();
    if running.len() >= HELD_MAX {
        return Err(ArchonError::Unavailable(format!(
            "archond holds {} tracked requests, running or with an outcome the store has not \
             recorded yet; nothing was changed. Try again later.",
            running.len()
        )));
    }
    if running.contains_key(&record.id) {
        return Err(ArchonError::Invalid(format!(
            "request {} is already running; every call needs a new request id",
            record.id
        )));
    }
    running.insert(record.id.clone(), (record.caller.clone(), pending));
    drop(running);
    Ok(Ticket {
        ctx,
        record,
        kept: false,
    })
}

/// Claim the ticket's key in the store, or find the call it was used for.
/// An unkeyed request only checks its id is new.
pub(crate) async fn claim(ticket: Ticket<'_>) -> Result<Begun<'_>, ArchonError> {
    let ctx = ticket.ctx;
    if ticket.record.idempotency_key.is_none() {
        if ctx
            .read_store()
            .await?
            .tracked_status(&ticket.record.id)?
            .is_some()
        {
            return Err(ArchonError::Invalid(format!(
                "request {} was made before; every call needs a new request id",
                ticket.record.id
            )));
        }
        return Ok(Begun::Run(ticket));
    }
    let claim = ctx.store.lock().await.claim_tracked(&ticket.record)?;
    match claim {
        Claim::New => Ok(Begun::Run(ticket)),
        Claim::Earlier { request, status } => {
            replay(&ticket.record, &request, kept_or(ctx, status))
        }
    }
}

/// The first outcome of a call repeated under its key: the reply itself once
/// applied, else its status. The same key for another request is refused.
fn replay(
    record: &TrackedRequest,
    earlier: &str,
    status: RequestStatus,
) -> Result<Begun<'static>, ArchonError> {
    let key = record.idempotency_key.as_deref().unwrap_or_default();
    assert_eq!(
        status.idempotency_key.as_deref(),
        Some(key),
        "the key found it"
    );
    if earlier != record.request {
        return Err(ArchonError::Invalid(format!(
            "idempotency_key `{key}` was already used for a different `{}` call ({}); repeat \
             that call's exact arguments, or use a new key for this one. Nothing was changed.",
            record.command, status.request
        )));
    }
    tracing::info!(request = %status.request, state = status.state.as_str(), "replayed a keyed call");
    match status.state {
        TrackedState::Applied => {
            let reply = status.reply.expect("an applied request keeps its reply");
            Ok(Begun::Replay(reply))
        }
        _ => Ok(Begun::Replay(Box::new(Reply::RequestStatus(Box::new(
            status,
        ))))),
    }
}

/// The status memory keeps for a request a store row still shows pending:
/// its outcome, when archond could not record it.
fn kept_or(ctx: &DaemonContext, stored: RequestStatus) -> RequestStatus {
    if stored.state != TrackedState::Pending {
        return stored;
    }
    let kept = ctx.tracked.lock().get(&stored.request).cloned();
    kept.map_or(stored, |(_, status)| status)
}

/// Record how the ticket's request ended, then forget it in memory. When the
/// store refuses the record, memory keeps the outcome for `request_status`
/// until [`record_kept`] records it, and the caller gets the store's error,
/// saying what became of the change so that it is not repeated.
pub(crate) async fn finish(
    mut ticket: Ticket<'_>,
    result: &Result<Reply, ArchonError>,
) -> Result<(), ArchonError> {
    let error = result.as_ref().err().map(ErrorReply::from);
    let outcome = match (result, &error) {
        (Ok(reply), _) => Ok(reply),
        (Err(_), Some(error)) => Err(error),
        (Err(_), None) => unreachable!("a failure has its error reply"),
    };
    let at = archon_core::now().max(ticket.record.started_at);
    let written = ticket
        .ctx
        .store
        .lock()
        .await
        .finish_tracked(&ticket.record, outcome, at);
    let Err(failure) = written else {
        return Ok(());
    };
    let status = ended(&ticket.record, result, error, at);
    assert!(status.is_consistent(), "memory keeps a whole outcome");
    let id = ticket.record.id.clone();
    let entry = (ticket.record.caller.clone(), status);
    ticket.ctx.tracked.lock().insert(id.clone(), entry);
    ticket.ctx.tracked.unrecorded().push(ticket.record.clone());
    ticket.kept = true;
    tracing::error!(request = %id, %failure, "a tracked request's outcome was not recorded; memory keeps it until the store takes it");
    let did = match result {
        Ok(_) => "was applied",
        Err(_) => "failed",
    };
    Err(failure.with_detail(&format!(
        "the change itself {did}, as request_status {id} reports; do not repeat it"
    )))
}

/// Record the outcomes memory kept when the store refused them, forgetting
/// each the store now holds. One still refused stays, logged, for the next
/// pass.
pub(crate) async fn record_kept(ctx: &DaemonContext) {
    let records = ctx.tracked.unrecorded().clone();
    assert!(
        records.len() <= HELD_MAX,
        "kept outcomes count toward the bound"
    );
    let mut recorded = 0;
    for record in records {
        if let Err(error) = record_one(ctx, &record).await {
            tracing::warn!(request = %record.id, %error, "a kept tracked outcome is still not recorded; the next prune pass retries");
            continue;
        }
        ctx.tracked.lock().remove(&record.id);
        ctx.tracked.unrecorded().retain(|kept| kept.id != record.id);
        recorded += 1;
    }
    if recorded > 0 {
        tracing::info!(recorded, "recorded tracked outcomes the store had refused");
    }
}

/// Write one kept outcome, unless the store already shows it ended.
async fn record_one(ctx: &DaemonContext, record: &TrackedRequest) -> Result<(), ArchonError> {
    let kept = ctx.tracked.lock().get(&record.id).map(|(_, s)| s.clone());
    let status = kept.expect("memory keeps an unrecorded outcome until it is recorded");
    assert!(status.is_consistent(), "memory keeps a whole outcome");
    let at = status.finished_at.expect("a kept outcome has ended");
    let outcome = match (&status.reply, &status.error) {
        (Some(reply), None) => Ok(reply.as_ref()),
        (None, Some(error)) => Err(error),
        _ => unreachable!("a kept outcome is a reply or an error"),
    };
    let mut store = ctx.store.lock().await;
    let stored = store.tracked_status(&record.id)?;
    if stored.is_some_and(|(_, stored)| stored.state != TrackedState::Pending) {
        return Ok(());
    }
    store.finish_tracked(record, outcome, at)
}

/// The status of the ticket's request, ended at `at` with `result`.
fn ended(
    record: &TrackedRequest,
    result: &Result<Reply, ArchonError>,
    error: Option<ErrorReply>,
    at: archon_core::Timestamp,
) -> RequestStatus {
    assert!(at >= record.started_at, "a request ends after it starts");
    assert_eq!(
        result.is_err(),
        error.is_some(),
        "only a failure has an error"
    );
    let (state, reply) = match result {
        Ok(reply) => (TrackedState::Applied, Some(Box::new(reply.clone()))),
        Err(_) => (TrackedState::Failed, None),
    };
    RequestStatus {
        request: record.id.clone(),
        command: record.command.clone(),
        state,
        idempotency_key: record.idempotency_key.clone(),
        reply,
        error,
        started_at: record.started_at,
        finished_at: Some(at),
    }
}

/// `request_status`: a request of `caller`'s that is running, then one
/// recorded; any other caller's is not found.
pub(crate) async fn status(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    id: &RequestId,
) -> Result<Reply, ArchonError> {
    let caller = caller_key(caller);
    let running = ctx.tracked.lock().get(id).cloned();
    let found = match running {
        Some(entry) => Some(entry),
        None => ctx.read_store().await?.tracked_status(id)?,
    };
    match found {
        Some((owner, status)) if owner == caller => {
            assert_eq!(&status.request, id, "the status is the one asked for");
            Ok(Reply::RequestStatus(Box::new(status)))
        }
        _ => Err(ArchonError::NotFound(format!(
            "archond holds no request {id} of yours: it never arrived, was refused before it \
             started, finished over 24 h ago, or archond restarted while it ran without an \
             idempotency_key. Check with get_item or list_queue before making the call again."
        ))),
    }
}

/// At start: what a stopped archond left pending is interrupted.
pub(crate) fn interrupt_left_pending(store: &mut archon_queue::Store) -> Result<(), ArchonError> {
    let marked = store.interrupt_tracked(archon_core::now())?;
    if marked > 0 {
        tracing::warn!(
            marked,
            "tracked requests were still running when archond stopped"
        );
    }
    Ok(())
}

/// How a caller is kept: its agent id, or empty for none.
fn caller_key(caller: Option<&AetherAgentId>) -> String {
    caller.map(|c| c.as_str().to_owned()).unwrap_or_default()
}
