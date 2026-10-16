//! Connection setup and forward-only migrations. A migration is appended to
//! [`MIGRATIONS`] and never edited once released; its 1-based position is
//! its version in `schema_migrations`.

use std::path::Path;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use rusqlite::{Connection, ErrorCode};

use super::codec::{SqlContext, ts, write_tx};

pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Version 1: the whole v1 store. `data` columns hold the full record as
/// JSON; the other columns are derived from it for filtering and ordering.
const V1: &str = r#"
CREATE TABLE repos (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE,
    path     TEXT NOT NULL UNIQUE,
    added_at TEXT NOT NULL,
    data     TEXT NOT NULL
);

CREATE TABLE items (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    id              TEXT NOT NULL UNIQUE,
    repo            TEXT,
    pipeline        TEXT NOT NULL,
    priority        INTEGER NOT NULL,
    state           TEXT NOT NULL,
    tags            TEXT NOT NULL,
    idempotency_key TEXT UNIQUE,
    request         TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    data            TEXT NOT NULL
);
CREATE INDEX items_by_state ON items (state, priority DESC, seq);
CREATE INDEX items_by_repo ON items (repo);

CREATE TABLE item_deps (
    item       TEXT NOT NULL REFERENCES items (id),
    depends_on TEXT NOT NULL REFERENCES items (id),
    PRIMARY KEY (item, depends_on),
    CHECK (item <> depends_on)
) WITHOUT ROWID;
CREATE INDEX item_deps_reverse ON item_deps (depends_on, item);

CREATE TABLE runs (
    id         TEXT PRIMARY KEY,
    item       TEXT NOT NULL REFERENCES items (id),
    pipeline   TEXT NOT NULL,
    terminal   INTEGER NOT NULL CHECK (terminal IN (0, 1)),
    started_at TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE UNIQUE INDEX runs_one_active_per_item ON runs (item) WHERE terminal = 0;
CREATE INDEX runs_by_item ON runs (item, started_at);

CREATE TABLE stage_runs (
    id           TEXT PRIMARY KEY,
    run          TEXT NOT NULL REFERENCES runs (id),
    stage_id     TEXT NOT NULL,
    attempt      INTEGER NOT NULL,
    fan_index    INTEGER NOT NULL,
    aether_agent TEXT UNIQUE,
    state        TEXT NOT NULL,
    terminal     INTEGER NOT NULL CHECK (terminal IN (0, 1)),
    started_at   TEXT NOT NULL,
    data         TEXT NOT NULL
);
CREATE INDEX stage_runs_by_run ON stage_runs (run, started_at, attempt, fan_index);
CREATE INDEX stage_runs_live ON stage_runs (run) WHERE terminal = 0;

CREATE TABLE feed (
    seq     INTEGER PRIMARY KEY,
    id      TEXT NOT NULL UNIQUE,
    at      TEXT NOT NULL,
    visible INTEGER NOT NULL CHECK (visible IN (0, 1)),
    data    TEXT NOT NULL
);

CREATE TABLE questions (
    id           TEXT PRIMARY KEY,
    open         INTEGER NOT NULL CHECK (open IN (0, 1)),
    asked_at     TEXT NOT NULL,
    aether_agent TEXT,
    aether_seq   INTEGER,
    data         TEXT NOT NULL,
    UNIQUE (aether_agent, aether_seq)
);

CREATE TABLE spend (
    agent              TEXT PRIMARY KEY,
    item               TEXT REFERENCES items (id),
    cost_usd           REAL NOT NULL,
    estimated_cost_usd REAL NOT NULL,
    input_tokens       INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    at                 TEXT NOT NULL
);
CREATE INDEX spend_by_item ON spend (item);

CREATE TABLE spend_daily (
    agent              TEXT NOT NULL REFERENCES spend (agent),
    day                TEXT NOT NULL,
    cost_usd           REAL NOT NULL,
    estimated_cost_usd REAL NOT NULL,
    PRIMARY KEY (agent, day)
) WITHOUT ROWID;
CREATE INDEX spend_daily_by_day ON spend_daily (day);

CREATE TABLE pause (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    data TEXT NOT NULL
);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Version 2: autonomy (ARCHITECTURE §18). Operator gates and directives get
/// tables; items gain their knobs and repos trade `auto_merge` for a ceiling.
const V2: &str = r#"
CREATE TABLE gates (
    run       TEXT NOT NULL REFERENCES runs (id),
    stage_id  TEXT NOT NULL,
    attempt   INTEGER NOT NULL,
    item      TEXT NOT NULL REFERENCES items (id),
    open      INTEGER NOT NULL CHECK (open IN (0, 1)),
    question  TEXT UNIQUE,
    report    TEXT,
    opened_at TEXT NOT NULL,
    data      TEXT NOT NULL,
    PRIMARY KEY (run, stage_id, attempt)
) WITHOUT ROWID;
CREATE INDEX gates_open ON gates (opened_at) WHERE open = 1;
CREATE INDEX gates_by_item ON gates (item, opened_at);
CREATE UNIQUE INDEX gates_one_open_per_report ON gates (report) WHERE open = 1;

CREATE TABLE directives (
    id         TEXT PRIMARY KEY,
    set_at     TEXT NOT NULL,
    expires_at TEXT,
    data       TEXT NOT NULL
);

UPDATE items SET data = json_set(data, '$.autonomy', json_object(
    'review', CASE pipeline
        WHEN 'quick-pr' THEN 'none'
        WHEN 'strict-pr-review' THEN 'strict'
        ELSE 'standard' END,
    'proposal', 'off',
    'report', 'off',
    'merge', CASE WHEN EXISTS (
        SELECT 1 FROM repos r
        WHERE r.id = items.repo AND json_extract(r.data, '$.auto_merge') = 1
    ) THEN 'auto' ELSE 'manual' END,
    'tools', 'guarded'))
WHERE json_extract(data, '$.autonomy') IS NULL;
UPDATE items SET request = json_set(request, '$.autonomy', json(json_extract(data, '$.autonomy')))
WHERE request IS NOT NULL;

UPDATE repos SET data = json_set(json_remove(data, '$.auto_merge'), '$.autonomy_max',
    CASE WHEN json_extract(data, '$.auto_merge') = 1
        THEN json_object('merge', 'auto') ELSE json_object() END);
"#;

/// Version 3: V2 copied each item's backfilled knobs into its enqueue
/// fingerprint, which a retry of the original request no longer produced.
/// Items enqueued before V2 get their V1 fingerprint back; `items::insert`
/// matches those against a retry that asks for no knob.
const V3: &str = r#"
UPDATE items SET request = json_remove(request, '$.autonomy')
WHERE request IS NOT NULL
  AND created_at < (SELECT applied_at FROM schema_migrations WHERE version = 2);
"#;

/// Version 4: every question gets a permanent operator-facing number (`Q7`),
/// so a number read in one listing never names another question later.
/// Questions already recorded are numbered in asking order.
const V4: &str = r#"
ALTER TABLE questions ADD COLUMN number INTEGER NOT NULL DEFAULT 0;
UPDATE questions SET number = ranked.n
FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY asked_at, id) AS n FROM questions) AS ranked
WHERE questions.id = ranked.id;
UPDATE questions SET data = json_set(data, '$.number', number);
CREATE UNIQUE INDEX questions_by_number ON questions (number);
"#;

