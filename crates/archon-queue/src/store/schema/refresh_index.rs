use super::*;

/// A store at version 16 in the shape 1.4.5 left: `qi_second` replaced the
/// settled `qi_first` under the owner's key for commit `abc`.
fn prior_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(16).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    for (id, state) in [
        ("qi_owner", "done"),
        ("qi_first", "failed"),
        ("qi_second", "done"),
        ("qi_fix", "done"),
    ] {
        conn.execute(
            "INSERT INTO items (id, pipeline, priority, state, tags, created_at, updated_at, data)
             VALUES (?1, 'direct', 1, ?2, '[]', 'a', 'a', '{}')",
            [id, state],
        )
        .unwrap();
    }
    let refresh = r#"{"owner":"qi_owner","branch":"b","team":false,
                      "refresh":{"base":"main","sha":"abc"}}"#;
    let fix = r#"{"owner":"qi_owner","branch":"b","team":false}"#;
    for (key, value) in [
        ("restack_fix:qi_owner:refresh:abc", "qi_second"),
        ("restack_target:qi_first", refresh),
        ("restack_target:qi_second", refresh),
        ("restack_fix:qi_owner:qi_parent:def", "qi_fix"),
        ("restack_target:qi_fix", fix),
        ("restack_target:qi_gone", refresh),
        ("pr_settled:github.com/o/r#1", "MERGED"),
    ] {
        conn.execute("INSERT INTO kv (key, value) VALUES (?1, ?2)", [key, value])
            .unwrap();
    }
    conn
}

/// The owner's index as `Store::stack_fixes_of` reads it.
fn owner_index(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT key, value FROM kv WHERE key GLOB 'restack_fix:qi_owner:*' ORDER BY key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn v17_indexes_each_refresh_a_later_one_replaced_under_its_own_key() {
    let mut conn = prior_store();
    let before = owner_index(&conn);
    assert_eq!(before.len(), 2, "1.4.5 kept only the latest refresh");

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    let mut expected = before.clone();
    expected.push((
        "restack_fix:qi_owner:refresh:abc:qi_first".into(),
        "qi_first".into(),
    ));
    expected.sort();
    assert_eq!(owner_index(&conn), expected, "only the replaced refresh");
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM kv", [], |row| row.get(0))
        .unwrap();
    assert_eq!(total, 8, "no row for an item the store no longer holds");
}
