//! Merges the operator asked for (ARCHITECTURE §18.6): one row per request,
//! written before any GitHub write, and at most one open per item.

use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::merge::{MergeAuthority, MergeRequest, MergeState};
use archon_core::notify::{AnswerVia, FeedEntry, FeedItem, Question};
use rusqlite::{Connection, OptionalExtension, params};

use super::codec::{SqlContext, from_json, to_json, ts, write_tx};
use super::feed;

fn question_of(merge: &MergeRequest) -> Option<&QuestionId> {
    match &merge.authority {
        MergeAuthority::Confirmed { question, .. } => Some(question),
        _ => None,
    }
}

fn load(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<MergeRequest>, ArchonError> {
    let mut stmt = conn.prepare_cached(sql).ctx("list merge requests")?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, String>(0))
        .ctx("list merge requests")?;
    let mut out = Vec::new();
    for data in rows {
        out.push(from_json(
            &data.ctx("read merge request")?,
            "merge request",
        )?);
    }
    Ok(out)
}

fn open_for(conn: &Connection, item: &QueueItemId) -> Result<Option<MergeRequest>, ArchonError> {
    let open = load(
        conn,
        "SELECT data FROM merge_requests
         WHERE item = ?1 AND state IN ('awaiting_operator', 'queued', 'merging')",
        [item.as_str()],
    )?;
    assert!(open.len() <= 1, "an index keeps one open request per item");
    Ok(open.into_iter().next())
}

/// Refused while another request for the item is still open.
fn insert_in(tx: &Connection, merge: &MergeRequest) -> Result<(), ArchonError> {
    if let Some(open) = open_for(tx, &merge.item)? {
        let waiting = open
            .waiting_on()
            .map_or(String::new(), |n| format!("; Q{n} waits for the operator"));
        return Err(ArchonError::InvalidTransition(format!(
            "merge request {} for {} is still {}{waiting}",
            open.id,
            merge.item,
            open.state.kind()
        )));
    }
    let inserted = tx
        .execute(
            "INSERT INTO merge_requests (id, item, run, question, state, requested_at,
             updated_at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                merge.id.as_str(),
                merge.item.as_str(),
                merge.run.as_str(),
                question_of(merge).map(QuestionId::as_str),
                merge.state.kind(),
                ts(&merge.requested_at),
                ts(&merge.updated_at),
                to_json(merge, "merge request")?,
            ],
        )
        .ctx("insert merge request")?;
    assert_eq!(inserted, 1, "one row per merge request");
    Ok(())
}

pub(super) fn insert(conn: &mut Connection, merge: &MergeRequest) -> Result<(), ArchonError> {
    assert!(
        question_of(merge).is_none(),
        "a confirmed request is recorded with its question by `ask`"
    );
    let tx = write_tx(conn)?;
    insert_in(&tx, merge)?;
    tx.commit().ctx("commit merge request")
}

/// Ask `question` and record `merge` as waiting on it, in one transaction,
/// so no confirmation question exists without its request.
pub(super) fn ask(
    conn: &mut Connection,
    question: &Question,
    mut merge: MergeRequest,
) -> Result<(MergeRequest, FeedItem), ArchonError> {
    assert_eq!(
        merge.state,
        MergeState::AwaitingOperator,
        "it waits on the question"
    );
    let tx = write_tx(conn)?;
    let asked = feed::insert_question_in(&tx, question)?;
    merge.authority = MergeAuthority::Confirmed {
        question: asked.id.clone(),
        number: asked.number,
    };
    insert_in(&tx, &merge)?;
    let feed_item = feed::append_in(&tx, FeedEntry::Question(asked), merge.requested_at)?;
    tx.commit().ctx("commit merge question")?;
    Ok((merge, feed_item))
}

/// Identity is fixed, and a request that ended never changes again.
pub(super) fn update(conn: &mut Connection, merge: &MergeRequest) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    update_in(&tx, merge)?;
    settle_question(&tx, merge)?;
    tx.commit().ctx("commit merge request update")
}

