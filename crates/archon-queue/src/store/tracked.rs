//! Tracked requests (`archon_core::tracked`): the changes `archon mcp`
//! forwards under a request id, with their outcome. A keyed request is
//! claimed before it runs, so a repeat finds it even while it is pending;
//! an unkeyed one is written once, when it ends.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::RequestId;
use archon_core::protocol::{ErrorReply, Reply};
use archon_core::tracked::{RequestStatus, TrackedState};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};

/// One tracked request as archond received it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedRequest {
    pub id: RequestId,
    /// The Aether agent it was forwarded for; empty for none.
    pub caller: String,
    /// The wire name of the change (`cmd`).
    pub command: String,
    pub idempotency_key: Option<String>,
    /// The change as canonical JSON; a repeat under the key must match it.
    pub request: String,
    pub started_at: Timestamp,
}

/// What claiming a keyed request found.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// Nothing held the key: the request is recorded pending and may run.
    New,
    /// The key was used before: that request, as canonical JSON, and its status.
    Earlier {
        request: String,
        status: RequestStatus,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Reply(Box<Reply>),
    Error(ErrorReply),
}

const COLUMNS: &str =
    "id, command, idempotency_key, state, outcome, started_at, finished_at, request, caller";

/// Record a keyed `tracked` request as pending, unless its key was used
/// before by the same caller for the same command.
pub(super) fn claim(conn: &mut Connection, tracked: &TrackedRequest) -> Result<Claim, ArchonError> {
    let Some(key) = tracked.idempotency_key.as_deref() else {
        panic!("only a keyed request is claimed before it runs");
    };
    assert!(!tracked.request.is_empty(), "the request is recorded whole");
    let tx = write_tx(conn)?;
    let earlier = tx
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM tracked_requests
                 WHERE caller = ?1 AND command = ?2 AND idempotency_key = ?3"
            ),
            params![tracked.caller, tracked.command, key],
            decode,
        )
        .optional()
        .ctx("look up a tracked request's key")?;
    if let Some(row) = earlier {
        let (status, request, _) = row?;
        return Ok(Claim::Earlier { request, status });
    }
    insert(&tx, tracked, TrackedState::Pending, None, None)?;
    tx.commit().ctx("commit a tracked request's claim")?;
    Ok(Claim::New)
}

/// Record how `tracked` ended: the claimed row of a keyed request moves on
/// from pending, and an unkeyed request is written whole.
pub(super) fn finish(
    conn: &mut Connection,
    tracked: &TrackedRequest,
    result: Result<&Reply, &ErrorReply>,
    at: Timestamp,
) -> Result<(), ArchonError> {
    assert!(at >= tracked.started_at, "a request ends after it starts");
    let (state, outcome) = match result {
        Ok(reply) => (
            TrackedState::Applied,
            Outcome::Reply(Box::new(reply.clone())),
        ),
        Err(error) => (TrackedState::Failed, Outcome::Error(error.clone())),
    };
    let outcome = to_json(&outcome, "tracked request outcome")?;
    let tx = write_tx(conn)?;
    let changed = match tracked.idempotency_key {
        Some(_) => tx
            .execute(
                "UPDATE tracked_requests SET state = ?2, outcome = ?3, finished_at = ?4
                 WHERE id = ?1 AND state = 'pending'",
                params![tracked.id.as_str(), state.as_str(), outcome, ts(&at)],
            )
            .ctx("record a tracked request's outcome")?,
        None => insert(&tx, tracked, state, Some(&outcome), Some(at))?,
    };
    if changed != 1 {
        return Err(ArchonError::Store(format!(
            "tracked request {} was not pending, so its outcome was not recorded",
            tracked.id
        )));
    }
    tx.commit().ctx("commit a tracked request's outcome")
}

fn insert(
    conn: &Connection,
    tracked: &TrackedRequest,
    state: TrackedState,
    outcome: Option<&str>,
    finished_at: Option<Timestamp>,
) -> Result<usize, ArchonError> {
    assert_eq!(state == TrackedState::Pending, finished_at.is_none());
    conn.execute(
        "INSERT INTO tracked_requests
             (id, caller, command, idempotency_key, request, state, outcome, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            tracked.id.as_str(),
            tracked.caller,
            tracked.command,
            tracked.idempotency_key,
            tracked.request,
            state.as_str(),
            outcome,
            ts(&tracked.started_at),
            finished_at.as_ref().map(ts),
        ],
    )
    .ctx("record a tracked request")
}

