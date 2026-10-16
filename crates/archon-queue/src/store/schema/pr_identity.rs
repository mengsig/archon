use archon_core::pull_request::PullRequestRef;
use rusqlite::OptionalExtension;

use super::*;

/// A store at version 14 holding one repository and the item `qi_a` in it.
fn prior_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(14).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch(
        "INSERT INTO repos (id, name, path, added_at, data) VALUES ('repo_a', 'a', '/a', 't', '{}');
         INSERT INTO items (id, repo, pipeline, priority, state, tags, created_at, updated_at, data)
         VALUES ('qi_a', 'repo_a', 'direct', 1, 'done', '[]', 'a', 'a', '{}');",
    )
    .unwrap();
    conn
}

fn insert_run(conn: &Connection, run: &str, branch: &str) {
    let data = serde_json::json!({ "branch": branch }).to_string();
    conn.execute(
        "INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
         VALUES (?1, 'qi_a', 'direct', 1, 't', ?2)",
        rusqlite::params![run, data],
    )
    .unwrap();
}

/// A stage of `run` started at second `at` in `state` that reported `url`.
fn insert_stage(conn: &Connection, run: &str, at: u32, state: &str, url: &str) {
    let data = serde_json::json!({ "structured": { "pr_url": url } }).to_string();
    conn.execute(
        "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, state, terminal,
             started_at, data)
         VALUES (?1, ?2, 'ship', 1, 0, ?3, 1, ?4, ?5)",
        rusqlite::params![
            format!("stg_{run}_{at}"),
            run,
            state,
            format!("t{at:02}"),
            data
        ],
    )
    .unwrap();
}

fn recorded(conn: &Connection, run: &str) -> Option<PullRequestRef> {
    conn.query_row(
        "SELECT host, repository, number FROM run_pull_requests WHERE run = ?1",
        [run],
        |row| {
            Ok(PullRequestRef {
                host: row.get(0)?,
                repository: row.get(1)?,
                number: row.get::<_, i64>(2)? as u64,
            })
        },
    )
    .optional()
    .unwrap()
}

#[test]
fn v15_reads_every_url_as_the_daemon_does() {
    let mut conn = prior_store();
    let urls = [
        "https://github.com/o/r/pull/7",
        "https://GitHub.com/Mengsig/Archon/pull/69",
        "https://ghe.example:8443/o-x/r_y.z/pull/123456789012345678",
        "https://github.com/o/pull/pull/5",
        "https://github.com/o/r/pull/0",
        "https://github.com/o/r/pull/01",
        "https://github.com/o/r/pull/1/",
        "https://github.com/o/r/pull/1?x=1",
        "https://github.com/o/r/pull/1#discussion",
        "https://github.com/o/r/issues/1",
        "https://github.com/o/pull/1",
        "https://github.com/o/r/x/pull/1",
        "https://github.com/o/r/pull/1/pull/2",
        "https://github.com//r/pull/1",
        "https://github.com/o/../pull/1",
        "https://github.com/o/r/pull/1234567890123456789",
        "https://gi*hub.com/o/r/pull/1",
        "https:///o/r/pull/1",
        "http://github.com/o/r/pull/1",
        "https://example.invalid/pr/1",
    ];
    for (n, url) in urls.iter().enumerate() {
        insert_run(&conn, &format!("prun_{n}"), &format!("b{n}"));
        insert_stage(&conn, &format!("prun_{n}"), 1, "completed", url);
    }
    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    for (n, url) in urls.iter().enumerate() {
        let expected = PullRequestRef::from_url(url);
        assert_eq!(recorded(&conn, &format!("prun_{n}")), expected, "{url}");
    }
    assert_eq!(recorded(&conn, "prun_0").unwrap().number, 7);
}

#[test]
fn v15_takes_the_newest_url_of_a_completed_stage() {
    let mut conn = prior_store();
    insert_run(&conn, "prun_a", "b");
    insert_stage(
        &conn,
        "prun_a",
        1,
        "completed",
        "https://github.com/o/r/pull/1",
    );
    insert_stage(
        &conn,
        "prun_a",
        2,
        "completed",
        "https://github.com/o/r/pull/2",
    );
    insert_stage(
        &conn,
        "prun_a",
        3,
        "failed",
        "https://github.com/o/r/pull/3",
    );
    insert_run(&conn, "prun_b", "c");
    insert_stage(
        &conn,
        "prun_b",
        1,
        "failed",
        "https://github.com/o/r/pull/4",
    );
    migrate(&mut conn).unwrap();
    assert_eq!(recorded(&conn, "prun_a").unwrap().number, 2);
    assert_eq!(
        recorded(&conn, "prun_b"),
        None,
        "no completed stage named one"
    );
    let node: Option<String> = conn
        .query_row("SELECT node_id FROM run_pull_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(node, None, "GitHub is asked for it later");
}

fn kv(conn: &Connection) -> Vec<(String, String)> {
    conn.prepare("SELECT key, value FROM kv ORDER BY key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn v15_keys_a_settled_branch_by_its_one_pull_request() {
    let mut conn = prior_store();
    insert_run(&conn, "prun_one", "one");
    insert_stage(
        &conn,
        "prun_one",
        1,
        "completed",
        "https://github.com/o/r/pull/1",
    );
    for (run, number) in [("prun_two_a", 2), ("prun_two_b", 3)] {
        insert_run(&conn, run, "two");
        let url = format!("https://github.com/o/r/pull/{number}");
        insert_stage(&conn, run, 1, "completed", &url);
    }
    conn.execute_batch(
        "INSERT INTO kv VALUES ('pr_settled:repo_a:one', 'MERGED');
         INSERT INTO kv VALUES ('pr_settled:repo_a:two', 'CLOSED');
         INSERT INTO kv VALUES ('pr_settled:repo_a:none', 'CLOSED');
         INSERT INTO kv VALUES ('other', 'kept');",
    )
    .unwrap();
    migrate(&mut conn).unwrap();
    let expected = [("other", "kept"), ("pr_settled:github.com/o/r#1", "MERGED")];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    assert_eq!(
        kv(&conn),
        expected,
        "a branch of two pull requests, or of none recorded, is asked again"
    );
}