/// Version 5: provenance labels only; raw charges and daily deltas stay intact.
/// Old rows have no evidence of billing and remain unclassified.
const V5: &str = r#"
ALTER TABLE spend ADD COLUMN cost_basis TEXT NOT NULL DEFAULT 'unknown'
    CHECK (cost_basis IN ('unknown', 'list_price', 'provider_reported', 'mixed'));
"#;

/// Version 6: durable run ownership for delegated agents. Only legacy items
/// with one run can attribute their unlinked spend without guessing.
const V6: &str = r#"
CREATE TABLE agent_runs (
    agent TEXT PRIMARY KEY,
    run TEXT NOT NULL REFERENCES runs(id)
);
CREATE INDEX agent_runs_by_run ON agent_runs(run);
INSERT INTO agent_runs SELECT aether_agent, run FROM stage_runs WHERE aether_agent IS NOT NULL;
INSERT OR IGNORE INTO agent_runs
SELECT SUBSTR(k.key, LENGTH('link_child_') + 1), r.id FROM kv k
JOIN runs r ON r.item = CASE WHEN json_valid(k.value) THEN json_extract(k.value, '$') END
WHERE k.key GLOB 'link_child_*' AND json_valid(k.value)
AND (SELECT COUNT(*) FROM runs other WHERE other.item = r.item) = 1;
INSERT OR IGNORE INTO agent_runs
SELECT SUBSTR(k.key, LENGTH('link_stage_agent_') + 1), r.id FROM kv k
JOIN runs r ON r.item = k.value WHERE k.key GLOB 'link_stage_agent_*'
AND (SELECT COUNT(*) FROM runs other WHERE other.item = r.item) = 1;
INSERT OR IGNORE INTO agent_runs
SELECT s.agent, r.id FROM spend s JOIN runs r ON r.item = s.item
WHERE (SELECT COUNT(*) FROM runs other WHERE other.item = s.item) = 1;
"#;

/// Version 7: freeze provenance with each daily charge. Older schemas kept
/// only the latest cumulative basis, which cannot establish historical splits.
const V7: &str = r#"
ALTER TABLE spend_daily ADD COLUMN billed_cost_usd REAL NOT NULL DEFAULT 0
    CHECK (billed_cost_usd >= 0);
ALTER TABLE spend_daily ADD COLUMN list_price_cost_usd REAL NOT NULL DEFAULT 0
    CHECK (list_price_cost_usd >= 0);
ALTER TABLE spend_daily ADD COLUMN unclassified_cost_usd REAL NOT NULL DEFAULT 0
    CHECK (unclassified_cost_usd >= 0);
UPDATE spend_daily SET unclassified_cost_usd = cost_usd;
"#;

/// Version 8: who delegated to whom, so a stage's cost includes the agents
/// beneath its own. Earlier agents have no row: their stages show no split.
const V8: &str = r#"
CREATE TABLE delegated_agents (
    agent TEXT PRIMARY KEY,
    parent TEXT NOT NULL CHECK (parent <> agent),
    model TEXT NOT NULL,
    effort TEXT
);
CREATE INDEX delegated_agents_by_parent ON delegated_agents(parent);
"#;

/// Version 9: the highest total the provider reported for each agent. A
/// charge may fall back to it when an interim estimate is corrected, never below.
const V9: &str = r#"
ALTER TABLE spend ADD COLUMN settled_cost_usd REAL NOT NULL DEFAULT 0
    CHECK (settled_cost_usd >= 0);
UPDATE spend SET settled_cost_usd = cost_usd WHERE cost_basis = 'provider_reported';
"#;

/// Version 10: initiatives, stacked branches, run edits, outcomes and lessons.
/// Every addition is a new table or a nullable column, so nothing is backfilled.
const V10: &str = r#"
CREATE TABLE initiatives (
    id              TEXT PRIMARY KEY,
    state           TEXT NOT NULL,
    idempotency_key TEXT UNIQUE,
    request         TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    data            TEXT NOT NULL
);
CREATE INDEX initiatives_by_state ON initiatives (state, created_at);

