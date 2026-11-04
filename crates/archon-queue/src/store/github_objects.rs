//! The read-only cache of GitHub issues and pull requests (V21,
//! CONTRACTS.md §1.3, owner D2). GitHub owns this data: rows are only ever
//! what a `GET` returned, kept with the ETag the next refresh sends. Their
//! text is untrusted and grants no authority.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::normalized_ref;
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::{ChecksSummary, ReviewDecision};
use archon_core::protocol::{GithubKind, GithubObject, GithubObjectState, GithubRef};
use archon_core::queue::QueueItem;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Store;
use super::codec::{SqlContext, from_json, sql_limit, to_i64, to_json, to_u64, ts};

/// The longest ETag kept; GitHub's are well under it.
pub const MAX_ETAG_BYTES: usize = 256;

const COLUMNS: &str = "host, repo, number, kind, title, state, state_reason, draft, merged_at, \
                       labels, author, updated_at, etag, fetched_at, review_decision, checks";

/// An object as last fetched, with what its refresh needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedObject {
    pub object: GithubObject,
    pub merged_at: Option<Timestamp>,
    /// Sent as `If-None-Match`, so an unchanged object costs a 304.
    pub etag: Option<String>,
}

/// A linked object the sync refreshes: never a pull request already merged
/// or closed, whose observation is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTarget {
    pub host: String,
    pub repo: String,
    pub number: u64,
    /// The cached state, `None` before the first fetch.
    pub state: Option<GithubObjectState>,
    pub etag: Option<String>,
}

/// A wire enum's snake_case name, as its serde form spells it.
fn name_of<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        other => unreachable!("a unit enum serializes as a string, not {other:?}"),
    }
}

fn parse_name<T: DeserializeOwned>(name: &str, what: &str) -> Result<T, ArchonError> {
    serde_json::from_value(serde_json::Value::String(name.to_owned()))
        .map_err(|_| ArchonError::Corrupt(format!("cached GitHub {what} {name:?}")))
}

fn parse_at(text: &str, what: &str) -> Result<Timestamp, ArchonError> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| ArchonError::Corrupt(format!("cached GitHub {what} {text:?}: {e}")))
}

fn check_etag(etag: Option<&str>) -> Result<(), ArchonError> {
    let Some(etag) = etag else {
        return Ok(());
    };
    let printable = etag.bytes().all(|b| (0x20..0x7f).contains(&b));
    if etag.is_empty() || etag.len() > MAX_ETAG_BYTES || !printable {
        return Err(ArchonError::Invalid(format!(
            "{etag:?} is not an ETag this cache keeps"
        )));
    }
    Ok(())
}

fn write_row(conn: &Connection, cached: &CachedObject) -> Result<(), ArchonError> {
    let object = &cached.object;
    let reference = &object.reference;
    let checks = object
        .checks
        .as_ref()
        .map(|c| to_json(c, "checks"))
        .transpose()?;
    let changed = conn
        .execute(
            &format!(
                "INSERT OR REPLACE INTO github_objects ({COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params![
                reference.host,
                reference.repo,
                to_i64(reference.number, "GitHub number")?,
                name_of(&reference.kind),
                object.title,
                name_of(&object.state),
                object.state_reason,
                object.draft,
                cached.merged_at.as_ref().map(ts),
                to_json(&object.labels, "labels")?,
                object.author,
                ts(&object.updated_at),
                cached.etag,
                ts(&object.fetched_at),
                object.review_decision.as_ref().map(name_of),
                checks,
            ],
        )
        .ctx("write cached GitHub object")?;
    assert_eq!(changed, 1, "one row per number");
    Ok(())
}

type Row = (
    (String, String, i64, String, String, String),
    (
        Option<String>,
        Option<bool>,
        Option<String>,
        String,
        Option<String>,
    ),
    (
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    ),
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
        ),
        (
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            row.get(14)?,
            row.get(15)?,
        ),
    ))
}

fn parse_row(row: Row) -> Result<CachedObject, ArchonError> {
    let ((host, repo, number, kind, title, state), more, rest) = row;
    let (state_reason, draft, merged_at, labels, author) = more;
    let (updated_at, etag, fetched_at, review_decision, checks) = rest;
    let reference = GithubRef {
        host,
        repo,
        number: to_u64(number, "GitHub number")?,
        kind: parse_name::<GithubKind>(&kind, "kind")?,
    };
    if normalized_ref(&reference).ok().as_ref() != Some(&reference) {
        return Err(ArchonError::Corrupt(format!(
            "a cached GitHub object has a malformed reference {reference:?}"
        )));
    }
    let object = GithubObject {
        reference,
        title,
        state: parse_name(&state, "state")?,
        state_reason,
        draft,
        labels: from_json::<Vec<String>>(&labels, "cached labels")?,
        author,
        updated_at: parse_at(&updated_at, "update time")?,
        fetched_at: parse_at(&fetched_at, "fetch time")?,
        review_decision: review_decision
            .map(|d| parse_name::<ReviewDecision>(&d, "review decision"))
            .transpose()?,
        checks: checks
            .map(|c| from_json::<ChecksSummary>(&c, "cached checks"))
            .transpose()?,
    };
    let merged_at = merged_at
        .map(|at| parse_at(&at, "merge time"))
        .transpose()?;
    Ok(CachedObject {
        object,
        merged_at,
        etag,
    })
}

