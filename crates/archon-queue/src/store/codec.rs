//! Conversions between domain values and SQLite columns, and the error
//! mapping every store query shares.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::queue::Priority;
use chrono::SecondsFormat;
use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Adds the operation that failed to a rusqlite error.
pub(super) trait SqlContext<T> {
    fn ctx(self, what: &str) -> Result<T, ArchonError>;
}

impl<T> SqlContext<T> for rusqlite::Result<T> {
    fn ctx(self, what: &str) -> Result<T, ArchonError> {
        self.map_err(|e| store_error(what, &e))
    }
}

/// A rusqlite error as a store error naming `what` failed; a full disk
/// keeps its identity as `DiskFull`, so the daemon can say so.
pub(super) fn store_error(what: &str, error: &rusqlite::Error) -> ArchonError {
    assert!(!what.is_empty(), "a store error names what failed");
    match error.sqlite_error_code() {
        Some(ErrorCode::DiskFull) => ArchonError::DiskFull(format!("{what}: {error}")),
        _ => ArchonError::Store(format!("{what}: {error}")),
    }
}

/// A write transaction that takes the write lock up front, so two
/// connections never deadlock upgrading a read lock (busy_timeout then
/// covers the wait).
pub(super) fn write_tx(conn: &mut Connection) -> Result<Transaction<'_>, ArchonError> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .ctx("begin write transaction")
}

/// A transaction over several reads, or none when the connection holds one
/// already: a reader keeps one snapshot for as long as it is borrowed.
pub(super) fn read_tx(conn: &Connection) -> Result<Option<Transaction<'_>>, ArchonError> {
    if !conn.is_autocommit() {
        return Ok(None);
    }
    conn.unchecked_transaction()
        .ctx("begin read transaction")
        .map(Some)
}

/// End a transaction [`read_tx`] began; reads leave nothing to keep.
pub(super) fn end_read_tx(tx: Option<Transaction<'_>>, what: &str) -> Result<(), ArchonError> {
    match tx {
        Some(tx) => tx.finish().ctx(what),
        None => Ok(()),
    }
}

/// Fixed-width RFC 3339 with nanoseconds, so text order is time order.
pub(super) fn ts(at: &Timestamp) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) fn to_json<T: Serialize>(value: &T, what: &str) -> Result<String, ArchonError> {
    serde_json::to_string(value).map_err(|e| ArchonError::Store(format!("encode {what}: {e}")))
}

/// A row that no longer decodes is corrupted storage, never a caller error.
pub(super) fn from_json<T: DeserializeOwned>(raw: &str, what: &str) -> Result<T, ArchonError> {
    serde_json::from_str(raw).map_err(|e| ArchonError::Corrupt(format!("decode {what}: {e}")))
}

pub(super) fn to_i64(value: u64, what: &str) -> Result<i64, ArchonError> {
    i64::try_from(value).map_err(|_| ArchonError::Invalid(format!("{what} {value} is too large")))
}

pub(super) fn to_u64(value: i64, what: &str) -> Result<u64, ArchonError> {
    u64::try_from(value).map_err(|_| ArchonError::Store(format!("{what} {value} is negative")))
}

/// SQL `LIMIT` from a caller-supplied count; counts beyond `i64` clamp.
pub(super) fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

pub(super) fn priority_rank(priority: Priority) -> i64 {
    match priority {
        Priority::Low => 0,
        Priority::Normal => 1,
        Priority::High => 2,
        Priority::Critical => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timestamps_sort_as_text_in_time_order() {
        let early = chrono::Utc.timestamp_opt(1_700_000_000, 5).unwrap();
        let late = chrono::Utc
            .timestamp_opt(1_700_000_000, 400_000_000)
            .unwrap();
        let (a, b) = (ts(&early), ts(&late));
        assert_eq!(a.len(), b.len(), "fixed width: {a} vs {b}");
        assert!(a < b);
    }

    #[test]
    fn priority_ranks_follow_the_enum_order() {
        let all = [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Critical,
        ];
        for pair in all.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(priority_rank(pair[0]) < priority_rank(pair[1]));
        }
    }

    #[test]
    fn a_full_disk_keeps_its_identity_and_other_failures_stay_store_errors() {
        let failure = |code: i32, text: &str| {
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some(text.to_owned()))
        };
        let full = failure(rusqlite::ffi::SQLITE_FULL, "database or disk is full");
        let error = rusqlite::Result::<()>::Err(full)
            .ctx("insert feed entry")
            .unwrap_err();
        assert!(matches!(&error, ArchonError::DiskFull(m) if m.contains("insert feed entry")));
        assert!(error.is_store(), "the daemon still stops its work on it");
        assert_eq!(
            error.to_string(),
            "store: disk full: insert feed entry: database or disk is full"
        );
        let corrupt = failure(
            rusqlite::ffi::SQLITE_CORRUPT,
            "database disk image is malformed",
        );
        let other = store_error("read item", &corrupt);
        assert!(matches!(&other, ArchonError::Store(m) if m.starts_with("read item: ")));
    }

    #[test]
    fn integer_conversions_refuse_out_of_range_values() {
        assert!(to_i64(u64::MAX, "seq").is_err());
        assert_eq!(to_i64(7, "seq").unwrap(), 7);
        assert!(to_u64(-1, "seq").is_err());
        assert_eq!(sql_limit(usize::MAX), i64::MAX);
    }
}
