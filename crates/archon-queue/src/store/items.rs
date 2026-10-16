//! Queue items, their dependencies and their state transitions.

use std::collections::BTreeSet;

use archon_core::Timestamp;
use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::protocol::ItemQuery;
use archon_core::queue::{QueueItem, QueueState};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::codec::{SqlContext, from_json, priority_rank, sql_limit, to_json, ts, write_tx};
use super::repos;

pub(super) const DEFAULT_LIST_LIMIT: usize = 50;

/// The fields an enqueue request decides. Two enqueues with the same
/// idempotency key are "the same request" when these agree. `origin` only
/// when set, so fingerprints stored before it existed still match.
fn request_value(item: &QueueItem) -> serde_json::Value {
    let mut value = serde_json::json!({
        "repo": item.repo,
        "pipeline": item.pipeline,
        "title": item.title,
        "brief": item.brief,
        "priority": item.priority,
        "dependencies": item.dependencies,
        "tags": item.tags,
        "model_plan": item.model_plan,
        "budget_usd": item.budget_usd,
        "autonomy": item.autonomy,
    });
    // Omit empty overrides so fingerprints written before this field still match.
    if !item.stage_timeouts.is_empty() {
        value["stage_timeouts"] = serde_json::json!(item.stage_timeouts);
    }
    if let Some(origin) = &item.origin {
        value["origin"] = serde_json::json!(origin);
    }
    if let Some(reasons) = &item.model_reasons {
        value["model_reasons"] = serde_json::json!(reasons);
    }
    // The target as asked: what it resolved to may move between retries.
    if let Some(onto) = &item.onto {
        value["onto"] = serde_json::json!(onto.target);
    }
    value
}

pub(super) fn load(conn: &Connection, id: &QueueItemId) -> Result<QueueItem, ArchonError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM items WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read item")?;
    let data = data.ok_or_else(|| ArchonError::NotFound(format!("queue item {id}")))?;
    let item: QueueItem = from_json(&data, "queue item")?;
    debug_assert_eq!(&item.id, id, "item row {id} holds another item");
    Ok(item)
}

fn exists(conn: &Connection, id: &QueueItemId) -> Result<bool, ArchonError> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM items WHERE id = ?1)",
        [id.as_str()],
        |row| row.get(0),
    )
    .ctx("look up item")
}

