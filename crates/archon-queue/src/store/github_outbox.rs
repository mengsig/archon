//! The GitHub outbox (V22, ARCHITECTURE §f, owner D3): every comment Archon
//! posts is written here, under a token its body carries, before the post.
//! A row stays `pending` until GitHub's comment id is recorded, so a restart
//! looks for the token on GitHub before it ever posts again.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::normalized_ref;
use archon_core::id::{OutboxToken, PipelineRunId, QueueItemId};
use archon_core::protocol::{GithubKind, GithubRef};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, sql_limit, to_i64, to_u64, ts, write_tx};

/// The longest failure kept on a row; `gh`'s complaints are cut to it.
pub const MAX_OUTBOX_ERROR_CHARS: usize = 2_000;

const COLUMNS: &str = "token, purpose, item, run, host, repo, number, body, state, attempts, \
                       next_attempt_at, last_error, comment_id, comment_url, created_at, posted_at";

/// Why a comment is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxPurpose {
    /// A `github_reply` on a watched thread.
    Reply,
    /// A ship comment: at most one per (item, run).
    Ship,
}

impl OutboxPurpose {
    fn name(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Ship => "ship",
        }
    }

    fn parse(name: &str) -> Result<Self, ArchonError> {
        match name {
            "reply" => Ok(Self::Reply),
            "ship" => Ok(Self::Ship),
            other => Err(ArchonError::Corrupt(format!("outbox purpose {other:?}"))),
        }
    }
}

/// A comment to post, recorded before any network call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxIntent {
    pub token: OutboxToken,
    pub purpose: OutboxPurpose,
    pub item: Option<QueueItemId>,
    pub run: Option<PipelineRunId>,
    /// The issue or pull request commented on; its kind is not kept.
    pub target: GithubRef,
    /// The whole body as posted, the token's marker included.
    pub body: String,
}

/// Where a recorded comment stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxState {
    /// Not known to be on GitHub: the post failed, or its answer was lost.
    Pending {
        attempts: u32,
        next_attempt_at: Timestamp,
        last_error: Option<String>,
    },
    Posted {
        comment_id: u64,
        url: Option<String>,
        at: Timestamp,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub intent: OutboxIntent,
    pub state: OutboxState,
    pub created_at: Timestamp,
}

impl OutboxRow {
    pub fn is_pending(&self) -> bool {
        matches!(self.state, OutboxState::Pending { .. })
    }
}

fn parse_at(text: &str) -> Result<Timestamp, ArchonError> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| ArchonError::Corrupt(format!("outbox time {text:?}: {e}")))
}

type Row = (
    (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
    ),
    (i64, String, String, i64, String, Option<String>),
    (Option<i64>, Option<String>, String, Option<String>),
);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        (
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ),
        (
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
        ),
        (row.get(12)?, row.get(13)?, row.get(14)?, row.get(15)?),
    ))
}

fn parse_state(
    state: &str,
    pending: (i64, &str, Option<String>),
    posted: (Option<i64>, Option<String>, Option<String>),
) -> Result<OutboxState, ArchonError> {
    let (attempts, next_attempt_at, last_error) = pending;
    let (comment_id, url, posted_at) = posted;
    match (state, comment_id, posted_at) {
        ("pending", None, None) => Ok(OutboxState::Pending {
            attempts: u32::try_from(attempts)
                .map_err(|_| ArchonError::Corrupt(format!("outbox attempts {attempts}")))?,
            next_attempt_at: parse_at(next_attempt_at)?,
            last_error,
        }),
        ("posted", Some(id), Some(at)) => Ok(OutboxState::Posted {
            comment_id: to_u64(id, "comment id")?,
            url,
            at: parse_at(&at)?,
        }),
        (other, ..) => Err(ArchonError::Corrupt(format!(
            "an outbox row in state {other:?} with an inconsistent comment"
        ))),
    }
}

