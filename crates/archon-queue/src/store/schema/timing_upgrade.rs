use super::*;

const PRE_TIMING_VERSION: usize = 27;

fn prior_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(PRE_TIMING_VERSION).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    let version: i64 = conn
        .query_row("SELECT max(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, PRE_TIMING_VERSION as i64);
    conn.execute_batch(
        "INSERT INTO items (id, pipeline, priority, state, tags, created_at, updated_at, data)
         VALUES ('qi_queued', 'direct', 1, 'queued', '[]',
                 '2026-01-01T00:00:00.000000000Z', '2026-01-01T00:00:00.000000000Z', '{}'),
                ('qi_delivered', 'direct', 1, 'succeeded', '[]',
                 '2026-01-01T00:00:00.000000000Z', '2026-01-01T00:00:00.000000000Z', '{}');
         INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
         VALUES ('prun_old', 'qi_delivered', 'direct', 1, '2026-01-01T00:00:00.000000000Z', '{}');
         INSERT INTO run_pull_requests (run, host, repository, number, node_id)
         VALUES ('prun_old', 'github.com', 'owner/repo', 42, 'PR_node');",
    )
    .unwrap();
    conn
}

#[test]
fn v28_keeps_deliveries_and_only_seeds_live_queue_history() {
    let mut conn = prior_store();
    let before = archon_core::now();
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    let legacy: (i64, String, bool) = conn
        .query_row(
            "SELECT number, node_id, (created_at IS NULL AND merged_at IS NULL
                     AND checks_started_at IS NULL AND checks_finished_at IS NULL)
             FROM run_pull_requests WHERE run = 'prun_old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(legacy, (42, "PR_node".into(), true));
    let (item, reason, start): (String, String, String) = conn
        .query_row(
            "SELECT item, reason, started_at FROM queue_reason_intervals WHERE ended_at IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((item.as_str(), reason.as_str()), ("qi_queued", "other"));
    let start: archon_core::Timestamp = start.parse().unwrap();
    let precision = chrono::TimeDelta::milliseconds(1);
    assert!(start >= before - precision && start <= archon_core::now() + precision);
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    let intervals: i64 = conn
        .query_row("SELECT COUNT(*) FROM queue_reason_intervals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        intervals, 1,
        "only already queued work gains an interval, once"
    );
}
