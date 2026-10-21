//! Operator gates (ARCHITECTURE §18.3): one row per gated stage instance,
//! open until the operator decides, so a restart re-asks nothing.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QuestionId, QueueItemId, ReportId};
use archon_core::notify::{AnswerVia, FeedItem};
use archon_core::run::{OperatorGate, PipelineRun, RunState};
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::{feed, runs};

fn select(
    conn: &Connection,
    filter: &str,
    args: Vec<Value>,
) -> Result<Vec<OperatorGate>, ArchonError> {
    let sql =
        format!("SELECT data FROM gates WHERE {filter} ORDER BY opened_at, run, stage_id, attempt");
    let mut stmt = conn.prepare_cached(&sql).ctx("list gates")?;
    let rows = stmt
        .query_map(params_from_iter(args), |row| row.get::<_, String>(0))
        .ctx("list gates")?;
    let mut out = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx("read gate")?, "operator gate")?);
    }
    Ok(out)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

pub(super) fn find(
    conn: &Connection,
    run: &PipelineRunId,
    stage_id: &str,
    attempt: u32,
) -> Result<Option<OperatorGate>, ArchonError> {
    let args = vec![
        text(run.as_str()),
        text(stage_id),
        Value::Integer(attempt.into()),
    ];
    let mut found = select(conn, "run = ? AND stage_id = ? AND attempt = ?", args)?;
    assert!(found.len() <= 1, "the primary key makes a gate unique");
    Ok(found.pop())
}

pub(super) fn for_run(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<Vec<OperatorGate>, ArchonError> {
    select(conn, "run = ?", vec![text(run.as_str())])
}

pub(super) fn for_item(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Vec<OperatorGate>, ArchonError> {
    select(conn, "item = ?", vec![text(item.as_str())])
}

pub(super) fn open_all(conn: &Connection) -> Result<Vec<OperatorGate>, ArchonError> {
    let gates = select(conn, "open = 1", Vec::new())?;
    debug_assert!(gates.iter().all(OperatorGate::is_open));
    Ok(gates)
}

pub(super) fn by_question(
    conn: &Connection,
    question: &QuestionId,
) -> Result<Option<OperatorGate>, ArchonError> {
    let mut found = select(conn, "question = ?", vec![text(question.as_str())])?;
    assert!(found.len() <= 1, "a question belongs to one gate");
    Ok(found.pop())
}

/// A report file is republished by every revision, so only the open gate
/// counts.
pub(super) fn open_by_report(
    conn: &Connection,
    report: &ReportId,
) -> Result<Option<OperatorGate>, ArchonError> {
    let mut found = select(conn, "open = 1 AND report = ?", vec![text(report.as_str())])?;
    assert!(found.len() <= 1, "one open gate per report");
    Ok(found.pop())
}

fn check_shape(gate: &OperatorGate) -> Result<(), ArchonError> {
    if gate.stage_id.trim().is_empty() || gate.attempt == 0 {
        return Err(ArchonError::Invalid(format!(
            "gate of run {} needs a stage id and an attempt from 1",
            gate.run
        )));
    }
    if gate.decision.is_some() && gate.withdrawn.is_some() {
        return Err(ArchonError::Invalid(format!(
            "gate {}#{} of run {} cannot be both decided and withdrawn",
            gate.stage_id, gate.attempt, gate.run
        )));
    }
    if gate.is_open() == gate.decided_at.is_some() {
        return Err(ArchonError::Invalid(format!(
            "gate {}#{} of run {} must have decided_at exactly when closed",
            gate.stage_id, gate.attempt, gate.run
        )));
    }
    Ok(())
}

fn write(conn: &Connection, gate: &OperatorGate) -> Result<(), ArchonError> {
    debug_assert!(check_shape(gate).is_ok(), "callers check the shape first");
    let written = conn.execute(
        "INSERT INTO gates (run, stage_id, attempt, item, open, question, report, opened_at, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (run, stage_id, attempt) DO UPDATE SET
             open = excluded.open, question = excluded.question,
             report = excluded.report, data = excluded.data",
        params![
            gate.run.as_str(),
            gate.stage_id,
            gate.attempt,
            gate.item.as_str(),
            gate.is_open(),
            gate.question.as_ref().map(|q| q.as_str()),
            gate.report.as_ref().map(|r| r.as_str()),
            ts(&gate.opened_at),
            to_json(gate, "operator gate")?,
        ],
    )
    .ctx("write gate")?;
    assert_eq!(written, 1, "an upsert writes exactly one gate row");
    Ok(())
}

/// Open `gate` on its live run, or return the gate already recorded for
/// the same stage instance (a replayed wait after a restart).
pub(super) fn open(
    conn: &mut Connection,
    gate: &OperatorGate,
) -> Result<OperatorGate, ArchonError> {
    check_shape(gate)?;
    if !gate.is_open() {
        return Err(ArchonError::Invalid("a new gate must be open".into()));
    }
    let tx = write_tx(conn)?;
    if let Some(stored) = find(&tx, &gate.run, &gate.stage_id, gate.attempt)? {
        assert_eq!(stored.item, gate.item, "a run belongs to one item");
        return Ok(stored);
    }
    let run = runs::load(&tx, &gate.run)?;
    if run.item != gate.item || run.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "gate {}#{} needs the live run {} of item {}",
            gate.stage_id, gate.attempt, gate.run, gate.item
        )));
    }
    write(&tx, gate)?;
    tx.commit().ctx("commit gate")?;
    Ok(gate.clone())
}

