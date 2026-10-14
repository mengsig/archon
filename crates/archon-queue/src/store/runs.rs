//! Pipeline runs and stage runs.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, StageRun, StageState};
use rusqlite::{Connection, OptionalExtension, params};

use super::FinishedRun;
use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::{gates, items};

fn read_rows<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    arg: &str,
    what: &str,
) -> Result<Vec<T>, ArchonError> {
    let mut stmt = conn.prepare_cached(sql).ctx(what)?;
    let rows = stmt
        .query_map([arg], |row| row.get::<_, String>(0))
        .ctx(what)?;
    let mut out = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx(what)?, what)?);
    }
    Ok(out)
}

pub(super) fn load(conn: &Connection, id: &PipelineRunId) -> Result<PipelineRun, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM runs WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read run")?;
    let data = data.ok_or_else(|| ArchonError::NotFound(format!("pipeline run {id}")))?;
    let run: PipelineRun = from_json(&data, "pipeline run")?;
    debug_assert_eq!(&run.id, id, "run row {id} holds another run");
    Ok(run)
}

/// The item's non-terminal run, if any (at most one by the unique index).
pub(super) fn active_run_of(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Option<PipelineRunId>, ArchonError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT id FROM runs WHERE item = ?1 AND terminal = 0",
            [item.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up active run")?;
    raw.map(|id| PipelineRunId::parse(&id)).transpose()
}

/// `finished_at` is set exactly when the run is terminal.
fn check_run_shape(run: &PipelineRun) -> Result<(), ArchonError> {
    if run.state.is_terminal() != run.finished_at.is_some() {
        return Err(ArchonError::Invalid(format!(
            "run {} must have finished_at exactly when it is terminal",
            run.id
        )));
    }
    if !run.cost_usd.is_finite() || run.cost_usd < 0.0 {
        return Err(ArchonError::Invalid(format!(
            "run {} has cost {}",
            run.id, run.cost_usd
        )));
    }
    Ok(())
}

fn write_run(conn: &Connection, run: &PipelineRun, insert: bool) -> Result<(), ArchonError> {
    let sql = if insert {
        "INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    } else {
        "UPDATE runs SET item = ?2, pipeline = ?3, terminal = ?4, started_at = ?5, data = ?6
         WHERE id = ?1"
    };
    let changed = conn
        .execute(
            sql,
            params![
                run.id.as_str(),
                run.item.as_str(),
                run.pipeline,
                run.state.is_terminal(),
                ts(&run.started_at),
                to_json(run, "pipeline run")?,
            ],
        )
        .ctx("write run")?;
    assert_eq!(changed, 1, "write of run {} touched {changed} rows", run.id);
    Ok(())
}

/// A new run starts live, for a `dispatching` item of the same pipeline
/// that has no other live run.
pub(super) fn insert(conn: &mut Connection, run: &PipelineRun) -> Result<(), ArchonError> {
    check_run_shape(run)?;
    if run.state.is_terminal() {
        return Err(ArchonError::Invalid(format!(
            "run {} cannot start finished",
            run.id
        )));
    }
    let tx = write_tx(conn)?;
    let item = items::load(&tx, &run.item)?;
    if item.state != QueueState::Dispatching {
        return Err(ArchonError::InvalidTransition(format!(
            "item {} is {}, a run starts only while it is dispatching",
            item.id,
            item.state.kind()
        )));
    }
    if item.pipeline != run.pipeline {
        return Err(ArchonError::Invalid(format!(
            "run {} names pipeline {:?} but item {} uses {:?}",
            run.id, run.pipeline, item.id, item.pipeline
        )));
    }
    if let Some(live) = active_run_of(&tx, &run.item)? {
        return Err(ArchonError::InvalidTransition(format!(
            "item {} already has the active run {live}",
            run.item
        )));
    }
    let taken: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE id = ?1)",
            [run.id.as_str()],
            |row| row.get(0),
        )
        .ctx("look up run")?;
    if taken {
        return Err(ArchonError::Invalid(format!(
            "run {} already exists",
            run.id
        )));
    }
    write_run(&tx, run, true)?;
    tx.commit().ctx("commit run")
}