/// Dependencies are distinct, exist, and adding them keeps the graph
/// acyclic: `id` must not be reachable from any of them.
fn check_dependencies(
    conn: &Connection,
    id: &QueueItemId,
    deps: &[QueueItemId],
) -> Result<(), ArchonError> {
    let mut seen = BTreeSet::new();
    for dep in deps {
        if dep == id {
            return Err(ArchonError::Invalid(format!(
                "item {id} cannot depend on itself"
            )));
        }
        if !seen.insert(dep) {
            return Err(ArchonError::Invalid(format!(
                "dependency {dep} is listed twice"
            )));
        }
        if !exists(conn, dep)? {
            return Err(ArchonError::NotFound(format!("dependency {dep}")));
        }
        let cycle: bool = conn
            .query_row(
                "WITH RECURSIVE reach (id) AS (
                     SELECT ?1
                     UNION
                     SELECT d.depends_on FROM item_deps d JOIN reach r ON d.item = r.id
                 )
                 SELECT EXISTS (SELECT 1 FROM reach WHERE id = ?2)",
                params![dep.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .ctx("check dependency cycle")?;
        if cycle {
            return Err(ArchonError::Invalid(format!(
                "depending on {dep} would create a cycle: {dep} already depends on {id}"
            )));
        }
    }
    Ok(())
}

fn replace_deps(conn: &Connection, item: &QueueItem) -> Result<(), ArchonError> {
    conn.execute("DELETE FROM item_deps WHERE item = ?1", [item.id.as_str()])
        .ctx("clear dependencies")?;
    let mut stmt = conn
        .prepare_cached("INSERT INTO item_deps (item, depends_on) VALUES (?1, ?2)")
        .ctx("prepare dependency insert")?;
    for dep in &item.dependencies {
        stmt.execute(params![item.id.as_str(), dep.as_str()])
            .ctx("insert dependency")?;
    }
    Ok(())
}

/// Rewrites every derived column from `item`, so columns and `data` agree.
fn write_columns(conn: &Connection, item: &QueueItem) -> Result<(), ArchonError> {
    let changed = conn
        .execute(
            "UPDATE items SET repo = ?2, pipeline = ?3, priority = ?4, state = ?5, tags = ?6,
                              created_at = ?7, updated_at = ?8, data = ?9, initiative_id = ?10
             WHERE id = ?1",
            params![
                item.id.as_str(),
                item.repo.as_ref().map(|r| r.as_str()),
                item.pipeline,
                priority_rank(item.priority),
                item.state.kind(),
                to_json(&item.tags, "tags")?,
                ts(&item.created_at),
                ts(&item.updated_at),
                to_json(item, "queue item")?,
                item.initiative.as_ref().map(|i| i.as_str()),
            ],
        )
        .ctx("write item")?;
    if changed != 1 {
        return Err(ArchonError::NotFound(format!("queue item {}", item.id)));
    }
    Ok(())
}

fn find_by_key(conn: &Connection, key: &str) -> Result<Option<(QueueItemId, String)>, ArchonError> {
    conn.query_row(
        "SELECT id, request FROM items WHERE idempotency_key = ?1",
        [key],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .ctx("look up idempotency key")?
    .map(|(id, request)| Ok((QueueItemId::parse(&id)?, request)))
    .transpose()
}

fn validate_new(item: &QueueItem, key: Option<&str>) -> Result<(), ArchonError> {
    if item.state != QueueState::Queued {
        return Err(ArchonError::Invalid(format!(
            "a new item must be queued, not {}",
            item.state.kind()
        )));
    }
    if item.pipeline.trim().is_empty() || item.title.trim().is_empty() {
        return Err(ArchonError::Invalid(
            "an item needs a pipeline and a title".into(),
        ));
    }
    if key.is_some_and(|k| k.trim().is_empty()) {
        return Err(ArchonError::Invalid("idempotency key is empty".into()));
    }
    Ok(())
}

/// Refuse `key` reused for a different request. A fingerprint without
/// `autonomy` predates the knobs (store migration 3): it matches a retry
/// that asks for no knob and agrees on every other field.
fn check_replay(
    key: &str,
    first: &QueueItemId,
    stored: &serde_json::Value,
    mut request: serde_json::Value,
    asked: &AutonomyPatch,
) -> Result<(), ArchonError> {
    let Some(stored_fields) = stored.as_object() else {
        return Err(ArchonError::Store(format!(
            "enqueue request of {first} is not a JSON object"
        )));
    };
    let fields = request
        .as_object_mut()
        .expect("request_value builds an object");
    let legacy = !stored_fields.contains_key("autonomy");
    if legacy {
        assert!(
            fields.remove("autonomy").is_some(),
            "request_value has autonomy"
        );
    }
    if *stored != request {
        return Err(ArchonError::Invalid(format!(
            "idempotency key {key:?} was already used for a different request ({first})"
        )));
    }
    if legacy && !asked.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "idempotency key {key:?} was used before autonomy knobs existed ({first}); \
             a retry of that request asks for none, not {}",
            asked.describe()
        )));
    }
    Ok(())
}

/// `asked` is the knobs the enqueue named, before resolution.
pub(super) fn insert(
    conn: &mut Connection,
    item: &QueueItem,
    key: Option<&str>,
    asked: &AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    let tx = write_tx(conn)?;
    let stored = insert_in(&tx, item, key, asked)?;
    tx.commit().ctx("commit item")?;
    Ok(stored)
}

