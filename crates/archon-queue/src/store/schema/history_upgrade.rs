use super::*;

/// A store as Archon 1.4.7 left it, with one finished run that has an
/// outcome, a stage outcome and a lesson use.
fn v17_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(17).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch(
        "INSERT INTO items (id, pipeline, priority, state, tags, created_at, updated_at, data)
         VALUES ('qi_old', 'direct', 1, 'succeeded', '[]', 'a', 'a', '{}');
         INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
         VALUES ('prun_old', 'qi_old', 'direct', 1, 'a', '{}');
         INSERT INTO outcomes (run, item, repo, pipeline, size, result, failure_class, corrected,
                               merged, cost_usd, finished_at, updated_at, data)
         VALUES ('prun_old', 'qi_old', 'repo_a', 'direct', 's', 'succeeded', NULL, 0, 1, 1.5,
                 'b', 'c', '{\"kept\":\"verbatim\"}');
         INSERT INTO outcome_stages VALUES ('prun_old', 0, 'build', 'm', 'high', 1.5, 60, 1, 1);
         INSERT INTO lessons (id, kind, state, pinned, uses, wins, confidence, source,
                              created_at, updated_at, data)
         VALUES ('lsn_a', 'tip', 'active', 0, 1, 1, 0.5, 'run', 't', 't', '{}');
         INSERT INTO lesson_uses VALUES ('lsn_a', 'prun_old', 'd', 1);",
    )
    .unwrap();
    conn
}

fn learning_rows(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT json_array(run, item, repo, pipeline, size, result, failure_class, corrected,
                           merged, cost_usd, finished_at, updated_at, data) FROM outcomes
         UNION ALL SELECT json_array(run, ordinal, stage_id, model, effort, cost_usd, wall_secs,
                                     attempts, passed) FROM outcome_stages
         UNION ALL SELECT json_array(lesson, run, used_at, won) FROM lesson_uses",
    )
    .unwrap()
    .query_map([], |row| row.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn v18_keeps_learning_whole_once_its_run_and_item_are_gone() {
    let mut conn = v17_store();
    let before = learning_rows(&conn);
    assert_eq!(before.len(), 3);
    let refused = conn.execute_batch("DELETE FROM runs WHERE id = 'prun_old'");
    assert!(refused.is_err(), "before V18 an outcome holds its run");

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    assert_eq!(learning_rows(&conn), before, "every column is copied");
    conn.execute_batch(
        "DELETE FROM runs WHERE id = 'prun_old'; DELETE FROM items WHERE id = 'qi_old';",
    )
    .unwrap();
    assert_eq!(learning_rows(&conn), before, "learning outlives its run");
    let dangling: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dangling, 0);
    let orphan = conn.execute_batch(
        "INSERT INTO outcome_stages VALUES ('prun_none', 0, 'build', 'm', 'high', 0, 0, 0, NULL)",
    );
    assert!(orphan.is_err(), "a stage outcome still needs its outcome");
    let indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN
             ('outcomes_by_route', 'outcomes_by_item', 'outcome_stages_by_choice',
              'lesson_uses_by_run')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 4, "the indexes are made again");
}