fn check_run_change(stored: &PipelineRun, run: &PipelineRun) -> Result<(), ArchonError> {
    if stored.item != run.item
        || stored.pipeline != run.pipeline
        || stored.started_at != run.started_at
    {
        return Err(ArchonError::Invalid(format!(
            "the item, pipeline and start time of run {} cannot change",
            run.id
        )));
    }
    if stored.state.is_terminal() && stored.state != run.state {
        return Err(ArchonError::InvalidTransition(format!(
            "run {} already finished",
            run.id
        )));
    }
    Ok(())
}

/// A finished run never changes state again; the item, pipeline and start
/// time are fixed at creation. A live run finishes only through [`finish`].
pub(super) fn update(conn: &mut Connection, run: &PipelineRun) -> Result<(), ArchonError> {
    check_run_shape(run)?;
    let tx = write_tx(conn)?;
    let stored = load(&tx, &run.id)?;
    check_run_change(&stored, run)?;
    if !stored.state.is_terminal() && run.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "run {} must finish together with its item through finish_run",
            run.id
        )));
    }
    write_run(&tx, run, false)?;
    tx.commit().ctx("commit run")
}

/// Finishes a live run, withdraws its open gates, closes their questions and
/// settles its item in one transaction, so a crash can never leave a
/// `running` item without a live run, a live run under a settled item, or a
/// question open on a withdrawn gate. `next` must be terminal and name no
/// other run.
pub(super) fn finish(
    conn: &mut Connection,
    run: &PipelineRun,
    next: QueueState,
    at: Timestamp,
) -> Result<FinishedRun, ArchonError> {
    check_run_shape(run)?;
    if !run.state.is_terminal() || !next.is_terminal() {
        return Err(ArchonError::Invalid(format!(
            "finishing run {} needs a terminal run state and item state, not {}",
            run.id,
            next.kind()
        )));
    }
    let named = match &next {
        QueueState::Succeeded { run, .. } => Some(run),
        QueueState::Failed { run, .. } => run.as_ref(),
        _ => None,
    };
    if named.is_some_and(|named| named != &run.id) {
        return Err(ArchonError::Invalid(format!(
            "finishing run {} cannot settle its item with another run",
            run.id
        )));
    }
    let tx = write_tx(conn)?;
    let stored = load(&tx, &run.id)?;
    check_run_change(&stored, run)?;
    if stored.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "run {} already finished",
            run.id
        )));
    }
    write_run(&tx, run, false)?;
    let answers = gates::withdraw_open(&tx, run, at)?;
    let item = items::transition_in(&tx, &run.item, next, at)?;
    debug_assert!(active_run_of(&tx, &item.id)?.is_none());
    tx.commit().ctx("commit finished run")?;
    Ok(FinishedRun { item, answers })
}

pub(super) fn for_item(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Vec<PipelineRun>, ArchonError> {
    items::load(conn, item)?;
    read_rows(
        conn,
        "SELECT data FROM runs WHERE item = ?1 ORDER BY started_at, id",
        item.as_str(),
        "pipeline run",
    )
}

pub(super) fn active(conn: &Connection) -> Result<Vec<PipelineRun>, ArchonError> {
    let mut stmt = conn
        .prepare_cached("SELECT data FROM runs WHERE terminal = 0 ORDER BY started_at, id")
        .ctx("list active runs")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .ctx("list active runs")?;
    let mut out: Vec<PipelineRun> = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx("read run")?, "pipeline run")?);
    }
    debug_assert!(out.iter().all(|r| !r.state.is_terminal()));
    Ok(out)
}

/// `spawning` < `running` < terminal; a state never moves backwards and a
/// terminal state never changes.
fn stage_rank(state: &StageState) -> u8 {
    match state {
        StageState::Spawning => 0,
        StageState::Running => 1,
        _ => 2,
    }
}

fn stage_kind(state: &StageState) -> &'static str {
    match state {
        StageState::Spawning => "spawning",
        StageState::Running => "running",
        StageState::Completed => "completed",
        StageState::Failed { .. } => "failed",
        StageState::Cancelled => "cancelled",
        StageState::TimedOut => "timed_out",
    }
}