pub(super) fn insert_in(
    tx: &Connection,
    item: &QueueItem,
    key: Option<&str>,
    asked: &AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    validate_new(item, key)?;
    assert_eq!(item.state, QueueState::Queued);
    assert!(
        !tx.is_autocommit(),
        "item insertion belongs to a transaction"
    );
    let request = request_value(item);
    if let Some(key) = key
        && let Some((first, stored)) = find_by_key(tx, key)?
    {
        let stored: serde_json::Value = from_json(&stored, "enqueue request")?;
        check_replay(key, &first, &stored, request, asked)?;
        return load(tx, &first);
    }
    if exists(tx, &item.id)? {
        return Err(ArchonError::Invalid(format!(
            "queue item {} already exists",
            item.id
        )));
    }
    if let Some(repo) = &item.repo
        && !repos::exists(tx, repo)?
    {
        return Err(ArchonError::NotFound(format!("repository {repo}")));
    }
    check_dependencies(tx, &item.id, &item.dependencies)?;
    tx.execute(
        "INSERT INTO items (id, repo, pipeline, priority, state, tags, idempotency_key,
                            request, created_at, updated_at, data, initiative_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            item.id.as_str(),
            item.repo.as_ref().map(|r| r.as_str()),
            item.pipeline,
            priority_rank(item.priority),
            item.state.kind(),
            to_json(&item.tags, "tags")?,
            key,
            to_json(&request, "enqueue request")?,
            ts(&item.created_at),
            ts(&item.updated_at),
            to_json(item, "queue item")?,
            item.initiative.as_ref().map(|i| i.as_str()),
        ],
    )
    .ctx("insert item")?;
    replace_deps(tx, item)?;
    Ok(item.clone())
}

