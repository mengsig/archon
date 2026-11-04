//! The change stream (CONTRACTS.md §1.3, V19): one `changes` row per state
//! change, inserted by the writer of that state inside its own transaction.
//! `seq` is `AUTOINCREMENT`: a rolled-back write takes its row and its seq
//! back with it, and writes are serialized, so committed seqs never skip.
//! Item transitions are kept beside them in `item_transitions`.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::initiative::Initiative;
use archon_core::notify::Question;
use archon_core::protocol::{
    Change, ChangeSubject, HistoryRange, ItemTransition, QueueStateKind, RunStateKind,
    StageStateKind, TransitionActor,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{OperatorGate, PipelineRun, RunState, StageRun};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{
    SqlContext, end_read_tx, from_json, read_tx, sql_limit, to_i64, to_json, to_u64, ts, write_tx,
};

/// Changes are kept for at least this many days (CONTRACTS.md §1.3)...
pub const KEEP_DAYS: i64 = 30;
/// ...or as the newest this many rows, whichever keeps more.
pub const KEEP_ROWS: u64 = 100_000;
/// A transition's reason is cut to this many characters.
const REASON_CHARS: usize = 500;

/// Append `subject` to the stream inside the caller's write transaction.
fn record(conn: &Connection, subject: &ChangeSubject) -> Result<u64, ArchonError> {
    assert!(
        !conn.is_autocommit(),
        "a change is written in the transaction of the state it describes"
    );
    conn.execute(
        "INSERT INTO changes (at, data) VALUES (?1, ?2)",
        params![ts(&archon_core::now()), to_json(subject, "change")?],
    )
    .ctx("record change")?;
    let seq = to_u64(conn.last_insert_rowid(), "change seq")?;
    assert!(seq > 0, "AUTOINCREMENT seqs start at 1");
    Ok(seq)
}

/// What a transition into `state` says about why, if anything.
fn transition_reason(state: &QueueState) -> Option<String> {
    let text = match state {
        QueueState::Blocked { reason } => reason.as_str(),
        QueueState::Failed { error, .. } => error.as_str(),
        QueueState::Cancelled { reason } => reason.as_deref()?,
        _ => return None,
    };
    Some(text.chars().take(REASON_CHARS).collect())
}

/// An item created (`before` is `None`) or moved to another state kind:
/// one change and one transition. A write that keeps the kind records none.
pub(super) fn record_item(
    conn: &Connection,
    item: &QueueItem,
    before: Option<&QueueState>,
) -> Result<(), ArchonError> {
    let to = QueueStateKind::from(&item.state);
    let from = before.map(QueueStateKind::from);
    if from == Some(to) {
        return Ok(());
    }
    assert!(
        before.is_some() || item.state == QueueState::Queued,
        "an item is created queued"
    );
    let subject = ChangeSubject::Item {
        item: item.id.clone(),
        state: to,
        stage: None,
        waiting_reason: None,
    };
    let seq = record(conn, &subject)?;
    conn.execute(
        "INSERT INTO item_transitions (seq, item, at, from_state, to_state, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            to_i64(seq, "change seq")?,
            item.id.as_str(),
            ts(&item.updated_at),
            before.map(QueueState::kind),
            item.state.kind(),
            transition_reason(&item.state),
            actor_name(TransitionActor::Daemon),
        ],
    )
    .ctx("record item transition")?;
    Ok(())
}

/// One change per dependency added or removed between `before` and `after`.
pub(super) fn record_dependencies(
    conn: &Connection,
    item: &QueueItemId,
    before: &[QueueItemId],
    after: &[QueueItemId],
) -> Result<(), ArchonError> {
    let removed = before
        .iter()
        .filter(|d| !after.contains(d))
        .map(|d| (d, false));
    let added = after
        .iter()
        .filter(|d| !before.contains(d))
        .map(|d| (d, true));
    for (depends_on, added) in removed.chain(added) {
        assert_ne!(depends_on, item, "an item never depends on itself");
        let subject = ChangeSubject::Dependency {
            item: item.clone(),
            depends_on: depends_on.clone(),
            added,
        };
        record(conn, &subject)?;
    }
    Ok(())
}