/// The status of request `id` and the caller it was made for.
pub(super) fn status(
    conn: &Connection,
    id: &RequestId,
) -> Result<Option<(String, RequestStatus)>, ArchonError> {
    let row = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM tracked_requests WHERE id = ?1"),
            [id.as_str()],
            decode,
        )
        .optional()
        .ctx("read a tracked request")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (status, _, caller) = row?;
    assert_eq!(&status.request, id, "the row read is the one asked for");
    Ok(Some((caller, status)))
}

/// Mark every request still pending as interrupted: archond stopped while it
/// ran. Returns how many there were.
pub(super) fn interrupt_pending(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<usize, ArchonError> {
    let tx = write_tx(conn)?;
    let marked = tx
        .execute(
            "UPDATE tracked_requests SET state = 'interrupted', finished_at = ?1
             WHERE state = 'pending'",
            [ts(&at)],
        )
        .ctx("mark interrupted tracked requests")?;
    tx.commit().ctx("commit interrupted tracked requests")?;
    Ok(marked)
}

/// Delete the finished requests that started before `before`; returns how many.
pub(super) fn prune(conn: &mut Connection, before: Timestamp) -> Result<usize, ArchonError> {
    let tx = write_tx(conn)?;
    let deleted = tx
        .execute(
            "DELETE FROM tracked_requests WHERE started_at < ?1 AND state <> 'pending'",
            [ts(&before)],
        )
        .ctx("prune tracked requests")?;
    tx.commit().ctx("commit pruned tracked requests")?;
    Ok(deleted)
}

type Decoded = Result<(RequestStatus, String, String), ArchonError>;

/// A row of [`COLUMNS`]: its status, canonical request and caller. Errors
/// of the data itself stay inside, as corrupt storage.
fn decode(row: &Row<'_>) -> rusqlite::Result<Decoded> {
    let id: String = row.get(0)?;
    let command: String = row.get(1)?;
    let idempotency_key: Option<String> = row.get(2)?;
    let state: String = row.get(3)?;
    let outcome: Option<String> = row.get(4)?;
    let started_at: String = row.get(5)?;
    let finished_at: Option<String> = row.get(6)?;
    let request: String = row.get(7)?;
    let caller: String = row.get(8)?;
    let parts = (
        id,
        command,
        idempotency_key,
        state,
        outcome,
        started_at,
        finished_at,
    );
    Ok(status_of(parts).map(|status| (status, request, caller)))
}

type Parts = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    Option<String>,
);

fn status_of(parts: Parts) -> Result<RequestStatus, ArchonError> {
    let (id, command, idempotency_key, state, outcome, started_at, finished_at) = parts;
    let corrupt = |what: &str| ArchonError::Corrupt(format!("tracked request {id}: {what}"));
    let request = RequestId::parse(&id).map_err(|e| corrupt(&e.to_string()))?;
    let state = TrackedState::parse(&state)?;
    let outcome: Option<Outcome> = outcome
        .map(|raw| from_json(&raw, "tracked request outcome"))
        .transpose()?;
    let (reply, error) = match outcome {
        Some(Outcome::Reply(reply)) => (Some(reply), None),
        Some(Outcome::Error(error)) => (None, Some(error)),
        None => (None, None),
    };
    let time = |raw: &str| {
        chrono::DateTime::parse_from_rfc3339(raw)
            .map(|t| t.with_timezone(&chrono::Utc))
            .map_err(|e| corrupt(&format!("time {raw}: {e}")))
    };
    let status = RequestStatus {
        request,
        command,
        state,
        idempotency_key,
        reply,
        error,
        started_at: time(&started_at)?,
        finished_at: finished_at.as_deref().map(time).transpose()?,
    };
    if !status.is_consistent() {
        return Err(corrupt("its outcome does not match its state"));
    }
    Ok(status)
}
