//! The operator feed and the questions awaiting an answer.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherQuestionRef;
use archon_core::error::ArchonError;
use archon_core::id::{FeedItemId, QuestionId};
use archon_core::notify::{AnswerVia, FeedEntry, FeedItem, Notification, Question};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::codec::{SqlContext, from_json, sql_limit, to_i64, to_json, to_u64, ts, write_tx};

/// How a feed row stores its entry, owned by the store so a change to
/// `FeedEntry`'s wire tagging never strands existing rows. Notifications
/// are stored bare: their own `kind` field once clashed with that tag.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredEntry {
    Notification(Notification),
    Entry(FeedEntry),
}

impl StoredEntry {
    fn from_entry(entry: &FeedEntry) -> Self {
        match entry {
            FeedEntry::Notification(n) => Self::Notification(n.clone()),
            other => Self::Entry(other.clone()),
        }
    }

    fn into_entry(self) -> FeedEntry {
        match self {
            Self::Notification(n) => FeedEntry::Notification(n),
            Self::Entry(entry) => entry,
        }
    }
}

fn decode_row(row: &rusqlite::Row<'_>) -> Result<FeedItem, ArchonError> {
    let seq: i64 = row.get(0).ctx("read feed seq")?;
    let id: String = row.get(1).ctx("read feed id")?;
    let at: String = row.get(2).ctx("read feed time")?;
    let data: String = row.get(3).ctx("read feed entry")?;
    let at = chrono::DateTime::parse_from_rfc3339(&at)
        .map_err(|e| ArchonError::Store(format!("decode feed time {at:?}: {e}")))?
        .with_timezone(&chrono::Utc);
    let stored: StoredEntry = from_json(&data, "feed entry")?;
    Ok(FeedItem {
        id: FeedItemId::parse(&id)?,
        seq: to_u64(seq, "feed seq")?,
        at,
        entry: stored.into_entry(),
    })
}

pub(super) fn last_seq(conn: &Connection) -> Result<u64, ArchonError> {
    let seq: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM feed", [], |row| {
            row.get(0)
        })
        .ctx("read last feed seq")?;
    to_u64(seq, "feed seq")
}

/// The next `seq` is read and written under the write lock, so concurrent
/// appenders on any connection still produce 1, 2, 3, ... without gaps.
pub(super) fn append(
    conn: &mut Connection,
    entry: FeedEntry,
    at: Timestamp,
) -> Result<FeedItem, ArchonError> {
    let tx = write_tx(conn)?;
    let item = append_in(&tx, entry, at)?;
    tx.commit().ctx("commit feed item")?;
    Ok(item)
}

pub(super) fn append_with_kv(
    conn: &mut Connection,
    entry: FeedEntry,
    at: Timestamp,
    updates: &[(&str, &str)],
) -> Result<FeedItem, ArchonError> {
    let tx = write_tx(conn)?;
    let item = append_in(&tx, entry, at)?;
    for (key, value) in updates {
        super::settings::kv_set(&tx, key, value)?;
    }
    tx.commit().ctx("commit feed item and kv updates")?;
    Ok(item)
}

/// Append inside the caller's write transaction `tx`.
fn append_in(tx: &Connection, entry: FeedEntry, at: Timestamp) -> Result<FeedItem, ArchonError> {
    let seq = last_seq(tx)? + 1;
    let item = FeedItem {
        id: FeedItemId::new(),
        seq,
        at,
        entry,
    };
    tx.execute(
        "INSERT INTO feed (seq, id, at, visible, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            to_i64(seq, "feed seq")?,
            item.id.as_str(),
            ts(&item.at),
            item.visible(false),
            to_json(&StoredEntry::from_entry(&item.entry), "feed entry")?,
        ],
    )
    .ctx("append feed item")?;
    debug_assert!(seq >= 1);
    Ok(item)
}