ALTER TABLE items ADD COLUMN initiative_id TEXT REFERENCES initiatives (id);
CREATE INDEX items_by_initiative ON items (initiative_id) WHERE initiative_id IS NOT NULL;

CREATE TABLE stacks (
    item          TEXT NOT NULL REFERENCES items (id),
    parent_item   TEXT NOT NULL REFERENCES items (id),
    position      INTEGER NOT NULL CHECK (position >= 0),
    run           TEXT REFERENCES runs (id),
    parent_branch TEXT NOT NULL,
    parent_sha    TEXT NOT NULL,
    state         TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    data          TEXT,
    PRIMARY KEY (item, parent_item),
    UNIQUE (item, position),
    CHECK (item <> parent_item)
) WITHOUT ROWID;
CREATE INDEX stacks_by_parent ON stacks (parent_item, state);

CREATE TABLE run_edits (
    run  TEXT NOT NULL REFERENCES runs (id),
    seq  INTEGER NOT NULL CHECK (seq >= 1),
    at   TEXT NOT NULL,
    edit TEXT NOT NULL CHECK (json_valid(edit)),
    PRIMARY KEY (run, seq)
) WITHOUT ROWID;

CREATE TABLE outcomes (
    run           TEXT PRIMARY KEY REFERENCES runs (id),
    item          TEXT NOT NULL REFERENCES items (id),
    repo          TEXT,
    pipeline      TEXT NOT NULL,
    size          TEXT,
    result        TEXT NOT NULL,
    failure_class TEXT,
    corrected     INTEGER NOT NULL CHECK (corrected IN (0, 1)),
    merged        INTEGER CHECK (merged IN (0, 1)),
    cost_usd      REAL NOT NULL CHECK (cost_usd >= 0),
    finished_at   TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    data          TEXT NOT NULL
);
CREATE INDEX outcomes_by_route ON outcomes (repo, pipeline, finished_at);
CREATE INDEX outcomes_by_item ON outcomes (item);

CREATE TABLE outcome_stages (
    run       TEXT NOT NULL REFERENCES outcomes (run),
    ordinal   INTEGER NOT NULL CHECK (ordinal >= 0),
    stage_id  TEXT NOT NULL,
    model     TEXT NOT NULL,
    effort    TEXT NOT NULL,
    cost_usd  REAL NOT NULL CHECK (cost_usd >= 0),
    wall_secs INTEGER NOT NULL CHECK (wall_secs >= 0),
    attempts  INTEGER NOT NULL CHECK (attempts >= 0),
    passed    INTEGER CHECK (passed IN (0, 1)),
    PRIMARY KEY (run, ordinal)
) WITHOUT ROWID;
CREATE INDEX outcome_stages_by_choice ON outcome_stages (stage_id, model, effort);

CREATE TABLE lessons (
    id            TEXT PRIMARY KEY,
    repo          TEXT,
    pipeline      TEXT,
    kind          TEXT NOT NULL,
    state         TEXT NOT NULL,
    pinned        INTEGER NOT NULL CHECK (pinned IN (0, 1)),
    uses          INTEGER NOT NULL CHECK (uses >= 0),
    wins          INTEGER NOT NULL CHECK (wins >= 0 AND wins <= uses),
    confidence    REAL NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
    source        TEXT NOT NULL,
    superseded_by TEXT REFERENCES lessons (id),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    confirmed_at  TEXT,
    data          TEXT NOT NULL
);
CREATE INDEX lessons_by_scope ON lessons (state, repo, confidence DESC);

CREATE TABLE lesson_uses (
    lesson  TEXT NOT NULL REFERENCES lessons (id),
    run     TEXT NOT NULL REFERENCES runs (id),
    used_at TEXT NOT NULL,
    won     INTEGER CHECK (won IN (0, 1)),
    PRIMARY KEY (lesson, run)
) WITHOUT ROWID;
CREATE INDEX lesson_uses_by_run ON lesson_uses (run);
"#;

/// Version 11: operator corrections of one stage of a finished run. The
/// columns beside `data` are its frozen attribution, so that routing reads
/// them through indexes and never the records.
const V11: &str = r#"
CREATE TABLE operator_feedback (
    id                 TEXT PRIMARY KEY,
    request_key        TEXT NOT NULL CHECK (request_key <> ''),
    item               TEXT NOT NULL REFERENCES items (id),
    run                TEXT NOT NULL REFERENCES runs (id),
    stage              TEXT NOT NULL CHECK (stage <> ''),
    stage_run          TEXT REFERENCES stage_runs (id),
    model              TEXT,
    effort             TEXT,
    repo               TEXT,
    pipeline           TEXT NOT NULL,
    revision           TEXT,
    review             TEXT,
    task_class         TEXT,
    cause              TEXT NOT NULL
                       CHECK (cause IN ('quality', 'scope', 'infrastructure', 'budget')),
    reason             TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 1000),
    observed_at        TEXT NOT NULL,
    revoked_at         TEXT CHECK (revoked_at >= observed_at),
    aether_instance_id TEXT CHECK (aether_instance_id <> ''),
    memory_id          TEXT,
    curation           TEXT CHECK (curation IN ('pending', 'applied', 'failed', 'restored')),
    data               TEXT NOT NULL CHECK (json_valid(data)),
    CHECK ((stage_run IS NULL) = (model IS NULL) AND (model IS NULL) = (effort IS NULL)),
    CHECK ((revision IS NULL) = (review IS NULL) AND (review IS NULL) = (task_class IS NULL)),
    CHECK (stage_run IS NULL OR revision IS NOT NULL),
    CHECK ((memory_id IS NULL) = (curation IS NULL)
           AND (memory_id IS NULL) = (aether_instance_id IS NULL))
);
CREATE UNIQUE INDEX operator_feedback_unrevoked_request ON operator_feedback (request_key)
    WHERE revoked_at IS NULL;
