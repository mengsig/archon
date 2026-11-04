use super::*;

const T1: &str = "2026-10-01T00:00:00.000000000Z";
const T2: &str = "2026-10-02T00:00:00.000000000Z";
const T3: &str = "2026-10-03T00:00:00.000000000Z";

/// A store as Archon left it at version 19, built from the migrations alone:
/// an item asked for on a thread whose two runs delivered one pull request, a
/// local item whose run delivered another, and an item with neither.
fn v19_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(19).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    let origin = r#"{"origin":{"repository":"mengsig/archon","number":384}}"#;
    for (item, data, created) in [
        ("qi_watched", origin, T1),
        ("qi_local", "{}", T2),
        ("qi_idle", "{}", T3),
    ] {
        conn.execute(
            "INSERT INTO items (id, pipeline, priority, state, tags, created_at, updated_at, data)
             VALUES (?1, 'adaptive', 1, 'succeeded', '[]', ?2, ?2, ?3)",
            rusqlite::params![item, created, data],
        )
        .unwrap();
    }
    conn.execute_batch(&format!(
        "INSERT INTO runs (id, item, pipeline, terminal, started_at, data) VALUES
             ('prun_w1', 'qi_watched', 'adaptive', 1, '{T2}', '{{}}'),
             ('prun_w2', 'qi_watched', 'adaptive', 1, '{T3}', '{{}}'),
             ('prun_l1', 'qi_local', 'adaptive', 1, '{T3}', '{{}}'),
             ('prun_i1', 'qi_idle', 'adaptive', 1, '{T3}', '{{}}');
         INSERT INTO run_pull_requests (run, host, repository, number) VALUES
             ('prun_w1', 'github.com', 'mengsig/archon', 378),
             ('prun_w2', 'github.com', 'mengsig/archon', 378),
             ('prun_l1', 'ghe.example', 'team/tool', 5);"
    ))
    .unwrap();
    conn
}

type Row = (String, String, String, i64, String, String, String, String);

