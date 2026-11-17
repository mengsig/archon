//! Pause state and the `kv` table.

use archon_core::error::ArchonError;
use archon_core::queue::PauseState;
use rusqlite::{Connection, OptionalExtension, params};

use super::codec::{SqlContext, from_json, to_json, write_tx};
use super::repos;

/// No row yet means nothing was ever paused.
pub(super) fn pause(conn: &Connection) -> Result<PauseState, ArchonError> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM pause WHERE id = 1", [], |row| row.get(0))
        .optional()
        .ctx("read pause state")?;
    match data {
        Some(data) => from_json(&data, "pause state"),
        None => Ok(PauseState::default()),
    }
}

pub(super) fn write_pause(conn: &Connection, pause: &PauseState) -> Result<(), ArchonError> {
    conn.execute(
        "INSERT INTO pause (id, data) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET data = excluded.data",
        [to_json(pause, "pause state")?],
    )
    .ctx("write pause state")?;
    Ok(())
}

/// Paused repositories must be registered; duplicates are refused so the
/// stored lists stay sets.
pub(super) fn set_pause(conn: &mut Connection, pause: &PauseState) -> Result<(), ArchonError> {
    let mut seen = std::collections::BTreeSet::new();
    if let Some(dup) = pause.repos.iter().find(|r| !seen.insert(r.as_str())) {
        return Err(ArchonError::Invalid(format!(
            "repository {dup} paused twice"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    if let Some(dup) = pause.pipelines.iter().find(|p| !seen.insert(p.as_str())) {
        return Err(ArchonError::Invalid(format!(
            "pipeline {dup:?} paused twice"
        )));
    }
    let tx = write_tx(conn)?;
    for repo in &pause.repos {
        if !repos::exists(&tx, repo)? {
            return Err(ArchonError::NotFound(format!("repository {repo}")));
        }
    }
    write_pause(&tx, pause)?;
    tx.commit().ctx("commit pause state")
}

fn check_key(key: &str) -> Result<(), ArchonError> {
    if key.trim().is_empty() {
        return Err(ArchonError::Invalid("kv key is empty".into()));
    }
    Ok(())
}

pub(super) fn kv_get(conn: &Connection, key: &str) -> Result<Option<String>, ArchonError> {
    check_key(key)?;
    conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
    .ctx("read kv")
}

pub(super) fn kv_set(conn: &Connection, key: &str, value: &str) -> Result<(), ArchonError> {
    check_key(key)?;
    let changed = conn
        .execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .ctx("write kv")?;
    debug_assert_eq!(changed, 1, "kv upsert of {key} touched {changed} rows");
    Ok(())
}

const KV_RANGE: &str = "SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key";

/// Every `(key, value)` whose key starts with `prefix`, in key order. A
/// range on the primary key (`prefix <= key < prefix_end`), so the cost
/// follows the entries under the prefix, not the whole table. The prefix
/// must end in an ASCII character below DEL (every Archon prefix does);
/// any other is refused as `Invalid`.
pub(super) fn kv_prefix(
    conn: &Connection,
    prefix: &str,
) -> Result<Vec<(String, String)>, ArchonError> {
    check_key(prefix)?;
    let end = prefix_end(prefix)?;
    let mut statement = conn.prepare(KV_RANGE).ctx("prepare kv prefix range")?;
    let rows = statement
        .query_map(params![prefix, end], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ctx("scan kv")?
        .collect::<Result<Vec<_>, _>>()
        .ctx("read kv rows")?;
    debug_assert!(rows.iter().all(|(k, _)| k.starts_with(prefix)));
    Ok(rows)
}

/// The smallest string above every key starting with `prefix`: the prefix
/// with its last byte incremented. Keys compare bytewise (`BINARY`), so
/// this holds only when that byte is ASCII below DEL, which keeps the
/// result one byte and valid UTF-8; any other last character is refused.
fn prefix_end(prefix: &str) -> Result<String, ArchonError> {
    let mut bytes = prefix.as_bytes().to_vec();
    let last = bytes.last_mut().expect("checked: the prefix is not empty");
    if !last.is_ascii() || *last >= 0x7F {
        return Err(ArchonError::Invalid(format!(
            "kv prefix {prefix:?} must end in an ASCII character below DEL"
        )));
    }
    *last += 1;
    Ok(String::from_utf8(bytes).expect("ASCII stays UTF-8"))
}

/// Remove `key`; a key that is not there is not an error.
pub(super) fn kv_delete(conn: &Connection, key: &str) -> Result<(), ArchonError> {
    check_key(key)?;
    let removed = conn
        .execute("DELETE FROM kv WHERE key = ?1", [key])
        .ctx("delete kv")?;
    debug_assert!(removed <= 1, "kv key {key} had {removed} rows");
    Ok(())
}

pub(super) fn kv_update(
    conn: &mut Connection,
    set: &[(&str, &str)],
    delete: &[&str],
) -> Result<(), ArchonError> {
    assert!(
        set.iter().all(|(key, _)| !delete.contains(key)),
        "a key is either written or removed"
    );
    let tx = super::codec::write_tx(conn)?;
    for (key, value) in set {
        kv_set(&tx, key, value)?;
    }
    for key in delete {
        kv_delete(&tx, key)?;
    }
    tx.commit().ctx("commit kv update")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_end_bounds_every_key_under_the_prefix() {
        assert_eq!(prefix_end("run_node:").unwrap(), "run_node;");
        assert_eq!(prefix_end("a").unwrap(), "b");
        assert!("run_node:zzz" < prefix_end("run_node:").unwrap().as_str());
        assert!("run_node:" >= "run_node:");
        assert_eq!(prefix_end("caf~").unwrap(), "caf\u{7f}");
    }

    #[test]
    fn a_prefix_without_a_successor_byte_is_refused() {
        for prefix in ["é", "ab\u{7f}", "run:€"] {
            assert!(
                matches!(prefix_end(prefix), Err(ArchonError::Invalid(_))),
                "{prefix:?}"
            );
        }
    }

    #[test]
    fn a_kv_update_that_fails_part_way_changes_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema::migrate(&mut conn).unwrap();
        kv_set(&conn, "kept", "before").unwrap();
        let refused = kv_update(&mut conn, &[("kept", "after"), ("new", "1")], &[" "]);
        assert!(
            matches!(refused, Err(ArchonError::Invalid(_))),
            "{refused:?}"
        );
        assert_eq!(kv_get(&conn, "kept").unwrap().as_deref(), Some("before"));
        assert_eq!(kv_get(&conn, "new").unwrap(), None, "rolled back");

        kv_update(&mut conn, &[("new", "1")], &["kept"]).unwrap();
        assert_eq!(kv_get(&conn, "kept").unwrap(), None);
        assert_eq!(kv_get(&conn, "new").unwrap().as_deref(), Some("1"));
    }

    /// The query `kv_prefix` runs, planned against the migrated schema.
    #[test]
    fn kv_prefix_searches_the_primary_key_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema::migrate(&mut conn).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {KV_RANGE}"))
            .unwrap()
            .query_map(params!["run_node:", "run_node;"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|step| step.starts_with("SEARCH kv")),
            "a range, not a scan: {plan:?}"
        );
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN")),
            "{plan:?}"
        );
    }
}