CREATE INDEX operator_feedback_by_run ON operator_feedback (run, stage_run);
CREATE INDEX operator_feedback_by_cohort
    ON operator_feedback
        (repo, pipeline, stage, revision, review, task_class, observed_at, revoked_at)
    WHERE stage_run IS NOT NULL AND cause = 'quality';
"#;

// Existing corrections owe no memory feedback; new ledgers live in their JSON data.
const V12: &str = r#"
UPDATE operator_feedback SET data = json_set(data, '$.memory_feedback', NULL)
WHERE json_type(data, '$.memory_feedback') IS NULL;
"#;

/// Version 13: merges the operator asked for (ARCHITECTURE §18.6). At most
/// one request per item is open; `question` is its confirmation, if any.
const V13: &str = r#"
CREATE TABLE merge_requests (
    id           TEXT PRIMARY KEY,
    item         TEXT NOT NULL REFERENCES items (id),
    run          TEXT NOT NULL REFERENCES runs (id),
    question     TEXT UNIQUE REFERENCES questions (id),
    state        TEXT NOT NULL CHECK (state IN
                     ('awaiting_operator', 'merging', 'merged', 'declined', 'refused')),
    requested_at TEXT NOT NULL,
    updated_at   TEXT NOT NULL CHECK (updated_at >= requested_at),
    data         TEXT NOT NULL CHECK (json_valid(data))
);
CREATE UNIQUE INDEX merge_requests_open_per_item ON merge_requests (item)
    WHERE state IN ('awaiting_operator', 'merging');
CREATE INDEX merge_requests_by_item ON merge_requests (item, requested_at);
"#;

/// Version 14: accepted merges may wait in GitHub’s merge queue before settling.
const V14: &str = r#"
CREATE TABLE merge_requests_next (
    id           TEXT PRIMARY KEY,
    item         TEXT NOT NULL REFERENCES items (id),
    run          TEXT NOT NULL REFERENCES runs (id),
    question     TEXT UNIQUE REFERENCES questions (id),
    state        TEXT NOT NULL CHECK (state IN
                     ('awaiting_operator', 'queued', 'merging', 'merged', 'declined', 'refused')),
    requested_at TEXT NOT NULL,
    updated_at   TEXT NOT NULL CHECK (updated_at >= requested_at),
    data         TEXT NOT NULL CHECK (json_valid(data))
);
INSERT INTO merge_requests_next (id, item, run, question, state, requested_at, updated_at, data)
SELECT id, item, run, question, state, requested_at, updated_at, data FROM merge_requests;
DROP TABLE merge_requests;
ALTER TABLE merge_requests_next RENAME TO merge_requests;
CREATE UNIQUE INDEX merge_requests_open_per_item ON merge_requests (item)
    WHERE state IN ('awaiting_operator', 'queued', 'merging');
CREATE INDEX merge_requests_by_item ON merge_requests (item, requested_at);
"#;

/// Version 15: the pull request each run delivered to, by host, repository and
/// number, from the newest `pr_url` its completed stages hold: GitHub listed each.
/// A settled branch's key moves to its one recorded pull request, or GitHub is asked again.
const V15: &str = r#"
CREATE TABLE run_pull_requests (
    run        TEXT PRIMARY KEY REFERENCES runs (id),
    host       TEXT NOT NULL CHECK (host <> '' AND host = lower(host)),
    repository TEXT NOT NULL CHECK (repository GLOB '?*/?*' AND repository = lower(repository)),
    number     INTEGER NOT NULL CHECK (number > 0),
    node_id    TEXT CHECK (node_id <> '')
);

WITH reported AS (
    SELECT run, json_extract(data, '$.structured.pr_url') AS url,
           ROW_NUMBER() OVER (PARTITION BY run
               ORDER BY started_at DESC, attempt DESC, fan_index DESC, id DESC) AS age
    FROM stage_runs
    WHERE state = 'completed' AND json_type(data, '$.structured.pr_url') = 'text'
), hosted AS (
    SELECT run, substr(url, 9) AS rest FROM reported WHERE age = 1 AND url GLOB 'https://*'
), owned AS (
    SELECT run, substr(rest, 1, instr(rest, '/') - 1) AS host,
           substr(rest, instr(rest, '/') + 1) AS path
    FROM hosted WHERE instr(rest, '/') > 1
), named AS (
    SELECT run, host, substr(path, 1, instr(path, '/') - 1) AS owner,
           substr(path, instr(path, '/') + 1) AS tail
    FROM owned WHERE instr(path, '/') > 1
), numbered AS (
    SELECT run, host, owner, substr(tail, 1, instr(tail, '/pull/') - 1) AS name,
           substr(tail, instr(tail, '/pull/') + 6) AS number
    FROM named WHERE instr(tail, '/pull/') > 1
)
INSERT INTO run_pull_requests (run, host, repository, number)
SELECT run, lower(host), lower(owner || '/' || name), CAST(number AS INTEGER) FROM numbered
WHERE host NOT GLOB '*[^A-Za-z0-9.:-]*'
  AND owner NOT GLOB '*[^A-Za-z0-9._-]*' AND owner NOT IN ('.', '..') AND length(owner) <= 100
  AND name NOT GLOB '*[^A-Za-z0-9._-]*' AND name NOT IN ('.', '..') AND length(name) <= 100
  AND number GLOB '[1-9]*' AND number NOT GLOB '*[^0-9]*' AND length(number) <= 18;