fn parse_row(row: Row) -> Result<OutboxRow, ArchonError> {
    let ((token, purpose, item, run, host, repo), more, rest) = row;
    let (number, body, state, attempts, next_attempt_at, last_error) = more;
    let (comment_id, comment_url, created_at, posted_at) = rest;
    let token = OutboxToken::parse(&token)
        .map_err(|_| ArchonError::Corrupt(format!("outbox token {token:?}")))?;
    let intent = OutboxIntent {
        purpose: OutboxPurpose::parse(&purpose)?,
        item: item.map(|i| QueueItemId::parse(&i)).transpose()?,
        run: run.map(|r| PipelineRunId::parse(&r)).transpose()?,
        target: GithubRef {
            host,
            repo,
            number: to_u64(number, "GitHub number")?,
            kind: GithubKind::Issue,
        },
        body,
        token,
    };
    assert!(
        intent.body.contains(intent.token.as_str()),
        "V22 checks the body carries its token"
    );
    let state = parse_state(
        &state,
        (attempts, &next_attempt_at, last_error),
        (comment_id, comment_url, posted_at),
    )?;
    Ok(OutboxRow {
        intent,
        state,
        created_at: parse_at(&created_at)?,
    })
}

fn query_rows(
    conn: &Connection,
    filter: &str,
    limit: usize,
    args: impl rusqlite::Params,
) -> Result<Vec<OutboxRow>, ArchonError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM github_outbox WHERE {filter}
         ORDER BY next_attempt_at, token LIMIT {}",
        sql_limit(limit)
    );
    let mut stmt = conn.prepare_cached(&sql).ctx("prepare outbox rows")?;
    let rows = stmt
        .query_map(args, read_row)
        .ctx("read outbox rows")?
        .collect::<Result<Vec<_>, _>>()
        .ctx("read outbox rows")?;
    rows.into_iter().map(parse_row).collect()
}

fn row_by_token(conn: &Connection, token: &OutboxToken) -> Result<Option<OutboxRow>, ArchonError> {
    let mut rows = query_rows(conn, "token = ?1", 2, [token.as_str()])?;
    assert!(rows.len() <= 1, "a token names one row");
    Ok(rows.pop())
}

/// The row an earlier ship comment of `(item, run)` left, if any.
fn ship_row(conn: &Connection, intent: &OutboxIntent) -> Result<Option<OutboxRow>, ArchonError> {
    let (Some(item), Some(run)) = (&intent.item, &intent.run) else {
        return Ok(None);
    };
    let filter = "item = ?1 AND run = ?2 AND purpose = ?3";
    let mut rows = query_rows(
        conn,
        filter,
        2,
        params![item.as_str(), run.as_str(), intent.purpose.name()],
    )?;
    assert!(
        rows.len() <= 1,
        "V22 keeps one row per (item, run, purpose)"
    );
    Ok(rows.pop())
}

fn check_intent(intent: &OutboxIntent) -> Result<GithubRef, ArchonError> {
    let target = normalized_ref(&GithubRef {
        kind: GithubKind::Issue,
        ..intent.target.clone()
    })?;
    if intent.body.matches(intent.token.as_str()).count() != 1 {
        return Err(ArchonError::Invalid(format!(
            "an outbox body carries its token {} exactly once",
            intent.token
        )));
    }
    if intent.purpose == OutboxPurpose::Ship && (intent.item.is_none() || intent.run.is_none()) {
        return Err(ArchonError::Invalid(
            "a ship comment names its item and run".into(),
        ));
    }
    Ok(target)
}

fn cut_error(error: &str) -> String {
    error.chars().take(MAX_OUTBOX_ERROR_CHARS).collect()
}