/// A run started (`before` is `None`) or moved to another state kind.
pub(super) fn record_run(
    conn: &Connection,
    run: &PipelineRun,
    before: Option<&RunState>,
) -> Result<(), ArchonError> {
    let state = RunStateKind::from(&run.state);
    if before.map(RunStateKind::from) == Some(state) {
        return Ok(());
    }
    let subject = ChangeSubject::Run {
        run: run.id.clone(),
        item: run.item.clone(),
        state,
    };
    record(conn, &subject).map(drop)
}

/// A stage run started (`before` is `None`), moved to another state kind,
/// or got its verdict.
pub(super) fn record_stage(
    conn: &Connection,
    stage: &StageRun,
    before: Option<&StageRun>,
) -> Result<(), ArchonError> {
    let state = StageStateKind::from(&stage.state);
    if let Some(before) = before
        && StageStateKind::from(&before.state) == state
        && before.verdict == stage.verdict
    {
        return Ok(());
    }
    let subject = ChangeSubject::Stage {
        stage_run: stage.id.clone(),
        run: stage.run.clone(),
        item: run_item(conn, &stage.run)?,
        stage_id: stage.stage_id.clone(),
        attempt: stage.attempt,
        fan_index: stage.fan_index,
        state,
        verdict: stage.verdict,
    };
    record(conn, &subject).map(drop)
}

fn run_item(conn: &Connection, run: &PipelineRunId) -> Result<QueueItemId, ArchonError> {
    let item: Option<String> = conn
        .query_row(
            "SELECT item FROM runs WHERE id = ?1",
            [run.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up the item of a run")?;
    let item = item.ok_or_else(|| ArchonError::NotFound(format!("pipeline run {run}")))?;
    QueueItemId::parse(&item)
}

/// A gate opened or decided. A withdrawal is told by its run's change.
pub(super) fn record_gate(conn: &Connection, gate: &OperatorGate) -> Result<(), ArchonError> {
    assert!(
        gate.withdrawn.is_none(),
        "a withdrawn gate is told by its run finishing"
    );
    let subject = ChangeSubject::Gate {
        run: gate.run.clone(),
        item: gate.item.clone(),
        stage_id: gate.stage_id.clone(),
        decision: gate.decision.clone(),
    };
    record(conn, &subject).map(drop)
}

pub(super) fn record_question(conn: &Connection, question: &Question) -> Result<(), ArchonError> {
    let subject = ChangeSubject::Question {
        question: question.id.clone(),
        item: question.item.clone(),
        open: question.is_open(),
    };
    record(conn, &subject).map(drop)
}

pub(super) fn record_initiative(
    conn: &Connection,
    initiative: &Initiative,
) -> Result<(), ArchonError> {
    let subject = ChangeSubject::Initiative {
        initiative: initiative.id.clone(),
        state: initiative.state,
    };
    record(conn, &subject).map(drop)
}

fn actor_name(actor: TransitionActor) -> &'static str {
    match actor {
        TransitionActor::Daemon => "daemon",
        TransitionActor::Cli => "cli",
        TransitionActor::Web => "web",
        TransitionActor::Mcp => "mcp",
        TransitionActor::Github => "github",
    }
}

fn parse_actor(name: &str) -> Result<TransitionActor, ArchonError> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .map_err(|_| ArchonError::Corrupt(format!("item transition by {name:?}")))
}

fn parse_kind(name: &str) -> Result<QueueStateKind, ArchonError> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .map_err(|_| ArchonError::Corrupt(format!("item transition to state {name:?}")))
}

fn parse_at(raw: &str) -> Result<Timestamp, ArchonError> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| ArchonError::Corrupt(format!("change time {raw:?}: {e}")))
}