fn check_stage_shape(stage: &StageRun) -> Result<(), ArchonError> {
    if stage.attempt == 0 {
        return Err(ArchonError::Invalid(format!(
            "stage run {} has attempt 0",
            stage.id
        )));
    }
    if stage.state.is_terminal() != stage.finished_at.is_some() {
        return Err(ArchonError::Invalid(format!(
            "stage run {} must have finished_at exactly when it is terminal",
            stage.id
        )));
    }
    if stage.state == StageState::Running && stage.aether_agent.is_none() {
        return Err(ArchonError::Invalid(format!(
            "stage run {} is running without an Aether agent",
            stage.id
        )));
    }
    Ok(())
}

fn check_stage_change(stored: &StageRun, next: &StageRun) -> Result<(), ArchonError> {
    let same_identity = stored.run == next.run
        && stored.stage_id == next.stage_id
        && stored.attempt == next.attempt
        && stored.fan_index == next.fan_index
        && stored.choice == next.choice
        && stored.started_at == next.started_at;
    if !same_identity {
        return Err(ArchonError::Invalid(format!(
            "stage run {} cannot change its run, stage, attempt, slot, model or start",
            stored.id
        )));
    }
    let agent_kept = stored.aether_agent.is_none() || stored.aether_agent == next.aether_agent;
    let task_kept = stored.aether_task.is_none() || stored.aether_task == next.aether_task;
    if !agent_kept || !task_kept {
        return Err(ArchonError::Invalid(format!(
            "stage run {} cannot replace its Aether agent or task",
            stored.id
        )));
    }
    let terminal_changed = stored.state.is_terminal() && stored.state != next.state;
    if terminal_changed || stage_rank(&next.state) < stage_rank(&stored.state) {
        return Err(ArchonError::InvalidTransition(format!(
            "stage run {} cannot go from {:?} to {:?}",
            stored.id, stored.state, next.state
        )));
    }
    Ok(())
}

fn load_stage(conn: &Connection, id: &StageRunId) -> Result<Option<StageRun>, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM stage_runs WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read stage run")?;
    data.map(|d| from_json(&d, "stage run")).transpose()
}

/// `holds_agent` says whether this row takes the unique `aether_agent`
/// column; `data` names the stage run's agent either way.
fn write_stage(conn: &Connection, stage: &StageRun, holds_agent: bool) -> Result<(), ArchonError> {
    assert!(
        !holds_agent || stage.aether_agent.is_some(),
        "only a stage run with an agent can hold one"
    );
    let agent = stage.aether_agent.as_ref().filter(|_| holds_agent);
    conn.execute(
        "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, aether_agent, state,
                                 terminal, started_at, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (id) DO UPDATE SET aether_agent = excluded.aether_agent,
             state = excluded.state, terminal = excluded.terminal, data = excluded.data",
        params![
            stage.id.as_str(),
            stage.run.as_str(),
            stage.stage_id,
            stage.attempt,
            stage.fan_index,
            agent.map(AetherAgentId::as_str),
            stage_kind(&stage.state),
            stage.state.is_terminal(),
            ts(&stage.started_at),
            to_json(stage, "stage run")?,
        ],
    )
    .ctx("write stage run")?;
    Ok(())
}