fn update_in(tx: &Connection, merge: &MergeRequest) -> Result<(), ArchonError> {
    let stored = by_id(tx, merge)?;
    if stored.authority != merge.authority {
        return Err(ArchonError::Invalid(format!(
            "the authority of merge request {} cannot change",
            merge.id
        )));
    }
    if stored.item != merge.item || stored.run != merge.run || stored.pr_url != merge.pr_url {
        return Err(ArchonError::Invalid(format!(
            "the item, run and pull request of merge request {} cannot change",
            merge.id
        )));
    }
    if !stored.state.is_open() {
        return Err(ArchonError::InvalidTransition(format!(
            "merge request {} already ended ({})",
            merge.id,
            stored.state.kind()
        )));
    }
    let updated = tx
        .execute(
            "UPDATE merge_requests SET state = ?2, updated_at = ?3, data = ?4 WHERE id = ?1",
            params![
                merge.id.as_str(),
                merge.state.kind(),
                ts(&merge.updated_at),
                to_json(merge, "merge request")?,
            ],
        )
        .ctx("update merge request")?;
    assert_eq!(updated, 1, "the request was just read");
    Ok(())
}

fn settle_question(tx: &Connection, merge: &MergeRequest) -> Result<(), ArchonError> {
    if merge.state != MergeState::AwaitingOperator
        && let Some(question) = question_of(merge)
    {
        let text = format!("merge confirmation closed: {}", merge.line());
        feed::settle_in(tx, question, &text, AnswerVia::Archon, merge.updated_at)?;
    }
    Ok(())
}

pub(super) fn decide(
    conn: &mut Connection,
    merge: &MergeRequest,
    answer: &str,
    via: AnswerVia,
) -> Result<Vec<FeedItem>, ArchonError> {
    let tx = write_tx(conn)?;
    let stored = by_id(&tx, merge)?;
    if stored.state != MergeState::AwaitingOperator || merge.state == MergeState::AwaitingOperator {
        return Err(ArchonError::InvalidTransition(format!(
            "merge request {} must leave awaiting_operator when decided",
            merge.id
        )));
    }
    let question = question_of(&stored).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "merge request {} has no confirmation question",
            merge.id
        ))
    })?;
    let answers = feed::settle_in(&tx, question, answer, via, merge.updated_at)?
        .into_iter()
        .collect();
    update_in(&tx, merge)?;
    tx.commit().ctx("commit merge decision")?;
    Ok(answers)
}

pub(super) fn reconcile_questions(conn: &mut Connection) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    let merges = load(
        &tx,
        "SELECT m.data FROM merge_requests m JOIN questions q ON q.id = m.question
         WHERE m.state != 'awaiting_operator' AND q.open = 1 ORDER BY m.requested_at, m.id",
        [],
    )?;
    for merge in merges {
        settle_question(&tx, &merge)?;
    }
    tx.commit().ctx("commit merge question reconciliation")
}

fn by_id(conn: &Connection, merge: &MergeRequest) -> Result<MergeRequest, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM merge_requests WHERE id = ?1",
            [merge.id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up merge request")?;
    let data = data.ok_or_else(|| ArchonError::NotFound(format!("merge request {}", merge.id)))?;
    from_json(&data, "merge request")
}

pub(super) fn by_question(
    conn: &Connection,
    question: &QuestionId,
) -> Result<Option<MergeRequest>, ArchonError> {
    let found = load(
        conn,
        "SELECT data FROM merge_requests WHERE question = ?1",
        [question.as_str()],
    )?;
    assert!(found.len() <= 1, "a question confirms one request");
    Ok(found.into_iter().next())
}

/// Oldest first.
pub(super) fn for_item(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Vec<MergeRequest>, ArchonError> {
    load(
        conn,
        "SELECT data FROM merge_requests WHERE item = ?1 ORDER BY requested_at, id",
        [item.as_str()],
    )
}

/// Every request still waiting, queued or merging, oldest first.
pub(super) fn open(conn: &Connection) -> Result<Vec<MergeRequest>, ArchonError> {
    let open = load(
        conn,
        "SELECT data FROM merge_requests
         WHERE state IN ('awaiting_operator', 'queued', 'merging') ORDER BY requested_at, id",
        [],
    )?;
    debug_assert!(open.iter().all(|m| m.state.is_open()));
    Ok(open)
}