impl Store {
    /// Record `intent` before it is posted, first attempt due at
    /// `first_attempt_at`. A ship comment already recorded for the same
    /// (item, run) is returned instead, so it is never posted twice.
    pub fn enqueue_outbox(
        &mut self,
        intent: &OutboxIntent,
        now: &Timestamp,
        first_attempt_at: &Timestamp,
    ) -> Result<OutboxRow, ArchonError> {
        let target = check_intent(intent)?;
        let tx = write_tx(&mut self.conn)?;
        if let Some(earlier) = ship_row(&tx, intent)? {
            tx.finish().ctx("end outbox read")?;
            return Ok(earlier);
        }
        tx.execute(
            &format!(
                "INSERT INTO github_outbox ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', 0, ?9, NULL, NULL, NULL, ?10, NULL)"
            ),
            params![
                intent.token.as_str(),
                intent.purpose.name(),
                intent.item.as_ref().map(QueueItemId::as_str),
                intent.run.as_ref().map(PipelineRunId::as_str),
                target.host,
                target.repo,
                to_i64(target.number, "GitHub number")?,
                intent.body,
                ts(first_attempt_at),
                ts(now),
            ],
        )
        .ctx("record outbox intent")?;
        let row = row_by_token(&tx, &intent.token)?.expect("the row just written");
        tx.commit().ctx("commit outbox intent")?;
        assert!(row.is_pending(), "a new intent is pending");
        Ok(row)
    }

    /// Record that `token`'s comment is on GitHub as `comment_id`. Returns
    /// whether this marked it; one already posted keeps its first id.
    pub fn mark_outbox_posted(
        &mut self,
        token: &OutboxToken,
        comment_id: u64,
        url: Option<&str>,
        at: &Timestamp,
    ) -> Result<bool, ArchonError> {
        assert!(comment_id > 0, "GitHub comment ids are positive");
        let tx = write_tx(&mut self.conn)?;
        let changed = tx
            .execute(
                "UPDATE github_outbox SET state = 'posted', comment_id = ?2, comment_url = ?3,
                     posted_at = ?4, last_error = NULL
                 WHERE token = ?1 AND state = 'pending'",
                params![
                    token.as_str(),
                    to_i64(comment_id, "comment id")?,
                    url,
                    ts(at)
                ],
            )
            .ctx("mark outbox row posted")?;
        if changed == 0 && row_by_token(&tx, token)?.is_none() {
            return Err(ArchonError::NotFound(format!("outbox row {token}")));
        }
        tx.commit().ctx("commit outbox post")?;
        assert!(changed <= 1, "a token names one row");
        Ok(changed == 1)
    }

    /// Record a failed or unconfirmed attempt of a pending `token`: it stays
    /// pending, next tried at `next_attempt_at`.
    pub fn mark_outbox_failed(
        &mut self,
        token: &OutboxToken,
        error: &str,
        next_attempt_at: &Timestamp,
    ) -> Result<(), ArchonError> {
        assert!(!error.is_empty(), "a failure says why");
        let tx = write_tx(&mut self.conn)?;
        let changed = tx
            .execute(
                "UPDATE github_outbox SET attempts = attempts + 1, last_error = ?2,
                     next_attempt_at = ?3
                 WHERE token = ?1 AND state = 'pending'",
                params![token.as_str(), cut_error(error), ts(next_attempt_at)],
            )
            .ctx("record outbox failure")?;
        assert!(changed <= 1, "a token names one row");
        if changed == 0 {
            return Err(ArchonError::Invalid(format!(
                "outbox row {token} is not pending"
            )));
        }
        tx.commit().ctx("commit outbox failure")?;
        Ok(())
    }

    /// The row `token` names.
    pub fn outbox_row(&self, token: &OutboxToken) -> Result<Option<OutboxRow>, ArchonError> {
        row_by_token(&self.conn, token)
    }

    /// Up to `limit` pending rows due by `now`, the longest due first.
    pub fn due_outbox(&self, now: &Timestamp, limit: usize) -> Result<Vec<OutboxRow>, ArchonError> {
        let rows = query_rows(
            &self.conn,
            "state = 'pending' AND next_attempt_at <= ?1",
            limit,
            [ts(now)],
        )?;
        assert!(
            rows.iter().all(OutboxRow::is_pending),
            "only pending rows are due"
        );
        Ok(rows)
    }