/// `Running` names the item's live run; `Succeeded` and `Failed` name one
/// of its runs. Re-queueing and settling require that no run of the item is
/// still live: a live run is finished together with its item by
/// `runs::finish`, so an item never outlives or strands its run.
fn check_run_refs(
    conn: &Connection,
    id: &QueueItemId,
    next: &QueueState,
) -> Result<(), ArchonError> {
    let live = super::runs::active_run_of(conn, id)?;
    let named: Option<&PipelineRunId> = match next {
        QueueState::Running { run } => {
            if live.as_ref() != Some(run) {
                return Err(ArchonError::Invalid(format!(
                    "run {run} is not the live run of item {id}"
                )));
            }
            None
        }
        QueueState::Succeeded { run, .. } => Some(run),
        QueueState::Failed { run, .. } => run.as_ref(),
        QueueState::Queued => {
            if let Some(live) = live {
                return Err(ArchonError::InvalidTransition(format!(
                    "item {id} cannot be re-queued while its run {live} is still active"
                )));
            }
            None
        }
        _ => None,
    };
    if next.is_terminal()
        && let Some(live) = live
    {
        return Err(ArchonError::InvalidTransition(format!(
            "item {id} cannot become {} while its run {live} is still active; \
             finish the run with finish_run",
            next.kind()
        )));
    }
    let Some(run) = named else {
        return Ok(());
    };
    let owner: Option<String> = conn
        .query_row(
            "SELECT item FROM runs WHERE id = ?1",
            [run.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("look up run")?;
    if owner.as_deref() != Some(id.as_str()) {
        return Err(ArchonError::Invalid(format!(
            "run {run} is not a run of item {id}"
        )));
    }
    Ok(())
}

pub(super) fn transition(
    conn: &mut Connection,
    id: &QueueItemId,
    next: QueueState,
    at: Timestamp,
) -> Result<QueueItem, ArchonError> {
    let tx = write_tx(conn)?;
    let item = transition_in(&tx, id, next, at)?;
    tx.commit().ctx("commit item transition")?;
    Ok(item)
}

/// The checked transition inside the caller's write transaction.
pub(super) fn transition_in(
    tx: &Connection,
    id: &QueueItemId,
    next: QueueState,
    at: Timestamp,
) -> Result<QueueItem, ArchonError> {
    let mut item = load(tx, id)?;
    let before = item.state.clone();
    // Edge check first, in memory. A wall clock stepping back must not make
    // `updated_at` go backwards.
    item.transition(next, at.max(item.updated_at))?;
    debug_assert!(before.can_transition_to(&item.state));
    check_run_refs(tx, id, &item.state)?;
    write_columns(tx, &item)?;
    if let QueueState::Succeeded { run, .. } = &before
        && item.state == QueueState::Queued
    {
        super::outcomes::correct(tx, run, archon_core::outcome::Correction::RetryAfterDone)?;
    }
    Ok(item)
}

/// Everything but the state (which goes through [`transition`]) and the
/// identity fields may change. Changed dependencies are re-validated.
pub(super) fn update(conn: &mut Connection, item: &QueueItem) -> Result<(), ArchonError> {
    update_with_kv(conn, item, &[])
}

pub(super) fn update_with_kv(
    conn: &mut Connection,
    item: &QueueItem,
    updates: &[(&str, &str)],
) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    let stored = load(&tx, &item.id)?;
    if stored.state != item.state {
        return Err(ArchonError::InvalidTransition(format!(
            "update_item cannot move {} from {} to {}; use transition_item",
            item.id,
            stored.state.kind(),
            item.state.kind()
        )));
    }
    if stored.initiative != item.initiative
        || stored.repo != item.repo
        || stored.pipeline != item.pipeline
        || stored.created_at != item.created_at
    {
        return Err(ArchonError::Invalid(format!(
            "the repository, pipeline and creation time of {} cannot change",
            item.id
        )));
    }
    if item.title.trim().is_empty() {
        return Err(ArchonError::Invalid("an item needs a title".into()));
    }
    let deps_changed = stored.dependencies != item.dependencies;
    if deps_changed {
        check_dependencies(&tx, &item.id, &item.dependencies)?;
    }
    write_columns(&tx, item)?;
    if deps_changed {
        replace_deps(&tx, item)?;
    }
    for (key, value) in updates {
        super::settings::kv_set(&tx, key, value)?;
    }
    tx.commit().ctx("commit item update")
}

pub(super) fn dependents(
    conn: &Connection,
    id: &QueueItemId,
) -> Result<Vec<QueueItemId>, ArchonError> {
    if !exists(conn, id)? {
        return Err(ArchonError::NotFound(format!("queue item {id}")));
    }
    let mut stmt = conn
        .prepare_cached(
            "SELECT d.item FROM item_deps d JOIN items i ON i.id = d.item
             WHERE d.depends_on = ?1 ORDER BY i.seq",
        )
        .ctx("list dependents")?;
    let rows = stmt
        .query_map([id.as_str()], |row| row.get::<_, String>(0))
        .ctx("list dependents")?;
    let mut out = Vec::new();
    for raw in rows {
        out.push(QueueItemId::parse(&raw.ctx("read dependent")?)?);
    }
    Ok(out)
}

/// The repositories of items in state `kind`, each once; `None` stands for
/// items without one.
pub(super) fn repos_in_state(
    conn: &Connection,
    kind: &str,
) -> Result<Vec<Option<RepoId>>, ArchonError> {
    assert!(
        QueueState::KINDS.contains(&kind),
        "{kind:?} is not a queue state"
    );
    let mut stmt = conn
        .prepare_cached("SELECT DISTINCT repo FROM items WHERE state = ?1 ORDER BY repo")
        .ctx("list item repositories")?;
    let rows = stmt
        .query_map([kind], |row| row.get::<_, Option<String>>(0))
        .ctx("list item repositories")?;
    let mut out = Vec::new();
    for raw in rows {
        let raw = raw.ctx("read item repository")?;
        out.push(raw.as_deref().map(RepoId::parse).transpose()?);
    }
    debug_assert!(out.windows(2).all(|w| w[0] != w[1]), "distinct");
    Ok(out)
}

/// Crash recovery (ARCHITECTURE §16): `dispatching` items that have no live
/// run go back to `queued`. This edge exists only here, never in
/// `QueueState::can_transition_to`, because only a restart may take it.
pub(super) fn requeue_orphaned_dispatching(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<Vec<QueueItemId>, ArchonError> {
    let tx = write_tx(conn)?;
    let ids: Vec<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT id FROM items i WHERE state = 'dispatching'
                 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.item = i.id AND r.terminal = 0)
                 ORDER BY seq",
            )
            .ctx("find orphaned dispatching items")?;
        let rows = stmt.query_map([], |row| row.get(0)).ctx("read orphans")?;
        rows.collect::<rusqlite::Result<_>>().ctx("read orphans")?
    };
    let mut requeued = Vec::with_capacity(ids.len());
    for raw in ids {
        let mut item = load(&tx, &QueueItemId::parse(&raw)?)?;
        assert_eq!(
            item.state,
            QueueState::Dispatching,
            "query returned {}",
            item.id
        );
        item.state = QueueState::Queued;
        item.updated_at = at.max(item.updated_at);
        write_columns(&tx, &item)?;
        requeued.push(item.id);
    }
    tx.commit().ctx("commit recovery")?;
    Ok(requeued)
}