/// The seqs still kept: `first_seq` is `last_seq + 1` when none is.
pub(super) fn range(conn: &Connection) -> Result<HistoryRange, ArchonError> {
    let (first, last): (Option<i64>, Option<i64>) = conn
        .query_row("SELECT MIN(seq), MAX(seq) FROM changes", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .ctx("read the change range")?;
    let last = to_u64(last.unwrap_or(0), "change seq")?;
    let first = match first {
        Some(first) => to_u64(first, "change seq")?,
        None => last + 1,
    };
    assert!(
        first >= 1 && first <= last + 1,
        "kept seqs {first}..={last}"
    );
    Ok(HistoryRange {
        first_seq: first,
        last_seq: last,
    })
}

/// A cursor resumes the stream only if every change after it is kept; one
/// past the last seq is from another store.
fn check_cursor(after: u64, kept: HistoryRange) -> Result<(), ArchonError> {
    if after + 1 >= kept.first_seq && after <= kept.last_seq {
        return Ok(());
    }
    Err(ArchonError::HistoryExpired {
        message: format!(
            "change cursor {after} is outside the kept changes {}..={}",
            kept.first_seq, kept.last_seq
        ),
        kept,
    })
}

fn changes_after(conn: &Connection, after: u64, limit: usize) -> Result<Vec<Change>, ArchonError> {
    assert!(limit > 0, "a page holds a change");
    check_cursor(after, range(conn)?)?;
    let mut stmt = conn
        .prepare_cached("SELECT seq, at, data FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2")
        .ctx("list changes")?;
    let rows = stmt
        .query_map(
            params![to_i64(after, "change cursor")?, sql_limit(limit)],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .ctx("list changes")?;
    let mut changes: Vec<Change> = Vec::new();
    for row in rows {
        let (seq, at, data) = row.ctx("read change")?;
        let seq = to_u64(seq, "change seq")?;
        let expected = changes.last().map_or(after, |c| c.seq) + 1;
        if seq != expected {
            return Err(ArchonError::Corrupt(format!(
                "change {expected} is missing: the next kept change is {seq}"
            )));
        }
        changes.push(Change {
            seq,
            at: parse_at(&at)?,
            subject: from_json(&data, "change")?,
        });
    }
    debug_assert!(changes.len() <= limit);
    Ok(changes)
}

fn transitions(
    conn: &Connection,
    item: &QueueItemId,
    limit: usize,
) -> Result<Vec<ItemTransition>, ArchonError> {
    assert!(limit > 0, "a page holds a transition");
    super::items::load(conn, item)?;
    let mut stmt = conn
        .prepare_cached(
            "SELECT seq, at, from_state, to_state, reason, actor FROM item_transitions
             WHERE item = ?1 ORDER BY seq DESC LIMIT ?2",
        )
        .ctx("list item transitions")?;
    let rows = stmt
        .query_map(params![item.as_str(), sql_limit(limit)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .ctx("list item transitions")?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, at, from, to, reason, actor) = row.ctx("read item transition")?;
        out.push(ItemTransition {
            seq: to_u64(seq, "change seq")?,
            at: parse_at(&at)?,
            from: from.as_deref().map(parse_kind).transpose()?,
            to: parse_kind(&to)?,
            reason,
            by: parse_actor(&actor)?,
        });
    }
    out.reverse();
    debug_assert!(out.windows(2).all(|w| w[0].seq < w[1].seq), "oldest first");
    Ok(out)
}

/// The newest seq that retention may delete through at `now`: everything
/// older than [`KEEP_DAYS`] that is not among the newest [`KEEP_ROWS`].
/// Only a prefix is ever deleted, so the kept seqs stay contiguous.
fn prune_through(conn: &Connection, now: Timestamp, limit: usize) -> Result<u64, ArchonError> {
    let kept = range(conn)?;
    let Some(by_rows) = kept.last_seq.checked_sub(KEEP_ROWS) else {
        return Ok(0);
    };
    let cutoff = now - chrono::Duration::days(KEEP_DAYS);
    let first_recent: Option<i64> = conn
        .query_row(
            "SELECT seq FROM changes WHERE at >= ?1 ORDER BY seq LIMIT 1",
            [ts(&cutoff)],
            |row| row.get(0),
        )
        .optional()
        .ctx("find the oldest recent change")?;
    let by_age = match first_recent {
        Some(seq) => to_u64(seq, "change seq")? - 1,
        None => kept.last_seq,
    };
    let by_limit = kept.first_seq.saturating_add(limit as u64) - 1;
    Ok(by_rows.min(by_age).min(by_limit))
}

impl Store {
    /// The last change seq committed: what a snapshot read in the same
    /// transaction is `as_of`.
    pub fn last_change_seq(&self) -> Result<u64, ArchonError> {
        Ok(range(&self.conn)?.last_seq)
    }

    /// The change seqs still kept.
    pub fn change_range(&self) -> Result<HistoryRange, ArchonError> {
        range(&self.conn)
    }

    /// At most `limit` changes with `seq > after`, oldest first, read in
    /// one transaction. `HistoryExpired` when some are no longer kept.
    pub fn changes_after(&self, after: u64, limit: usize) -> Result<Vec<Change>, ArchonError> {
        let tx = read_tx(&self.conn)?;
        let changes = changes_after(&self.conn, after, limit)?;
        end_read_tx(tx, "end change read")?;
        Ok(changes)
    }

    /// The newest `limit` transitions of `item`, oldest first.
    pub fn item_history(
        &self,
        item: &QueueItemId,
        limit: usize,
    ) -> Result<Vec<ItemTransition>, ArchonError> {
        let tx = read_tx(&self.conn)?;
        let out = transitions(&self.conn, item, limit)?;
        end_read_tx(tx, "end item history read")?;
        Ok(out)
    }

    /// Deletes up to `limit` of the oldest changes retention no longer
    /// keeps at `now`; how many it deleted.
    pub fn prune_changes(&mut self, now: Timestamp, limit: usize) -> Result<u64, ArchonError> {
        assert!(limit > 0, "a prune deletes something");
        let tx = write_tx(&mut self.conn)?;
        let through = prune_through(&tx, now, limit)?;
        let deleted = tx
            .execute(
                "DELETE FROM changes WHERE seq <= ?1",
                [to_i64(through, "change seq")?],
            )
            .ctx("prune changes")?;
        let after = range(&tx)?;
        assert!(
            deleted == 0 || after.first_seq == through + 1,
            "retention deletes a prefix"
        );
        let left = after.last_seq + 1 - after.first_seq;
        assert!(
            left >= KEEP_ROWS.min(after.last_seq),
            "the newest rows stay"
        );
        tx.commit().ctx("commit change pruning")?;
        Ok(deleted as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_resumes_only_inside_the_kept_changes() {
        let kept = HistoryRange {
            first_seq: 5,
            last_seq: 9,
        };
        assert!(check_cursor(4, kept).is_ok());
        assert!(check_cursor(9, kept).is_ok());
        assert!(matches!(
            check_cursor(3, kept),
            Err(ArchonError::HistoryExpired { kept: k, .. }) if k == kept
        ));
        assert!(
            check_cursor(10, kept).is_err(),
            "a cursor from another store"
        );
        let empty = HistoryRange {
            first_seq: 1,
            last_seq: 0,
        };
        assert!(check_cursor(0, empty).is_ok());
    }

    #[test]
    fn actors_are_spelled_as_the_wire_spells_them() {
        for actor in [
            TransitionActor::Daemon,
            TransitionActor::Cli,
            TransitionActor::Web,
            TransitionActor::Mcp,
            TransitionActor::Github,
        ] {
            assert_eq!(parse_actor(actor_name(actor)).unwrap(), actor);
            assert_eq!(serde_json::to_value(actor).unwrap(), actor_name(actor));
        }
    }

    #[test]
    fn a_reason_is_cut_to_its_limit() {
        let long = "x".repeat(REASON_CHARS * 2);
        let reason = transition_reason(&QueueState::Blocked { reason: long }).unwrap();
        assert_eq!(reason.chars().count(), REASON_CHARS);
        assert_eq!(transition_reason(&QueueState::Queued), None);
    }
}
