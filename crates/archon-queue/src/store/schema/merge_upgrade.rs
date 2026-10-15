use super::*;

fn prior_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(13).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch(
        "INSERT INTO items
        (id, pipeline, priority, state, tags, created_at, updated_at, data)
        VALUES ('qi_old', 'direct', 1, 'done', '[]', 'a', 'a', '{}');
        INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
        VALUES ('run_old', 'qi_old', 'direct', 1, 'a', '{}');
        INSERT INTO questions (id, open, asked_at, data, number)
        VALUES ('q_old', 1, 'a', '{}', 1);",
    )
    .unwrap();
    conn
}

fn insert(
    conn: &Connection,
    id: &str,
    state: &str,
    question: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO merge_requests
        (id, item, run, question, state, requested_at, updated_at, data)
        VALUES (?1, 'qi_old', 'run_old', ?2, ?3, 'a', 'b', ?4)",
        rusqlite::params![id, question, state, r#"{"kept":"verbatim"}"#],
    )
}

fn rows(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT json_array(id, item, run, question, state, requested_at, updated_at, data)
        FROM merge_requests ORDER BY id",
    )
    .unwrap()
    .query_map([], |row| row.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn v14_preserves_every_merge_column_and_extends_open_uniqueness() {
    let mut conn = prior_store();
    for state in ["merged", "declined", "refused", "merging"] {
        insert(&conn, state, state, None).unwrap();
    }
    let before = rows(&conn);
    assert!(insert(&conn, "queued_before", "queued", Some("q_old")).is_err());
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    assert_eq!(rows(&conn), before);
    assert!(insert(&conn, "queued_blocked", "queued", Some("q_old")).is_err());
    conn.execute(
        "UPDATE merge_requests SET state = 'merged' WHERE state = 'merging'",
        [],
    )
    .unwrap();
    insert(&conn, "queued", "queued", Some("q_old")).unwrap();
    for state in ["queued", "awaiting_operator", "merging", "unknown"] {
        assert!(insert(&conn, "blocked", state, None).is_err(), "{state}");
    }
    assert!(
        conn.execute(
            "UPDATE merge_requests SET question = 'missing' WHERE id = 'queued'",
            []
        )
        .is_err()
    );
    let indexes: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master
        WHERE type = 'index' AND name IN ('merge_requests_by_item', 'merge_requests_open_per_item')",
        [], |row| row.get(0)).unwrap();
    assert_eq!(indexes, 2);
    let after = rows(&conn);
    migrate(&mut conn).unwrap();
    assert_eq!(rows(&conn), after, "migration is applied once");
}

#[test]
fn v14_keeps_waiting_confirmation_links_unique() {
    let mut conn = prior_store();
    insert(&conn, "waiting", "awaiting_operator", Some("q_old")).unwrap();
    let before = rows(&conn);
    migrate(&mut conn).unwrap();
    assert_eq!(rows(&conn), before);
    conn.execute("UPDATE merge_requests SET state = 'declined'", [])
        .unwrap();
    assert!(insert(&conn, "duplicate_question", "queued", Some("q_old")).is_err());
    insert(&conn, "new", "queued", None).unwrap();
}