fn query_filters(
    conn: &Connection,
    query: &ItemQuery,
) -> Result<(String, Vec<Value>), ArchonError> {
    let mut clauses = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    if !query.states.is_empty() {
        for state in &query.states {
            if !QueueState::KINDS.contains(&state.as_str()) {
                return Err(ArchonError::Invalid(format!(
                    "unknown state {state:?}; expected one of {}",
                    QueueState::KINDS.join(", ")
                )));
            }
            args.push(Value::Text(state.clone()));
        }
        let marks = vec!["?"; query.states.len()].join(", ");
        clauses.push(format!("state IN ({marks})"));
    }
    if let Some(reference) = &query.repo {
        let repo = repos::find(conn, reference)?;
        clauses.push("repo = ?".to_owned());
        args.push(Value::Text(repo.id.as_str().to_owned()));
    }
    if let Some(pipeline) = &query.pipeline {
        clauses.push("pipeline = ?".to_owned());
        args.push(Value::Text(pipeline.clone()));
    }
    if let Some(tag) = &query.tag {
        clauses.push("EXISTS (SELECT 1 FROM json_each(items.tags) WHERE value = ?)".to_owned());
        args.push(Value::Text(tag.clone()));
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    Ok((filter, args))
}

/// Newest first; `limit` defaults to [`DEFAULT_LIST_LIMIT`].
pub(super) fn list(conn: &Connection, query: &ItemQuery) -> Result<Vec<QueueItem>, ArchonError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 {
        return Err(ArchonError::Invalid("limit must be at least 1".into()));
    }
    let (filter, mut args) = query_filters(conn, query)?;
    args.push(Value::Integer(sql_limit(limit)));
    let sql = format!("SELECT data FROM items {filter} ORDER BY seq DESC LIMIT ?");
    let mut stmt = conn.prepare(&sql).ctx("list items")?;
    let rows = stmt
        .query_map(params_from_iter(args), |row| row.get::<_, String>(0))
        .ctx("list items")?;
    let mut items = Vec::new();
    for data in rows {
        items.push(from_json(&data.ctx("read item")?, "queue item")?);
    }
    debug_assert!(items.len() <= limit);
    Ok(items)
}

/// Items whose id starts with `prefix` (`qi_` and upper-case ULID
/// characters, as [`archon_core::id::queue_item_prefix`] makes it), oldest
/// first, at most `limit`.
pub(super) fn with_id_prefix(
    conn: &Connection,
    prefix: &str,
    limit: usize,
) -> Result<Vec<QueueItem>, ArchonError> {
    assert!(limit > 0, "a prefix lookup returns something");
    assert!(
        prefix.starts_with("qi_") && prefix[3..].bytes().all(|b| b.is_ascii_alphanumeric()),
        "prefix {prefix:?} is checked before the lookup"
    );
    // '~' sorts after every id character, so this range is exactly the ids
    // that start with `prefix`, and the id index serves it.
    let end = format!("{prefix}~");
    let mut stmt = conn
        .prepare("SELECT data FROM items WHERE id >= ?1 AND id < ?2 ORDER BY id LIMIT ?3")
        .ctx("find items by id prefix")?;
    let rows = stmt
        .query_map(params![prefix, end, sql_limit(limit)], |row| {
            row.get::<_, String>(0)
        })
        .ctx("find items by id prefix")?;
    let mut items: Vec<QueueItem> = Vec::new();
    for data in rows {
        items.push(from_json(&data.ctx("read item")?, "queue item")?);
    }
    debug_assert!(items.iter().all(|i| i.id.as_str().starts_with(prefix)));
    Ok(items)
}