/// The rows `filter` selects, newest update first, at most `limit` of them.
fn query_objects(
    conn: &Connection,
    filter: &str,
    limit: usize,
    args: impl rusqlite::Params,
) -> Result<Vec<CachedObject>, ArchonError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM github_objects WHERE {filter}
         ORDER BY updated_at DESC, host, repo, number DESC LIMIT {}",
        sql_limit(limit)
    );
    let mut stmt = conn
        .prepare_cached(&sql)
        .ctx("prepare cached GitHub objects")?;
    let rows = stmt
        .query_map(args, read_row)
        .ctx("read cached GitHub objects")?
        .collect::<Result<Vec<_>, _>>()
        .ctx("read cached GitHub objects")?;
    rows.into_iter().map(parse_row).collect()
}

/// host, repo, number, cached state, ETag.
type TargetRow = (String, String, i64, Option<String>, Option<String>);

fn read_target(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

impl Store {
    /// Keep what a fetch of one object returned, replacing what was cached.
    pub fn cache_github_object(&mut self, cached: &CachedObject) -> Result<(), ArchonError> {
        let reference = &cached.object.reference;
        if normalized_ref(reference)?.ne(reference) {
            return Err(ArchonError::Invalid(format!(
                "cache objects by their normalized reference, not {reference:?}"
            )));
        }
        check_etag(cached.etag.as_deref())?;
        let merged = cached.object.state == GithubObjectState::Merged;
        assert!(
            !merged || reference.kind == GithubKind::PullRequest,
            "only a pull request merges"
        );
        write_row(&self.conn, cached)
    }

    /// The cached object `reference` numbers, whichever kind it is.
    pub fn github_object(
        &self,
        reference: &GithubRef,
    ) -> Result<Option<CachedObject>, ArchonError> {
        let reference = normalized_ref(reference)?;
        let number = to_i64(reference.number, "GitHub number")?;
        let filter = "host = ?1 AND repo = ?2 AND number = ?3";
        let args = params![reference.host, reference.repo, number];
        let mut found = query_objects(&self.conn, filter, 2, args)?;
        assert!(found.len() <= 1, "one row per number");
        Ok(found.pop())
    }

    /// One repository's cached objects, newest update first, and when the
    /// newest of them was fetched; `repo` is `owner/name` on any host.
    pub fn github_objects(
        &self,
        repo: &str,
        kind: Option<GithubKind>,
        state: Option<GithubObjectState>,
        limit: usize,
    ) -> Result<(Vec<GithubObject>, Option<Timestamp>), ArchonError> {
        let repo = archon_core::github::GithubRepo::parse(repo)?;
        let kind = kind.map(|k| name_of(&k));
        let state = state.map(|s| name_of(&s));
        let filter = "repo = ?1 AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR state = ?3)";
        let rows = query_objects(
            &self.conn,
            filter,
            limit,
            params![repo.as_str(), kind, state],
        )?;
        assert!(rows.len() <= limit, "at most the limit");
        let synced: Option<String> = self
            .conn
            .query_row(
                "SELECT MAX(fetched_at) FROM github_objects WHERE repo = ?1",
                [repo.as_str()],
                |row| row.get(0),
            )
            .ctx("read the cache's last fetch")?;
        let synced_at = synced.map(|at| parse_at(&at, "fetch time")).transpose()?;
        Ok((rows.into_iter().map(|c| c.object).collect(), synced_at))
    }

    /// The cached objects `item` links to, newest update first.
    pub fn linked_github_objects(
        &self,
        item: &QueueItemId,
    ) -> Result<Vec<GithubObject>, ArchonError> {
        let filter = "(host, repo, number) IN
                      (SELECT host, repo, number FROM github_links WHERE item = ?1)";
        let rows = query_objects(&self.conn, filter, usize::MAX, [item.as_str()])?;
        Ok(rows.into_iter().map(|c| c.object).collect())
    }

    /// Up to `limit` linked objects to refresh, those never fetched first and
    /// then the stalest. A merged or closed pull request is no longer watched.
    pub fn github_sync_targets(&self, limit: usize) -> Result<Vec<SyncTarget>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT l.host, l.repo, l.number, o.state, o.etag
                 FROM github_links l LEFT JOIN github_objects o
                     ON (o.host, o.repo, o.number) = (l.host, l.repo, l.number)
                 WHERE COALESCE(o.kind = 'pull_request' AND o.state IN ('merged', 'closed'), 0) = 0
                 GROUP BY l.host, l.repo, l.number
                 ORDER BY o.fetched_at IS NOT NULL, o.fetched_at, l.host, l.repo, l.number
                 LIMIT ?1",
            )
            .ctx("prepare GitHub sync targets")?;
        let rows = stmt
            .query_map([sql_limit(limit)], read_target)
            .ctx("read GitHub sync targets")?
            .collect::<Result<Vec<_>, _>>()
            .ctx("read GitHub sync targets")?;
        let targets = rows
            .into_iter()
            .map(|(host, repo, number, state, etag)| {
                Ok(SyncTarget {
                    host,
                    repo,
                    number: to_u64(number, "GitHub number")?,
                    state: state.map(|s| parse_name(&s, "state")).transpose()?,
                    etag,
                })
            })
            .collect::<Result<Vec<_>, ArchonError>>()?;
        assert!(targets.len() <= limit, "at most the limit");
        Ok(targets)
    }

    /// The item an earlier import created under idempotency `key`.
    pub fn imported_item(&self, key: &str) -> Result<Option<QueueItem>, ArchonError> {
        assert!(key.starts_with("github:"), "an import's key");
        let item = super::items::by_key(&self.conn, key)?;
        Ok(item)
    }

    /// Rows this connection inserted, updated or deleted since it opened
    /// (SQLite's `total_changes`), so a check can show that a path wrote nothing.
    pub fn rows_written(&self) -> u64 {
        self.conn.total_changes()
    }
}