fn links(conn: &Connection) -> Vec<Row> {
    let mut stmt = conn
        .prepare(
            "SELECT item, host, repo, number, kind, relation, source, created_at
             FROM github_links ORDER BY item, relation",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// The `(at, data)` of every `link` change, in seq order.
fn link_changes(conn: &Connection) -> Vec<(String, serde_json::Value)> {
    let mut stmt = conn
        .prepare("SELECT at, data FROM changes WHERE json_extract(data, '$.subject') = 'link' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|row| {
            let (at, data) = row.unwrap();
            (at, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

fn row(
    item: &str,
    host: &str,
    repo: &str,
    number: i64,
    kind: &str,
    relation: &str,
    at: &str,
) -> Row {
    let text = |s: &str| s.to_owned();
    (
        text(item),
        text(host),
        text(repo),
        number,
        text(kind),
        text(relation),
        text("backfill"),
        text(at),
    )
}

fn rerun_backfill(conn: &mut Connection) {
    let tx = conn.transaction().unwrap();
    tx.execute_batch(GITHUB_LINKS_BACKFILL).unwrap();
    tx.commit().unwrap();
}

#[test]
fn v20_backfills_exactly_the_origins_and_deliveries_with_their_changes() {
    let mut conn = v19_store();
    let seq_before: i64 = conn
        .query_row("SELECT MAX(seq) FROM changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    let expected = vec![
        row(
            "qi_local",
            "ghe.example",
            "team/tool",
            5,
            "pull_request",
            "delivered",
            T3,
        ),
        row(
            "qi_watched",
            "github.com",
            "mengsig/archon",
            378,
            "pull_request",
            "delivered",
            T2,
        ),
        row(
            "qi_watched",
            "github.com",
            "mengsig/archon",
            384,
            "issue",
            "origin",
            T1,
        ),
    ];
    assert_eq!(
        links(&conn),
        expected,
        "one row per link, the earliest run's time"
    );

    let changes = link_changes(&conn);
    let link = |item: &str, host: &str, repo: &str, number: u64, relation: &str| {
        serde_json::json!({"subject": "link", "item": item, "host": host, "repo": repo,
                           "number": number, "relation": relation})
    };
    let expected_changes = vec![
        (
            T1.to_owned(),
            link("qi_watched", "github.com", "mengsig/archon", 384, "origin"),
        ),
        (
            T2.to_owned(),
            link(
                "qi_watched",
                "github.com",
                "mengsig/archon",
                378,
                "delivered",
            ),
        ),
        (
            T3.to_owned(),
            link("qi_local", "ghe.example", "team/tool", 5, "delivered"),
        ),
    ];
    assert_eq!(changes, expected_changes, "a change per backfilled link");
    let first_link_seq: i64 = conn
        .query_row(
            "SELECT MIN(seq) FROM changes WHERE json_extract(data, '$.subject') = 'link'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        first_link_seq,
        seq_before + 1,
        "appended after every earlier change"
    );
    for (_, data) in &changes {
        serde_json::from_value::<archon_core::protocol::ChangeSubject>(data.clone())
            .expect("a change the stream reads");
    }

    rerun_backfill(&mut conn);
    assert_eq!(links(&conn), expected, "running it again adds nothing");
    assert_eq!(
        link_changes(&conn),
        expected_changes,
        "and records no change"
    );
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    assert_eq!(links(&conn), expected);
}

/// A link a live path wrote keeps its source; only the missing ones are added.
#[test]
fn the_backfill_keeps_live_links_and_fills_only_the_gaps() {
    let mut conn = v19_store();
    migrate(&mut conn).unwrap();
    conn.execute_batch(
        "DELETE FROM github_links WHERE item = 'qi_local';
         UPDATE github_links SET source = 'watcher' WHERE relation = 'origin';",
    )
    .unwrap();
    let changes_before = link_changes(&conn).len();
    rerun_backfill(&mut conn);
    let sources: Vec<(String, String, String)> = conn
        .prepare("SELECT item, relation, source FROM github_links ORDER BY item, relation")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let triple = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
    assert_eq!(
        sources,
        vec![
            triple("qi_local", "delivered", "backfill"),
            triple("qi_watched", "delivered", "backfill"),
            triple("qi_watched", "origin", "watcher"),
        ]
    );
    assert_eq!(
        link_changes(&conn).len(),
        changes_before + 1,
        "one change, for the one gap"
    );
}

#[test]
fn a_link_row_refuses_what_the_store_never_writes() {
    let mut conn = v19_store();
    migrate(&mut conn).unwrap();
    for bad in [
        "('qi_idle', 'GitHub.com', 'o/r', 1, 'issue', 'closes', 'operator', 't')",
        "('qi_idle', 'github.com', 'O/R', 1, 'issue', 'closes', 'operator', 't')",
        "('qi_idle', 'github.com', 'o/r', 0, 'issue', 'closes', 'operator', 't')",
        "('qi_idle', 'github.com', 'o/r', 1, 'discussion', 'closes', 'operator', 't')",
        "('qi_idle', 'github.com', 'o/r', 1, 'issue', 'fixes', 'operator', 't')",
        "('qi_nobody', 'github.com', 'o/r', 1, 'issue', 'closes', 'operator', 't')",
    ] {
        let insert = format!("INSERT INTO github_links VALUES {bad}");
        assert!(conn.execute_batch(&insert).is_err(), "{bad} was stored");
    }
    conn.execute_batch(
        "DELETE FROM run_pull_requests WHERE run = 'prun_l1';
         DELETE FROM runs WHERE item = 'qi_local'; DELETE FROM items WHERE id = 'qi_local';",
    )
    .expect("an item's links go with it");
    assert_eq!(links(&conn).len(), 2, "qi_local's link went with it");
}