INSERT OR IGNORE INTO kv (key, value)
SELECT 'pr_settled:' || p.host || '/' || p.repository || '#' || p.number, k.value
FROM run_pull_requests p
JOIN runs r ON r.id = p.run
JOIN items i ON i.id = r.item
JOIN kv k ON k.key = 'pr_settled:' || i.repo || ':' || json_extract(r.data, '$.branch')
WHERE k.value IN ('MERGED', 'CLOSED')
GROUP BY k.key
HAVING COUNT(DISTINCT p.host || '/' || p.repository || '#' || p.number) = 1;
DELETE FROM kv WHERE key GLOB 'pr_settled:*' AND key NOT GLOB 'pr_settled:*/*#*';
"#;

pub(super) const MIGRATIONS: &[&str] = &[
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15,
];

/// Pragmas every connection needs. `on_disk` connections must end up in
/// WAL mode; in-memory databases report `memory` and stay that way.
pub(super) fn configure(conn: &Connection, on_disk: bool) -> Result<(), ArchonError> {
    conn.busy_timeout(BUSY_TIMEOUT).ctx("set busy_timeout")?;
    let mode = enable_wal(conn)?;
    if on_disk && !mode.eq_ignore_ascii_case("wal") {
        return Err(ArchonError::Store(format!(
            "journal_mode is {mode}, WAL is required"
        )));
    }
    // FULL: a commit survives power loss, which keeps Aether idempotency keys
    // (persisted before each call) from ever being lost.
    conn.pragma_update(None, "synchronous", "FULL")
        .ctx("set synchronous")?;
    conn.pragma_update(None, "foreign_keys", true)
        .ctx("enable foreign keys")?;
    let fk: bool = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .ctx("read foreign_keys")?;
    if !fk {
        return Err(ArchonError::Store(
            "foreign keys could not be enabled".into(),
        ));
    }
    Ok(())
}

/// The `file:` URI that opens `path` immutable: SQLite then takes no locks
/// and writes nothing beside it. Every byte but unreserved ones and `/` is
/// percent-encoded, so no `?` or `#` in the path is read as URI syntax.
pub(super) fn immutable_uri(path: &Path) -> std::io::Result<String> {
    let path = std::path::absolute(path)?;
    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    debug_assert!(uri.starts_with("file:/"), "the path is absolute");
    Ok(uri)
}

/// A read-only connection waits out the writer and reads a store of
/// exactly this version: it can migrate nothing.
pub(super) fn check_read_only(conn: &Connection) -> Result<(), ArchonError> {
    conn.busy_timeout(BUSY_TIMEOUT).ctx("set busy_timeout")?;
    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .ctx("read schema version")?;
    let expected = MIGRATIONS.len() as i64;
    if version != expected {
        return Err(ArchonError::Store(format!(
            "store schema version {version} is not this archon's ({expected}); start \
             archond of this version once to migrate it, or use the matching archon"
        )));
    }
    assert!(
        conn.is_readonly(rusqlite::MAIN_DB)
            .ctx("read the open mode")?
    );
    Ok(())
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Switching to WAL takes an exclusive lock, and SQLite skips the busy
/// handler when that upgrade could deadlock, so racing first opens see
/// `SQLITE_BUSY` at once. Retry with backoff for up to [`BUSY_TIMEOUT`].
fn enable_wal(conn: &Connection) -> Result<String, ArchonError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let mut backoff = Duration::from_millis(2);
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0)) {
            Ok(mode) => return Ok(mode),
            Err(err) if is_busy(&err) && Instant::now() < deadline => {
                tracing::debug!(?backoff, "journal_mode switch busy; retrying");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
            Err(err) => return Err(err).ctx("enable WAL"),
        }
    }
}

/// Apply every migration newer than the store's version, each with its
/// version row, in one write transaction. Returns the resulting version.
pub(super) fn migrate(conn: &mut Connection) -> Result<usize, ArchonError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             applied_at TEXT NOT NULL
         )",
    )
    .ctx("create schema_migrations")?;
    let tx = write_tx(conn)?;
    let current: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .ctx("read schema version")?;
    let current = usize::try_from(current)
        .map_err(|_| ArchonError::Store(format!("schema version {current} is negative")))?;
    if current > MIGRATIONS.len() {
        return Err(ArchonError::Store(format!(
            "store schema version {current} is newer than this archon supports ({}); \
             upgrade archon",
            MIGRATIONS.len()
        )));
    }
    let now = ts(&archon_core::now());
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let version = index + 1;
        tx.execute_batch(sql)
            .ctx(&format!("apply migration {version}"))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version as i64, now],
        )
        .ctx("record migration")?;
    }
    tx.commit().ctx("commit migrations")?;
    Ok(MIGRATIONS.len())
}