/// Whether `conn` has the row `reference` numbers; for the store's own tests.
#[cfg(test)]
fn has_row(conn: &Connection, reference: &GithubRef) -> bool {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT 1 FROM github_objects WHERE host = ?1 AND repo = ?2 AND number = ?3",
        params![reference.host, reference.repo, reference.number as i64],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(number: u64, kind: GithubKind, state: GithubObjectState) -> CachedObject {
        let at = archon_core::now();
        CachedObject {
            object: GithubObject {
                reference: GithubRef {
                    host: "github.com".into(),
                    repo: "o/r".into(),
                    number,
                    kind,
                },
                title: format!("object {number}"),
                state,
                state_reason: None,
                draft: (kind == GithubKind::PullRequest).then_some(false),
                labels: vec!["bug".into()],
                author: Some("someone".into()),
                updated_at: at,
                fetched_at: at,
                review_decision: None,
                checks: None,
            },
            merged_at: None,
            etag: Some("W/\"abc\"".into()),
        }
    }

    #[test]
    fn an_object_reads_back_as_written_and_replaces_its_row() {
        let mut store = Store::open_in_memory().unwrap();
        let mut cached = object(7, GithubKind::Issue, GithubObjectState::Open);
        store.cache_github_object(&cached).unwrap();
        let reference = cached.object.reference.clone();
        assert_eq!(
            store.github_object(&reference).unwrap(),
            Some(cached.clone())
        );
        cached.object.state = GithubObjectState::Closed;
        cached.object.state_reason = Some("completed".into());
        store.cache_github_object(&cached).unwrap();
        assert_eq!(store.github_object(&reference).unwrap(), Some(cached));
        assert!(has_row(&store.conn, &reference));
    }

    #[test]
    fn a_malformed_reference_or_etag_is_refused() {
        let mut store = Store::open_in_memory().unwrap();
        let migrated = store.rows_written();
        let mut shouted = object(1, GithubKind::Issue, GithubObjectState::Open);
        shouted.object.reference.repo = "O/R".into();
        assert!(store.cache_github_object(&shouted).is_err());
        let mut bad_etag = object(2, GithubKind::Issue, GithubObjectState::Open);
        bad_etag.etag = Some("W/\"a\"\nX-Injected: 1".into());
        assert!(store.cache_github_object(&bad_etag).is_err());
        assert_eq!(store.rows_written(), migrated, "nothing refused is written");
    }

    #[test]
    fn a_listing_filters_and_reports_its_last_fetch() {
        let mut store = Store::open_in_memory().unwrap();
        let (none, synced) = store.github_objects("o/r", None, None, 10).unwrap();
        assert!(none.is_empty() && synced.is_none());
        store
            .cache_github_object(&object(1, GithubKind::Issue, GithubObjectState::Open))
            .unwrap();
        store
            .cache_github_object(&object(
                2,
                GithubKind::PullRequest,
                GithubObjectState::Merged,
            ))
            .unwrap();
        let (all, synced) = store.github_objects("O/R", None, None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert!(synced.is_some());
        let (prs, _) = store
            .github_objects("o/r", Some(GithubKind::PullRequest), None, 10)
            .unwrap();
        assert_eq!(
            prs.iter().map(|o| o.reference.number).collect::<Vec<_>>(),
            [2]
        );
        let (open, _) = store
            .github_objects("o/r", None, Some(GithubObjectState::Open), 10)
            .unwrap();
        assert_eq!(
            open.iter().map(|o| o.reference.number).collect::<Vec<_>>(),
            [1]
        );
        let (one, _) = store.github_objects("o/r", None, None, 1).unwrap();
        assert_eq!(one.len(), 1);
    }
}
