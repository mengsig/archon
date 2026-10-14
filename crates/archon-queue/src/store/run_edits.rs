//! Recorded changes to a run's shape (ARCHITECTURE §6.6). A steer appends
//! them in order; whoever rebuilds the run's driver replays them in that
//! order, so a restart sees the pipeline the steer left.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::steer::RunEdit;
use rusqlite::{Connection, params};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::runs;

/// One edit as recorded: its place in the run's order and when it was made.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedEdit {
    /// 1-based and gapless within the run.
    pub seq: u32,
    pub at: Timestamp,
    pub edit: RunEdit,
}

/// Append `edit` to a live run and return its sequence number. A finished
/// run keeps the shape it ended with.
pub(super) fn append(
    conn: &mut Connection,
    run: &PipelineRunId,
    edit: &RunEdit,
    at: Timestamp,
) -> Result<u32, ArchonError> {
    let tx = write_tx(conn)?;
    if runs::load(&tx, run)?.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "run {run} already finished; its shape cannot change"
        )));
    }
    let last: u32 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM run_edits WHERE run = ?1",
            [run.as_str()],
            |row| row.get(0),
        )
        .ctx("read last run edit")?;
    let seq = last + 1;
    tx.execute(
        "INSERT INTO run_edits (run, seq, at, edit) VALUES (?1, ?2, ?3, ?4)",
        params![run.as_str(), seq, ts(&at), to_json(edit, "run edit")?],
    )
    .ctx("append run edit")?;
    tx.commit().ctx("commit run edit")?;
    assert!(seq >= 1, "run edits are numbered from one");
    Ok(seq)
}

/// The run's edits in the order they were made.
pub(super) fn for_run(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<Vec<RecordedEdit>, ArchonError> {
    let mut stmt = conn
        .prepare_cached("SELECT seq, at, edit FROM run_edits WHERE run = ?1 ORDER BY seq")
        .ctx("list run edits")?;
    let rows = stmt
        .query_map([run.as_str()], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .ctx("list run edits")?;
    let mut edits: Vec<RecordedEdit> = Vec::new();
    for row in rows {
        let (seq, at, raw) = row.ctx("read run edit")?;
        let at = chrono::DateTime::parse_from_rfc3339(&at)
            .map_err(|e| {
                ArchonError::Store(format!("run edit {seq} of {run} has a bad time: {e}"))
            })?
            .with_timezone(&chrono::Utc);
        let edit = from_json(&raw, "run edit")?;
        assert_eq!(seq as usize, edits.len() + 1, "run edits are gapless");
        edits.push(RecordedEdit { seq, at, edit });
    }
    Ok(edits)
}