/// Identity and subject are fixed, and a decision or withdrawal is final.
pub(super) fn update(conn: &mut Connection, gate: &OperatorGate) -> Result<(), ArchonError> {
    check_shape(gate)?;
    let tx = write_tx(conn)?;
    let stored = find(&tx, &gate.run, &gate.stage_id, gate.attempt)?.ok_or_else(|| {
        ArchonError::NotFound(format!(
            "gate {}#{} of run {}",
            gate.stage_id, gate.attempt, gate.run
        ))
    })?;
    if stored.item != gate.item
        || stored.subject != gate.subject
        || stored.opened_at != gate.opened_at
    {
        return Err(ArchonError::Invalid(format!(
            "the item, subject and opening time of gate {}#{} cannot change",
            gate.stage_id, gate.attempt
        )));
    }
    let outcome_changed = stored.decision != gate.decision || stored.withdrawn != gate.withdrawn;
    if !stored.is_open() && outcome_changed {
        return Err(ArchonError::InvalidTransition(format!(
            "gate {}#{} of run {} was already closed",
            gate.stage_id, gate.attempt, gate.run
        )));
    }
    write(&tx, gate)?;
    if matches!(
        gate.decision,
        Some(
            archon_core::run::GateDecision::Reject { .. }
                | archon_core::run::GateDecision::Revise { .. }
        )
    ) {
        super::outcomes::correct(
            &tx,
            &gate.run,
            archon_core::outcome::Correction::GateRejected,
        )?;
    }
    tx.commit().ctx("commit gate update")
}

/// Close the open gates of `run`, which is finishing in transaction `tx`,
/// so no answer can act on a run that ended, and close their questions.
/// Returns the feed answers that closed them.
pub(super) fn withdraw_open(
    tx: &Connection,
    run: &PipelineRun,
    at: Timestamp,
) -> Result<Vec<FeedItem>, ArchonError> {
    let reason = match &run.state {
        RunState::Cancelled { .. } => "the run was cancelled",
        RunState::Failed { .. } => "the run failed",
        RunState::Succeeded { .. } => "the run succeeded",
        RunState::Running { .. } | RunState::AwaitingOperator { .. } => {
            unreachable!("gates are withdrawn only when a run finishes")
        }
    };
    let mut answers = Vec::new();
    for mut gate in for_run(tx, &run.id)?
        .into_iter()
        .filter(OperatorGate::is_open)
    {
        gate.withdrawn = Some(reason.to_owned());
        gate.decided_at = Some(at.max(gate.opened_at));
        check_shape(&gate)?;
        write(tx, &gate)?;
        if let Some(question) = &gate.question {
            answers.extend(feed::close_withdrawn(tx, question, reason, at)?);
        }
    }
    debug_assert!(for_run(tx, &run.id)?.iter().all(|g| !g.is_open()));
    Ok(answers)
}

/// Crash recovery for stores written before gate questions closed with
/// their run: close every open question of a withdrawn gate. Returns the
/// feed answers that closed them.
pub(super) fn close_withdrawn_questions(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<Vec<FeedItem>, ArchonError> {
    let tx = write_tx(conn)?;
    let stale = select(
        &tx,
        "open = 0 AND question IN (SELECT id FROM questions WHERE open = 1)",
        Vec::new(),
    )?;
    let mut answers = Vec::new();
    for gate in stale {
        let (Some(reason), Some(question)) = (&gate.withdrawn, &gate.question) else {
            continue;
        };
        let closed = feed::close_withdrawn(&tx, question, reason, at)?;
        assert!(closed.is_some(), "the question was selected as open");
        answers.extend(closed);
    }
    tx.commit().ctx("commit withdrawn questions")?;
    Ok(answers)
}

/// Close every open question of a gate that was decided: the decision is
/// the answer, whoever recorded it and whether or not the caller that
/// persisted it went on to answer the question. Returns the feed answers.
pub(super) fn close_decided_questions(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<Vec<FeedItem>, ArchonError> {
    let tx = write_tx(conn)?;
    let decided = select(
        &tx,
        "open = 0 AND question IN (SELECT id FROM questions WHERE open = 1)",
        Vec::new(),
    )?;
    let mut answers = Vec::new();
    for gate in decided {
        let (Some(decision), Some(question), None) =
            (&gate.decision, &gate.question, &gate.withdrawn)
        else {
            continue;
        };
        let text = format!("answered: the gate was {}", decision.word());
        let closed = feed::settle_in(&tx, question, &text, AnswerVia::Archon, at)?;
        assert!(closed.is_some(), "the question was selected as open");
        answers.extend(closed);
    }
    tx.commit().ctx("commit decided gate questions")?;
    Ok(answers)
}