#[cfg(test)]
mod merge_upgrade;
#[cfg(test)]
mod pr_identity;
#[cfg(test)]
mod v1_replay;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const V1_TIME: &str = "2025-01-01T00:00:00.000000000Z";
    const V2_TIME: &str = "2025-06-01T00:00:00.000000000Z";

    fn v1_store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn, false).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
             INSERT INTO schema_migrations VALUES (1, 'then');",
        )
        .unwrap();
        conn.execute_batch(V1).unwrap();
        conn
    }

    fn insert_v1(conn: &Connection, repo: &str, auto_merge: bool, item: &str, pipeline: &str) {
        let repo_data = json!({"id": repo, "name": repo, "auto_merge": auto_merge});
        conn.execute(
            "INSERT INTO repos (id, name, path, added_at, data) VALUES (?1, ?1, ?1, 't', ?2)",
            rusqlite::params![repo, repo_data.to_string()],
        )
        .unwrap();
        let request = json!({"repo": repo, "pipeline": pipeline});
        conn.execute(
            "INSERT INTO items (id, repo, pipeline, priority, state, tags, request, created_at,
                                updated_at, data)
             VALUES (?1, ?2, ?3, 1, 'queued', '[]', ?4, ?6, ?6, ?5)",
            rusqlite::params![
                item,
                repo,
                pipeline,
                request.to_string(),
                json!({"id": item}).to_string(),
                V1_TIME
            ],
        )
        .unwrap();
    }

    fn column(conn: &Connection, sql: &str, id: &str) -> Value {
        let raw: String = conn.query_row(sql, [id], |row| row.get(0)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn v5_preserves_legacy_charges_as_unclassified() {
        let mut conn = v1_store();
        conn.execute(
            "INSERT INTO spend VALUES ('agt_old', NULL, 2.0, 0.5, 1, 1, '2026-10-09T12:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO spend_daily VALUES ('agt_old', '2026-10-09', 2.0, 0.5)",
            [],
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let range = archon_core::cost::DayRange {
            since: day,
            until: day,
        };
        for estimates in [true, false] {
            let report = super::super::cost::report(
                &conn,
                &range,
                archon_core::cost::CostGroup::Day,
                estimates,
            )
            .unwrap();
            assert_eq!(report.total.counted_usd, if estimates { 2.5 } else { 2.0 });
            assert_eq!(report.total.cost_usd, 0.0);
            assert_eq!(report.total.estimated_cost_usd, 0.5);
            assert_eq!(report.total.unclassified_cost_usd, 2.0);
        }
        let basis: String = conn
            .query_row("SELECT cost_basis FROM spend", [], |r| r.get(0))
            .unwrap();
        assert_eq!(basis, "unknown");
        migrate(&mut conn).unwrap();
        let total: f64 = conn
            .query_row("SELECT cost_usd FROM spend", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2.0);
    }

    #[test]
    fn v2_moves_auto_merge_into_the_ceiling_and_gives_items_knobs() {
        let mut conn = v1_store();
        insert_v1(&conn, "repo_a", true, "qi_a", "strict-pr-review");
        insert_v1(&conn, "repo_b", false, "qi_b", "quick-pr");
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        let a = column(&conn, "SELECT data FROM repos WHERE id = ?1", "repo_a");
        assert_eq!(a["autonomy_max"], json!({"merge": "auto"}));
        assert!(a.get("auto_merge").is_none(), "{a}");
        let b = column(&conn, "SELECT data FROM repos WHERE id = ?1", "repo_b");
        assert_eq!(b["autonomy_max"], json!({}));
        let item = column(&conn, "SELECT data FROM items WHERE id = ?1", "qi_a");
        assert_eq!(item["autonomy"]["merge"], "auto");
        assert_eq!(item["autonomy"]["review"], "strict");
        let quick = column(&conn, "SELECT data FROM items WHERE id = ?1", "qi_b");
        assert_eq!(quick["autonomy"]["review"], "none");
        assert_eq!(quick["autonomy"]["merge"], "manual");
        let request = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_b");
        assert_eq!(request, json!({"repo": "repo_b", "pipeline": "quick-pr"}));
        let decoded: archon_core::autonomy::Autonomy =
            serde_json::from_value(item["autonomy"].clone()).unwrap();
        assert_eq!(decoded.merge, archon_core::autonomy::MergeMode::Auto);
    }

    /// A store at the released V2 schema: `qi_old` was enqueued under V1
    /// and backfilled by V2, which was applied at `V2_TIME`.
    fn v2_store() -> Connection {
        let conn = v1_store();
        insert_v1(&conn, "repo_a", false, "qi_old", "ask");
        conn.execute_batch(V2).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO schema_migrations VALUES (2, '{V2_TIME}');"
        ))
        .unwrap();
        conn
    }

    /// Records an item enqueued after V2, whose fingerprint carries knobs.
    fn insert_v2_item(conn: &Connection, item: &str) -> Value {
        let request = json!({"pipeline": "ask", "autonomy": {"review": "none", "merge": "manual"}});
        conn.execute(
            "INSERT INTO items (id, repo, pipeline, priority, state, tags, request, created_at,
                                updated_at, data)
             VALUES (?1, 'repo_a', 'ask', 1, 'queued', '[]', ?2,
                     '2025-07-01T00:00:00.000000000Z', '2025-07-01T00:00:00.000000000Z', '{}')",
            rusqlite::params![item, request.to_string()],
        )
        .unwrap();
        request
    }

    fn insert_question(conn: &Connection, id: &str, open: bool, asked_at: &str) {
        conn.execute(
            "INSERT INTO questions (id, open, asked_at, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, open, asked_at, json!({"id": id}).to_string()],
        )
        .unwrap();
    }

    fn question_number(conn: &Connection, id: &str) -> i64 {
        let data = column(conn, "SELECT data FROM questions WHERE id = ?1", id);
        let stored: i64 = conn
            .query_row("SELECT number FROM questions WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(data["number"], stored, "{id}: record and column agree");
        stored
    }

    fn applied_versions(conn: &Connection) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare("SELECT version, applied_at FROM schema_migrations ORDER BY version")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// A store already at V2 keeps the fingerprints written since V2.
    #[test]
    fn v3_restores_the_v1_fingerprint_of_items_from_before_v2_only() {
        let mut conn = v2_store();
        let request = insert_v2_item(&conn, "qi_new");
        let old = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_old");
        assert_eq!(old["autonomy"]["review"], "standard", "V2 backfilled it");

        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        let old = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_old");
        assert_eq!(old, json!({"repo": "repo_a", "pipeline": "ask"}));
        let new = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_new");
        assert_eq!(new, request, "a fingerprint written since V2 is kept");
    }

    #[test]
    fn v4_numbers_recorded_questions_in_asking_order() {
        let mut conn = v1_store();
        insert_question(&conn, "q_b", true, "2026-01-02T00:00:00Z");
        insert_question(&conn, "q_a", true, "2026-01-01T00:00:00Z");
        insert_question(&conn, "q_c", true, "2026-01-02T00:00:00Z");
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        for (id, number) in [("q_a", 1), ("q_b", 2), ("q_c", 3)] {
            assert_eq!(question_number(&conn, id), number, "{id}");
        }
        let duplicate = conn.execute("UPDATE questions SET number = 1 WHERE id = 'q_b'", []);
        assert!(duplicate.is_err(), "numbers are unique");
    }

    /// The released 1.0.0 store is at V2; opening it applies every later
    /// migration once, in order, each seeing the state the previous left.
    #[test]
    fn a_released_v2_store_upgrades_through_every_later_migration() {
        let mut conn = v2_store();
        let request = insert_v2_item(&conn, "qi_new");
        insert_question(&conn, "q_open", true, "2025-08-02T00:00:00Z");
        insert_question(&conn, "q_closed", false, "2025-08-01T00:00:00Z");

        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        let applied = applied_versions(&conn);
        let versions: Vec<i64> = applied.iter().map(|(v, _)| *v).collect();
        assert_eq!(versions, (1..=MIGRATIONS.len() as i64).collect::<Vec<_>>());
        assert_eq!(applied[1].1, V2_TIME, "V2's record is left alone");
        let old = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_old");
        assert_eq!(old, json!({"repo": "repo_a", "pipeline": "ask"}), "V3 ran");
        let new = column(&conn, "SELECT request FROM items WHERE id = ?1", "qi_new");
        assert_eq!(new, request);
        assert_eq!(question_number(&conn, "q_closed"), 1, "V4 ran");
        assert_eq!(question_number(&conn, "q_open"), 2);

        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        assert_eq!(applied_versions(&conn), applied, "a reopen applies nothing");
        assert_eq!(question_number(&conn, "q_open"), 2);
    }

    fn insert_run(conn: &Connection, run: &str, item: &str) {
        conn.execute(
            "INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
             VALUES (?1, ?2, 'ask', 1, 't', '{}')",
            [run, item],
        )
        .unwrap();
    }

    /// Statements V10's tables must take, in dependency order.
    const V10_ROWS: &str = "
        INSERT INTO initiatives (id, state, created_at, updated_at, data)
            VALUES ('ini_a', 'open', 't', 't', '{}');
        UPDATE items SET initiative_id = 'ini_a' WHERE id = 'qi_new';
        INSERT INTO stacks (item, parent_item, position, run, parent_branch, parent_sha, state,
                            updated_at)
            VALUES ('qi_new', 'qi_old', 0, 'prun_new', 'archon/old', 'abc123', 'stacked', 't');
        INSERT INTO run_edits (run, seq, at, edit) VALUES ('prun_new', 1, 't', '{}');
        INSERT INTO outcomes (run, item, repo, pipeline, result, corrected, cost_usd, finished_at,
                              updated_at, data)
            VALUES ('prun_new', 'qi_new', 'repo_a', 'ask', 'succeeded', 0, 0.5, 't', 't', '{}');
        INSERT INTO outcome_stages (run, ordinal, stage_id, model, effort, cost_usd, wall_secs,
                                    attempts)
            VALUES ('prun_new', 0, 'answer', 'codex/astra', 'high', 0.5, 90, 1);
        INSERT INTO lessons (id, kind, state, pinned, uses, wins, confidence, source, created_at,
                             updated_at, data)
            VALUES ('lsn_a', 'pitfall', 'active', 0, 2, 1, 0.4, 'operator', 't', 't', '{}');
        INSERT INTO lesson_uses (lesson, run, used_at) VALUES ('lsn_a', 'prun_new', 't');";

    /// Rows V10 must refuse, each for the reason named.
    const V10_REFUSED: [(&str, &str); 8] = [
        (
            "an item in no recorded initiative",
            "UPDATE items SET initiative_id = 'ini_none' WHERE id = 'qi_old'",
        ),
        (
            "an item stacked on itself",
            "INSERT INTO stacks VALUES ('qi_old', 'qi_old', 0, NULL, 'b', 's', 'stacked', 't', NULL)",
        ),
        (
            "two parents in one position",
            "INSERT INTO stacks VALUES ('qi_new', 'qi_new2', 0, NULL, 'b', 's', 'stacked', 't', NULL)",
        ),
        (
            "an edit that is not JSON",
            "INSERT INTO run_edits VALUES ('prun_new', 2, 't', 'not json')",
        ),
        (
            "a second edit with one sequence number",
            "INSERT INTO run_edits VALUES ('prun_new', 1, 't', '{}')",
        ),
        (
            "a second outcome for one run",
            "INSERT INTO outcomes (run, item, pipeline, result, corrected, cost_usd, finished_at,
                                   updated_at, data)
             VALUES ('prun_new', 'qi_new', 'ask', 'failed', 1, 0, 't', 't', '{}')",
        ),
        (
            "more wins than uses",
            "UPDATE lessons SET wins = 3 WHERE id = 'lsn_a'",
        ),
        (
            "a use by an unrecorded run",
            "INSERT INTO lesson_uses (lesson, run, used_at) VALUES ('lsn_a', 'prun_none', 't')",
        ),
    ];

    #[test]
    fn v10_adds_the_adaptive_tables_and_leaves_earlier_items_outside_initiatives() {
        let mut conn = v2_store();
        insert_v2_item(&conn, "qi_new");
        insert_v2_item(&conn, "qi_new2");
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        let unassigned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE initiative_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unassigned, 3, "no earlier item joins an initiative");

        insert_run(&conn, "prun_new", "qi_new");
        conn.execute_batch(V10_ROWS).unwrap();
        for (why, sql) in V10_REFUSED {
            assert!(conn.execute_batch(sql).is_err(), "V10 must refuse {why}");
        }
        let members: Vec<String> = conn
            .prepare("SELECT id FROM items WHERE initiative_id = 'ini_a'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(members, ["qi_new"]);
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    }

    fn insert_feedback(conn: &Connection, id: &str, values: &str) -> rusqlite::Result<()> {
        conn.execute_batch(&format!(
            "INSERT INTO operator_feedback (id, request_key, item, run, stage, stage_run, model,
                 effort, repo, pipeline, revision, review, task_class, cause, reason,
                 observed_at, revoked_at, aether_instance_id, memory_id, curation, data)
             VALUES ('{id}', {values})"
        ))
    }

    /// Columns after the id of a valid attributed quality correction.
    const V11_ROW: &str = "'k1', 'qi_new', 'prun_new', 'answer', 'stg_a', 'codex/astra', 'high',
        'repo_a', 'ask', 'rev', 'strict', 'general', 'quality', 'wrong file', 't1', NULL,
        'aether-1', 'mem_a', 'pending', '{}'";

    /// Rows V11 must refuse, each under its own request key so that only
    /// the named defect can refuse it.
    fn refused_v11_rows() -> [(&'static str, String); 10] {
        [
            ("a replayed unrevoked request", V11_ROW.to_owned()),
            (
                "a partial attribution",
                V11_ROW.replace("'high'", "NULL").replace("'k1'", "'k2'"),
            ),
            (
                "an unknown cause",
                V11_ROW
                    .replace("'quality'", "'taste'")
                    .replace("'k1'", "'k3'"),
            ),
            (
                "an empty reason",
                V11_ROW
                    .replace("'wrong file'", "''")
                    .replace("'k1'", "'k4'"),
            ),
            (
                "a memory without a curation state",
                V11_ROW.replace("'pending'", "NULL").replace("'k1'", "'k5'"),
            ),
            (
                "an unrecorded stage run",
                V11_ROW
                    .replace("'stg_a'", "'stg_none'")
                    .replace("'k1'", "'k6'"),
            ),
            (
                "data that is not JSON",
                V11_ROW.replace("'{}'", "'nope'").replace("'k1'", "'k7'"),
            ),
            (
                "a memory forget bound to no Aether instance",
                V11_ROW
                    .replace("'aether-1'", "NULL")
                    .replace("'k1'", "'k8'"),
            ),
            (
                "a partial routing context",
                V11_ROW.replace("'strict'", "NULL").replace("'k1'", "'k9'"),
            ),
            (
                "a model credit without its routing context",
                V11_ROW
                    .replace("'rev', 'strict', 'general'", "NULL, NULL, NULL")
                    .replace("'k1'", "'k10'"),
            ),
        ]
    }

    #[test]
    fn v11_keeps_one_unrevoked_correction_per_request_and_whole_attributions() {
        let mut conn = v2_store();
        insert_v2_item(&conn, "qi_new");
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        insert_run(&conn, "prun_new", "qi_new");
        conn.execute_batch(
            "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, state, terminal,
                 started_at, data)
             VALUES ('stg_a', 'prun_new', 'answer', 1, 0, 'completed', 1, 't', '{}')",
        )
        .unwrap();
        insert_feedback(&conn, "fb_a", V11_ROW).unwrap();
        for (why, values) in &refused_v11_rows() {
            assert!(
                insert_feedback(&conn, "fb_x", values).is_err(),
                "V11 must refuse {why}"
            );
        }
        conn.execute_batch("UPDATE operator_feedback SET revoked_at = 't2' WHERE id = 'fb_a'")
            .unwrap();
        insert_feedback(&conn, "fb_b", V11_ROW).expect("a revoked request may be made again");
        assert!(
            conn.execute_batch("UPDATE operator_feedback SET revoked_at = 't0' WHERE id = 'fb_b'")
                .is_err(),
            "a correction is not revoked before it was made"
        );
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn v12_leaves_old_corrections_without_retroactive_memory_feedback() {
        let mut conn = v2_store();
        insert_v2_item(&conn, "qi_new");
        migrate(&mut conn).unwrap();
        insert_run(&conn, "prun_new", "qi_new");
        conn.execute_batch(
            "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, state, terminal,
                 started_at, data)
             VALUES ('stg_a', 'prun_new', 'answer', 1, 0, 'completed', 1, 't', '{}')",
        )
        .unwrap();
        insert_feedback(&conn, "fb_old", V11_ROW).unwrap();
        conn.execute_batch(
            "DROP TABLE run_pull_requests; DROP TABLE merge_requests;
             DELETE FROM schema_migrations WHERE version >= 12",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
        let (data, reason): (String, String) = conn
            .query_row(
                "SELECT data, reason FROM operator_feedback WHERE id = 'fb_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(data, r#"{"memory_feedback":null}"#);
        assert_eq!(reason, "wrong file");
        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());
    }
}