fn read_items(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<FeedItem>, ArchonError> {
    let mut stmt = conn.prepare_cached(sql).ctx("read feed")?;
    let mut rows = stmt.query(args).ctx("read feed")?;
    let mut out: Vec<FeedItem> = Vec::new();
    while let Some(row) = rows.next().ctx("read feed")? {
        out.push(decode_row(row)?);
    }
    debug_assert!(
        out.windows(2).all(|w| w[0].seq < w[1].seq),
        "feed out of order"
    );
    Ok(out)
}

/// Every item with `seq > after`, oldest first.
pub(super) fn after(
    conn: &Connection,
    after: u64,
    limit: usize,
) -> Result<Vec<FeedItem>, ArchonError> {
    read_items(
        conn,
        "SELECT seq, id, at, data FROM feed WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        params![to_i64(after, "feed seq")?, sql_limit(limit)],
    )
}

/// The newest `limit` items before `before` (all when `None`), oldest
/// first; progress stays hidden unless `verbose`.
pub(super) fn before(
    conn: &Connection,
    before: Option<u64>,
    limit: usize,
    verbose: bool,
) -> Result<Vec<FeedItem>, ArchonError> {
    let before = match before {
        Some(seq) => to_i64(seq, "feed seq")?,
        None => i64::MAX,
    };
    let items = read_items(
        conn,
        "SELECT seq, id, at, data FROM (
             SELECT seq, id, at, data FROM feed WHERE seq < ?1 AND (?2 OR visible = 1)
             ORDER BY seq DESC LIMIT ?3
         ) ORDER BY seq",
        params![before, verbose, sql_limit(limit)],
    )?;
    debug_assert!(items.iter().all(|i| i.visible(verbose)));
    Ok(items)
}

fn check_question(question: &Question) -> Result<(), ArchonError> {
    if question.text.trim().is_empty() {
        return Err(ArchonError::Invalid("a question needs text".into()));
    }
    if question.answer.is_some() != question.answered_at.is_some() {
        return Err(ArchonError::Invalid(format!(
            "question {} must have answered_at exactly when it has an answer",
            question.id
        )));
    }
    Ok(())
}

/// An update keeps the number; the `number = ?7` guard makes a changed one
/// touch no row, which the assert below catches.
fn write_question(conn: &Connection, question: &Question, insert: bool) -> Result<(), ArchonError> {
    assert!(
        question.number > 0,
        "question {} has no number",
        question.id
    );
    let sql = if insert {
        "INSERT INTO questions (id, open, asked_at, aether_agent, aether_seq, data, number)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
    } else {
        "UPDATE questions SET open = ?2, asked_at = ?3, aether_agent = ?4, aether_seq = ?5,
                              data = ?6 WHERE id = ?1 AND number = ?7"
    };
    let aether = question.aether.as_ref();
    let aether_seq = aether.map(|a| to_i64(a.seq, "question seq")).transpose()?;
    let changed = conn
        .execute(
            sql,
            params![
                question.id.as_str(),
                question.is_open(),
                ts(&question.asked_at),
                aether.map(|a| a.agent.as_str()),
                aether_seq,
                to_json(question, "question")?,
                to_i64(question.number, "question number")?,
            ],
        )
        .ctx("write question")?;
    assert_eq!(
        changed, 1,
        "write of question {} touched {changed} rows",
        question.id
    );
    Ok(())
}

pub(super) fn load_question(conn: &Connection, id: &QuestionId) -> Result<Question, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM questions WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read question")?;
    let data = data.ok_or_else(|| ArchonError::NotFound(format!("question {id}")))?;
    from_json(&data, "question")
}

/// The number the next recorded question gets: one past the highest ever
/// given, so numbers are never reused.
fn next_number(conn: &Connection) -> Result<u64, ArchonError> {
    let highest: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(number), 0) FROM questions",
            [],
            |row| row.get(0),
        )
        .ctx("read highest question number")?;
    Ok(to_u64(highest, "question number")? + 1)
}