    /// Up to `limit` pending rows, due or not, and how many there are.
    pub fn pending_outbox(&self, limit: usize) -> Result<(Vec<OutboxRow>, u64), ArchonError> {
        let rows = query_rows(&self.conn, "state = 'pending'", limit, [])?;
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM github_outbox WHERE state = 'pending'",
                [],
                |r| r.get(0),
            )
            .optional()
            .ctx("count pending outbox rows")?
            .unwrap_or(0);
        let total = to_u64(total, "pending count")?;
        assert!(rows.len() as u64 <= total, "a page of the pending rows");
        Ok((rows, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(purpose: OutboxPurpose) -> OutboxIntent {
        let token = OutboxToken::new();
        OutboxIntent {
            body: format!("hello\n<!-- archon-outbox:{token} -->"),
            token,
            purpose,
            item: Some(QueueItemId::new()),
            run: Some(PipelineRunId::new()),
            target: GithubRef {
                host: "github.com".into(),
                repo: "O/R".into(),
                number: 5,
                kind: GithubKind::PullRequest,
            },
        }
    }

    #[test]
    fn an_intent_is_pending_until_its_comment_id_is_recorded() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let reply = intent(OutboxPurpose::Reply);
        let row = store.enqueue_outbox(&reply, &now, &now).unwrap();
        assert_eq!(row.intent.target.repo, "o/r", "kept normalized");
        assert_eq!(store.due_outbox(&now, 10).unwrap().len(), 1);
        store
            .mark_outbox_failed(
                &reply.token,
                "HTTP 502",
                &(now + chrono::Duration::minutes(1)),
            )
            .unwrap();
        let Some(OutboxRow {
            state:
                OutboxState::Pending {
                    attempts,
                    last_error,
                    ..
                },
            ..
        }) = store.outbox_row(&reply.token).unwrap()
        else {
            panic!("a failed post stays pending");
        };
        assert_eq!((attempts, last_error.as_deref()), (1, Some("HTTP 502")));
        assert!(store.due_outbox(&now, 10).unwrap().is_empty(), "backed off");
        assert_eq!(store.pending_outbox(10).unwrap().1, 1, "still visible");
        assert!(
            store
                .mark_outbox_posted(&reply.token, 9, None, &now)
                .unwrap()
        );
        assert!(
            !store
                .mark_outbox_posted(&reply.token, 10, None, &now)
                .unwrap()
        );
        let posted = store.outbox_row(&reply.token).unwrap().unwrap();
        assert!(
            matches!(posted.state, OutboxState::Posted { comment_id: 9, .. }),
            "{posted:?}"
        );
        assert!(
            store
                .mark_outbox_failed(&reply.token, "late", &now)
                .is_err()
        );
        assert_eq!(store.pending_outbox(10).unwrap().1, 0);
    }

    #[test]
    fn a_ship_comment_is_recorded_once_per_item_and_run() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let first = intent(OutboxPurpose::Ship);
        store.enqueue_outbox(&first, &now, &now).unwrap();
        let again = OutboxIntent {
            token: OutboxToken::new(),
            ..first.clone()
        };
        let again = OutboxIntent {
            body: format!("again {}", again.token),
            ..again
        };
        let row = store.enqueue_outbox(&again, &now, &now).unwrap();
        assert_eq!(row.intent.token, first.token, "the first intent stands");
        assert_eq!(store.pending_outbox(10).unwrap().1, 1);
        let replies = [intent(OutboxPurpose::Reply), intent(OutboxPurpose::Reply)];
        for reply in &replies {
            store
                .enqueue_outbox(
                    &OutboxIntent {
                        item: None,
                        run: None,
                        ..reply.clone()
                    },
                    &now,
                    &now,
                )
                .unwrap();
        }
        assert_eq!(
            store.pending_outbox(10).unwrap().1,
            3,
            "replies have no (item, run)"
        );
    }

    #[test]
    fn an_intent_without_its_token_or_a_ship_without_its_run_is_refused() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let written = store.rows_written();
        let mut unmarked = intent(OutboxPurpose::Reply);
        unmarked.body = "no marker".into();
        assert!(store.enqueue_outbox(&unmarked, &now, &now).is_err());
        let mut runless = intent(OutboxPurpose::Ship);
        runless.run = None;
        assert!(store.enqueue_outbox(&runless, &now, &now).is_err());
        assert_eq!(store.rows_written(), written, "nothing refused is written");
    }
}