/// Whether `stage` holds its agent in the unique `aether_agent` column: a kept agent (§6.7)
/// passes to the next attempt of its slot, named in `data`, with its [`super::handoffs`].
fn claim_agent(conn: &Connection, stage: &StageRun) -> Result<bool, ArchonError> {
    let Some(agent) = &stage.aether_agent else {
        return Ok(false);
    };
    if let Some(run) = super::spend::agent_run(conn, agent)?
        && run != stage.run
    {
        return Err(ArchonError::Invalid(format!(
            "Aether agent {agent} already belongs to run {run}"
        )));
    }
    let holder: Option<String> = conn
        .query_row(
            "SELECT data FROM stage_runs WHERE aether_agent = ?1 AND id <> ?2",
            params![agent.as_str(), stage.id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up agent owner")?;
    let Some(holder) = holder else {
        return Ok(true);
    };
    let holder: StageRun = from_json(&holder, "stage run")?;
    let same_slot = holder.run == stage.run
        && holder.stage_id == stage.stage_id
        && holder.fan_index == stage.fan_index;
    if same_slot && holder.attempt > stage.attempt {
        return Ok(false);
    }
    if !same_slot || holder.attempt == stage.attempt || !holder.state.is_terminal() {
        return Err(ArchonError::Invalid(format!(
            "Aether agent {agent} already belongs to stage run {}",
            holder.id
        )));
    }
    assert!(
        holder.attempt < stage.attempt,
        "a kept agent passes to a later attempt only"
    );
    super::handoffs::record(conn, agent, stage)?;
    let released = conn
        .execute(
            "UPDATE stage_runs SET aether_agent = NULL WHERE id = ?1",
            [holder.id.as_str()],
        )
        .ctx("hand a kept agent to its next attempt")?;
    assert_eq!(released, 1, "the holder was read in this transaction");
    Ok(true)
}

/// Inserts a new stage run into a live pipeline run, or applies a forward
/// change to an existing one.
pub(super) fn upsert_stage(conn: &mut Connection, stage: &StageRun) -> Result<(), ArchonError> {
    check_stage_shape(stage)?;
    let tx = write_tx(conn)?;
    let mut stage = stage.clone();
    match load_stage(&tx, &stage.id)? {
        Some(stored) => {
            stage.model_segments = stored.model_segments.clone();
            check_stage_change(&stored, &stage)?;
        }
        None => {
            if load(&tx, &stage.run)?.state.is_terminal() {
                return Err(ArchonError::InvalidTransition(format!(
                    "run {} already finished; stage run {} cannot start",
                    stage.run, stage.id
                )));
            }
        }
    }
    let holds_agent = claim_agent(&tx, &stage)?;
    write_stage(&tx, &stage, holds_agent)?;
    tx.commit().ctx("commit stage run")
}

/// Stage runs still `spawning` or `running`, under any run: a cancelled
/// run's agents occupy capacity until they settle. Oldest first.
pub(super) fn live_stages(conn: &Connection) -> Result<Vec<StageRun>, ArchonError> {
    let mut stmt = conn
        .prepare_cached("SELECT data FROM stage_runs WHERE terminal = 0 ORDER BY started_at, id")
        .ctx("list live stage runs")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .ctx("list live stage runs")?;
    let mut out: Vec<StageRun> = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx("read stage run")?, "stage run")?);
    }
    debug_assert!(out.iter().all(|s| !s.state.is_terminal()));
    Ok(out)
}

pub(super) fn stages_of(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<Vec<StageRun>, ArchonError> {
    load(conn, run)?;
    read_rows(
        conn,
        "SELECT data FROM stage_runs WHERE run = ?1
         ORDER BY started_at, attempt, fan_index, id",
        run.as_str(),
        "stage run",
    )
}

/// Every stage run of `item`'s runs: the runs in [`for_item`]'s order,
/// each run's stage runs in [`stages_of`]'s.
pub(super) fn stages_of_item(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Vec<StageRun>, ArchonError> {
    items::load(conn, item)?;
    read_rows(
        conn,
        "SELECT s.data FROM stage_runs s JOIN runs r ON r.id = s.run
         WHERE r.item = ?1
         ORDER BY r.started_at, r.id, s.started_at, s.attempt, s.fan_index, s.id",
        item.as_str(),
        "stage run",
    )
}

pub(super) fn stage_by_agent(
    conn: &Connection,
    agent: &AetherAgentId,
) -> Result<Option<StageRun>, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM stage_runs WHERE aether_agent = ?1",
            [agent.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up stage run by agent")?;
    data.map(|d| from_json(&d, "stage run")).transpose()
}

pub(super) fn stage(conn: &Connection, id: &StageRunId) -> Result<StageRun, ArchonError> {
    load_stage(conn, id)?.ok_or_else(|| ArchonError::NotFound(format!("stage run {id}")))
}