/// Records `question` under the next number and returns it numbered. An
/// Aether `ask_user` question is recorded once, however often its
/// `question.asked` event is delivered.
pub(super) fn insert_question(
    conn: &mut Connection,
    question: &Question,
) -> Result<Question, ArchonError> {
    check_question(question)?;
    let tx = write_tx(conn)?;
    let taken: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM questions WHERE id = ?1)",
            [question.id.as_str()],
            |row| row.get(0),
        )
        .ctx("look up question")?;
    if taken {
        return Err(ArchonError::Invalid(format!(
            "question {} already exists",
            question.id
        )));
    }
    if let Some(aether) = &question.aether
        && let Some(existing) = by_aether(&tx, aether)?
    {
        return Err(ArchonError::Invalid(format!(
            "Aether question {}#{} is already recorded as {}",
            aether.agent, aether.seq, existing.id
        )));
    }
    assert_eq!(
        question.number, 0,
        "new question {} arrives numbered; the store numbers questions",
        question.id
    );
    let numbered = Question {
        number: next_number(&tx)?,
        ..question.clone()
    };
    write_question(&tx, &numbered, true)?;
    tx.commit().ctx("commit question")?;
    Ok(numbered)
}

/// Close the open question `id` of a withdrawn gate inside the caller's
/// write transaction `tx`, with its feed answer. An answered one is kept.
pub(super) fn close_withdrawn(
    tx: &Connection,
    id: &QuestionId,
    reason: &str,
    at: Timestamp,
) -> Result<Option<FeedItem>, ArchonError> {
    assert!(!reason.trim().is_empty(), "a withdrawal gives its reason");
    let mut question = load_question(tx, id)?;
    if !question.is_open() {
        return Ok(None);
    }
    let text = format!("withdrawn: {reason}");
    question.answer = Some(text.clone());
    question.answered_at = Some(at.max(question.asked_at));
    write_question(tx, &question, false)?;
    let entry = FeedEntry::Answer {
        question: id.clone(),
        text,
        via: AnswerVia::Archon,
    };
    let item = append_in(tx, entry, at)?;
    debug_assert!(!load_question(tx, id)?.is_open());
    Ok(Some(item))
}

pub(super) fn answer(
    conn: &mut Connection,
    id: &QuestionId,
    text: &str,
    at: Timestamp,
) -> Result<Question, ArchonError> {
    if text.trim().is_empty() {
        return Err(ArchonError::Invalid("an answer needs text".into()));
    }
    let tx = write_tx(conn)?;
    let mut question = load_question(&tx, id)?;
    if !question.is_open() {
        return Err(ArchonError::InvalidTransition(format!(
            "question {id} was already answered"
        )));
    }
    question.answer = Some(text.to_owned());
    question.answered_at = Some(at);
    write_question(&tx, &question, false)?;
    tx.commit().ctx("commit answer")?;
    Ok(question)
}

/// Oldest first.
pub(super) fn list(conn: &Connection, open_only: bool) -> Result<Vec<Question>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM questions WHERE (?1 = 0 OR open = 1) ORDER BY asked_at, id",
        )
        .ctx("list questions")?;
    let rows = stmt
        .query_map([open_only], |row| row.get::<_, String>(0))
        .ctx("list questions")?;
    let mut out: Vec<Question> = Vec::new();
    for data in rows {
        out.push(from_json(&data.ctx("read question")?, "question")?);
    }
    debug_assert!(!open_only || out.iter().all(Question::is_open));
    Ok(out)
}

pub(super) fn by_aether(
    conn: &Connection,
    aether: &AetherQuestionRef,
) -> Result<Option<Question>, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM questions WHERE aether_agent = ?1 AND aether_seq = ?2",
            params![aether.agent.as_str(), to_i64(aether.seq, "question seq")?],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up Aether question")?;
    data.map(|d| from_json(&d, "question")).transpose()
}
