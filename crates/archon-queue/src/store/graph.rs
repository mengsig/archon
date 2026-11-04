//! The `task_graph` projection (CONTRACTS.md §1.3, owner AR-G): items as
//! nodes with their typed edges and initiatives, read in one transaction
//! together with the change seq they are `as_of`.

use std::collections::{BTreeMap, HashSet};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, PipelineRunId, QueueItemId, RepoId};
use archon_core::initiative::Initiative;
use archon_core::protocol::graph::{
    GraphInitiative, MAX_TASK_GRAPH_LIMIT, TASK_GRAPH_LIMIT, TaskEdgeKind,
};
use archon_core::protocol::{
    GithubRef, PrRef, QueueStateKind, TaskEdge, TaskFilter, TaskGraph, TaskNode, WaitingReasonCode,
};
use archon_core::queue::{Priority, QueueState};
use archon_core::run::PipelineRun;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};

use super::Store;
use super::codec::{SqlContext, end_read_tx, from_json, priority_rank, read_tx, sql_limit, ts};

/// The connected component of `?1` over every edge kind, both directions.
const COMPONENT_CTE: &str = "WITH RECURSIVE component (id) AS (
         SELECT ?1
         UNION SELECT d.depends_on FROM item_deps d JOIN component c ON d.item = c.id
         UNION SELECT d.item FROM item_deps d JOIN component c ON d.depends_on = c.id
         UNION SELECT s.parent_item FROM stacks s JOIN component c ON s.item = c.id
         UNION SELECT s.item FROM stacks s JOIN component c ON s.parent_item = c.id
     ) ";

const TERMINAL: &str = "('succeeded', 'failed', 'cancelled')";

impl Store {
    /// The items `filter` selects, at most `limit` (default
    /// [`TASK_GRAPH_LIMIT`]), ordered by `(created_at, id)`, with the edges
    /// between them ordered by `(from, to, kind)`. Terminal items are left out
    /// unless they changed at or after `include_terminal_since`. Only a blocked
    /// node gets a waiting reason here: the daemon adds the dispatcher's.
    pub fn task_graph(
        &self,
        filter: &TaskFilter,
        limit: Option<u32>,
        estimates: bool,
    ) -> Result<TaskGraph, ArchonError> {
        let limit = graph_limit(limit)?;
        let tx = read_tx(&self.conn)?;
        let as_of = self.last_change_seq()?;
        let graph = read_graph(&self.conn, filter, limit, estimates, as_of)?;
        debug_assert_eq!(self.last_change_seq()?, as_of, "one read transaction");
        end_read_tx(tx, "end task graph read")?;
        Ok(graph)
    }
}

fn graph_limit(limit: Option<u32>) -> Result<usize, ArchonError> {
    let limit = limit.unwrap_or(TASK_GRAPH_LIMIT);
    if !(1..=MAX_TASK_GRAPH_LIMIT).contains(&limit) {
        return Err(ArchonError::Invalid(format!(
            "a task graph holds 1 to {MAX_TASK_GRAPH_LIMIT} nodes, not {limit}"
        )));
    }
    Ok(limit as usize)
}

fn read_graph(
    conn: &Connection,
    filter: &TaskFilter,
    limit: usize,
    estimates: bool,
    as_of: u64,
) -> Result<TaskGraph, ArchonError> {
    assert!(!conn.is_autocommit(), "a graph is read in one transaction");
    check_filter(conn, filter)?;
    let (mut selected, truncated) = select_nodes(conn, filter, limit)?;
    let ids = id_array(selected.iter().map(|(n, _)| n.id.as_str()));
    fill_nodes(conn, &mut selected, &ids, estimates)?;
    let nodes: Vec<TaskNode> = selected.into_iter().map(|(node, _)| node).collect();
    let members: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let edges = edges_between(conn, &ids, &members)?;
    let initiatives = initiatives_of(conn, &nodes, filter.initiative.as_ref())?;
    assert!(nodes.len() <= limit, "a graph keeps to its limit");
    debug_assert!(
        nodes
            .windows(2)
            .all(|w| (w[0].created_at, &w[0].id) < (w[1].created_at, &w[1].id)),
        "nodes are ordered by (created_at, id)"
    );
    Ok(TaskGraph {
        nodes,
        edges,
        initiatives,
        as_of,
        truncated,
    })
}

/// An empty list would match nothing, and a filter naming an item or
/// initiative that does not exist is the caller's mistake.
fn check_filter(conn: &Connection, filter: &TaskFilter) -> Result<(), ArchonError> {
    let lists = [
        ("repos", filter.repos.as_ref().map(Vec::len)),
        ("states", filter.states.as_ref().map(Vec::len)),
        ("pipelines", filter.pipelines.as_ref().map(Vec::len)),
    ];
    if let Some((name, _)) = lists.iter().find(|(_, len)| *len == Some(0)) {
        return Err(ArchonError::Invalid(format!(
            "filter `{name}` is empty and would match nothing; leave it out to match all"
        )));
    }
    if let Some(item) = &filter.component_of
        && !row_exists(conn, "SELECT 1 FROM items WHERE id = ?1", item.as_str())?
    {
        return Err(ArchonError::NotFound(format!("queue item {item}")));
    }
    if let Some(initiative) = &filter.initiative
        && !row_exists(
            conn,
            "SELECT 1 FROM initiatives WHERE id = ?1",
            initiative.as_str(),
        )?
    {
        return Err(ArchonError::NotFound(format!("initiative {initiative}")));
    }
    Ok(())
}

fn row_exists(conn: &Connection, sql: &str, id: &str) -> Result<bool, ArchonError> {
    let found = conn
        .query_row(sql, [id], |_| Ok(()))
        .optional()
        .ctx("look up a task graph filter")?;
    Ok(found.is_some())
}

/// The `WHERE` clauses and their arguments, which follow the component's `?1`.
fn node_clauses(filter: &TaskFilter) -> (Vec<String>, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut args = Vec::new();
    let mut any_of = |column: &str, values: Vec<String>| {
        let marks = vec!["?"; values.len()].join(", ");
        clauses.push(format!("{column} IN ({marks})"));
        args.extend(values.into_iter().map(Value::Text));
    };
    if let Some(repos) = &filter.repos {
        any_of(
            "repo",
            repos.iter().map(|r| r.as_str().to_owned()).collect(),
        );
    }
    if let Some(states) = &filter.states {
        any_of(
            "state",
            states.iter().map(|s| kind_name(*s).to_owned()).collect(),
        );
    }
    if let Some(pipelines) = &filter.pipelines {
        any_of("pipeline", pipelines.clone());
    }
    if let Some(priority) = filter.min_priority {
        clauses.push("priority >= ?".to_owned());
        args.push(Value::Integer(priority_rank(priority)));
    }
    if let Some(initiative) = &filter.initiative {
        clauses.push("initiative_id = ?".to_owned());
        args.push(Value::Text(initiative.as_str().to_owned()));
    }
    match &filter.include_terminal_since {
        Some(since) => {
            clauses.push(format!("(state NOT IN {TERMINAL} OR updated_at >= ?)"));
            args.push(Value::Text(ts(since)));
        }
        None => clauses.push(format!("state NOT IN {TERMINAL}")),
    }
    if filter.component_of.is_some() {
        clauses.push("id IN (SELECT id FROM component)".to_owned());
    }
    (clauses, args)
}

/// The first `limit` nodes in `(created_at, id)` order, each with the run its
/// state names when running, and whether more matched.
fn select_nodes(
    conn: &Connection,
    filter: &TaskFilter,
    limit: usize,
) -> Result<(Vec<(TaskNode, Option<PipelineRunId>)>, bool), ArchonError> {
    let (clauses, rest) = node_clauses(filter);
    assert!(!clauses.is_empty(), "terminal items are always filtered");
    let mut args = Vec::with_capacity(rest.len() + 2);
    let cte = match &filter.component_of {
        Some(root) => {
            args.push(Value::Text(root.as_str().to_owned()));
            COMPONENT_CTE
        }
        None => "",
    };
    args.extend(rest);
    args.push(Value::Integer(sql_limit(limit + 1)));
    let sql = format!(
        "{cte}SELECT id, repo, pipeline, priority, json_extract(data, '$.title'),
                json_extract(data, '$.state'), tags, initiative_id, created_at, updated_at, state
         FROM items WHERE {} ORDER BY created_at, id LIMIT ?",
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql).ctx("prepare task graph nodes")?;
    let mut rows = stmt
        .query(params_from_iter(args))
        .ctx("read task graph nodes")?;
    let mut nodes = Vec::new();
    while let Some(row) = rows.next().ctx("read task graph node")? {
        nodes.push(node_of(row)?);
    }
    let truncated = nodes.len() > limit;
    nodes.truncate(limit);
    Ok((nodes, truncated))
}

fn node_of(row: &Row<'_>) -> Result<(TaskNode, Option<PipelineRunId>), ArchonError> {
    let text = |index: usize| row.get::<_, String>(index).ctx("read task graph node");
    let id = QueueItemId::parse(&text(0)?)?;
    let repo: Option<String> = row.get(1).ctx("read node repository")?;
    let state: QueueState = from_json(&text(5)?, "queue item state")?;
    let initiative: Option<String> = row.get(7).ctx("read node initiative")?;
    let kind_column = text(10)?;
    let kind = QueueStateKind::from(&state);
    if kind_name(kind) != kind_column {
        return Err(ArchonError::Corrupt(format!(
            "item {id} is indexed {kind_column} but its record says {}",
            kind_name(kind)
        )));
    }
    let run = match &state {
        QueueState::Running { run } => Some(run.clone()),
        _ => None,
    };
    let node = TaskNode {
        repo: repo.as_deref().map(RepoId::parse).transpose()?,
        title: text(4)?,
        pipeline: text(2)?,
        priority: priority_of(row.get(3).ctx("read node priority")?)?,
        state: kind,
        state_detail: state_detail(&state),
        stage: None,
        waiting_reason: None,
        spent_usd: 0.0,
        initiative: initiative.as_deref().map(InitiativeId::parse).transpose()?,
        tags: from_json(&text(6)?, "item tags")?,
        created_at: timestamp(&text(8)?, "item creation time")?,
        updated_at: timestamp(&text(9)?, "item update time")?,
        issue: None,
        pr: None,
        agents_running: 0,
        id,
    };
    Ok((node, run))
}

/// Adds what other tables know of each node: its live stage, running agents,
/// spend, GitHub refs and, when blocked, why.
fn fill_nodes(
    conn: &Connection,
    nodes: &mut [(TaskNode, Option<PipelineRunId>)],
    ids: &str,
    estimates: bool,
) -> Result<(), ArchonError> {
    let stages = live_stages(conn, ids)?;
    let agents = per_item(
        conn,
        "SELECT r.item, COUNT(*) FROM stage_runs s JOIN runs r ON r.id = s.run
         WHERE s.terminal = 0 AND r.item IN (SELECT value FROM json_each(?1)) GROUP BY r.item",
        ids,
        |row| row.get::<_, u32>(1),
    )?;
    let spend = per_item(
        conn,
        &format!(
            "SELECT item, SUM(cost_usd + {}) FROM spend
             WHERE item IN (SELECT value FROM json_each(?1)) GROUP BY item",
            if estimates { "estimated_cost_usd" } else { "0" }
        ),
        ids,
        |row| row.get::<_, f64>(1),
    )?;
    let deps = per_item(
        conn,
        "SELECT d.item, SUM(x.state IN ('failed', 'cancelled')), SUM(x.state <> 'succeeded')
         FROM item_deps d JOIN items x ON x.id = d.depends_on
         WHERE d.item IN (SELECT value FROM json_each(?1)) GROUP BY d.item",
        ids,
        |row| Ok((row.get::<_, u32>(1)?, row.get::<_, u32>(2)?)),
    )?;
    let mut links = links_by_item(conn, ids)?;
    for (node, run) in nodes.iter_mut() {
        if let Some(run) = run {
            node.stage = stages.get(run.as_str()).cloned();
        }
        let id = node.id.as_str();
        node.agents_running = agents.get(id).copied().unwrap_or(0);
        node.spent_usd = spend.get(id).copied().unwrap_or(0.0);
        debug_assert!(node.spent_usd.is_finite(), "spend is a finite sum");
        (node.issue, node.pr) = links.remove(id).unwrap_or_default();
        if node.state == QueueStateKind::Blocked {
            let (broken, unmet) = deps.get(id).copied().unwrap_or((0, 0));
            node.waiting_reason = Some(blocked_code(broken, unmet));
        }
    }
    Ok(())
}

/// Each live run of the nodes, by run id, with the stage it is in.
fn live_stages(conn: &Connection, ids: &str) -> Result<BTreeMap<String, String>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM runs
             WHERE terminal = 0 AND item IN (SELECT value FROM json_each(?1))",
        )
        .ctx("read live runs")?;
    let rows = stmt
        .query_map([ids], |row| row.get::<_, String>(0))
        .ctx("read live runs")?;
    let mut stages = BTreeMap::new();
    for data in rows {
        let run: PipelineRun = from_json(&data.ctx("read live run")?, "pipeline run")?;
        if let Some(stage) = run.state.live_stage() {
            stages.insert(run.id.as_str().to_owned(), stage.to_owned());
        }
    }
    Ok(stages)
}

/// One value per item from `sql`, whose first column is the item id and
/// whose `?1` is the JSON array of node ids.
fn per_item<T>(
    conn: &Connection,
    sql: &str,
    ids: &str,
    value: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<BTreeMap<String, T>, ArchonError> {
    let mut stmt = conn.prepare_cached(sql).ctx("prepare task graph detail")?;
    let rows = stmt
        .query_map([ids], |row| Ok((row.get::<_, String>(0)?, value(row)?)))
        .ctx("read task graph detail")?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (item, value) = row.ctx("read task graph detail")?;
        let earlier = out.insert(item, value);
        assert!(earlier.is_none(), "{sql} groups by item");
    }
    Ok(out)
}

fn links_by_item(
    conn: &Connection,
    ids: &str,
) -> Result<BTreeMap<String, (Option<GithubRef>, Option<PrRef>)>, ArchonError> {
    let links =
        super::github_links::query_links(conn, "item IN (SELECT value FROM json_each(?1))", [ids])?;
    let mut grouped: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for link in links {
        grouped
            .entry(link.item.as_str().to_owned())
            .or_default()
            .push(link);
    }
    Ok(grouped
        .into_iter()
        .map(|(item, links)| (item, super::github_links::summary_refs(&links)))
        .collect())
}

/// The waiting reason of a blocked item, from its dependencies' states, as
/// the summaries give it.
fn blocked_code(broken: u32, unmet: u32) -> WaitingReasonCode {
    debug_assert!(broken <= unmet, "a broken dependency is also unmet");
    match (broken, unmet) {
        (0, 0) => WaitingReasonCode::Blocked,
        (0, _) => WaitingReasonCode::DependencyWaiting,
        _ => WaitingReasonCode::DependencyFailed,
    }
}

/// Dependency and stack edges with both ends among the nodes, ordered by
/// `(from, to, kind)`. A dependency inside one initiative is that initiative's
/// ordering; a stack edge is satisfied once its parent merged or closed.
/// Each node's edges are found by its key; testing both ends in SQL took
/// seconds on 2,000 nodes, so the far end is tested against `members`.
fn edges_between(
    conn: &Connection,
    ids: &str,
    members: &HashSet<&str>,
) -> Result<Vec<TaskEdge>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT d.depends_on, d.item,
                    p.initiative_id IS NOT NULL AND p.initiative_id IS c.initiative_id,
                    p.state = 'succeeded'
             FROM json_each(?1) n CROSS JOIN item_deps d ON d.item = n.value
             JOIN items p ON p.id = d.depends_on JOIN items c ON c.id = d.item
             UNION ALL
             SELECT s.parent_item, s.item, NULL, s.state IN ('merged', 'closed')
             FROM json_each(?1) n CROSS JOIN stacks s ON s.item = n.value",
        )
        .ctx("read task graph edges")?;
    let rows = stmt
        .query_map([ids], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<bool>>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .ctx("read task graph edges")?;
    let mut edges = Vec::new();
    for row in rows {
        let (from, to, initiative, satisfied) = row.ctx("read task graph edge")?;
        assert!(
            members.contains(to.as_str()),
            "edges are read from the nodes"
        );
        if !members.contains(from.as_str()) {
            continue;
        }
        let kind = match initiative {
            None => TaskEdgeKind::StackedOn,
            Some(true) => TaskEdgeKind::InitiativeAfter,
            Some(false) => TaskEdgeKind::DependsOn,
        };
        let (from, to) = (QueueItemId::parse(&from)?, QueueItemId::parse(&to)?);
        assert_ne!(from, to, "an item never waits for itself");
        edges.push(TaskEdge {
            from,
            to,
            kind,
            satisfied,
        });
    }
    edges.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));
    Ok(edges)
}

/// The initiatives the nodes belong to, and the one the filter names,
/// ordered by `(created_at, id)`.
fn initiatives_of(
    conn: &Connection,
    nodes: &[TaskNode],
    named: Option<&InitiativeId>,
) -> Result<Vec<GraphInitiative>, ArchonError> {
    let wanted = id_array(
        nodes
            .iter()
            .filter_map(|n| n.initiative.as_ref())
            .chain(named)
            .map(InitiativeId::as_str),
    );
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM initiatives WHERE id IN (SELECT value FROM json_each(?1))
             ORDER BY created_at, id",
        )
        .ctx("read task graph initiatives")?;
    let rows = stmt
        .query_map([wanted], |row| row.get::<_, String>(0))
        .ctx("read task graph initiatives")?;
    let mut out = Vec::new();
    for data in rows {
        let initiative: Initiative = from_json(&data.ctx("read initiative")?, "initiative")?;
        out.push(GraphInitiative {
            id: initiative.id,
            title: initiative.title,
            state: initiative.state,
            atomic: initiative.atomic,
        });
    }
    debug_assert!(
        nodes
            .iter()
            .filter_map(|n| n.initiative.as_ref())
            .all(|i| out.iter().any(|g| &g.id == i)),
        "every member's initiative exists"
    );
    Ok(out)
}

/// A JSON array of ids, bound once and read with `json_each`.
fn id_array<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let ids: Vec<&str> = ids.collect();
    serde_json::to_string(&ids).expect("a list of strings encodes")
}

fn priority_of(rank: i64) -> Result<Priority, ArchonError> {
    let priority = match rank {
        0 => Priority::Low,
        1 => Priority::Normal,
        2 => Priority::High,
        3 => Priority::Critical,
        other => {
            return Err(ArchonError::Corrupt(format!(
                "item priority rank {other} is not a priority"
            )));
        }
    };
    assert_eq!(priority_rank(priority), rank, "ranks round-trip");
    Ok(priority)
}

fn kind_name(kind: QueueStateKind) -> &'static str {
    match kind {
        QueueStateKind::Queued => "queued",
        QueueStateKind::Dispatching => "dispatching",
        QueueStateKind::Running => "running",
        QueueStateKind::Blocked => "blocked",
        QueueStateKind::Succeeded => "succeeded",
        QueueStateKind::Failed => "failed",
        QueueStateKind::Cancelled => "cancelled",
    }
}

/// The reason a blocked, failed or cancelled item carries.
fn state_detail(state: &QueueState) -> Option<String> {
    match state {
        QueueState::Blocked { reason } => Some(reason.clone()),
        QueueState::Failed { error, .. } => Some(error.clone()),
        QueueState::Cancelled { reason } => reason.clone(),
        QueueState::Queued
        | QueueState::Dispatching
        | QueueState::Running { .. }
        | QueueState::Succeeded { .. } => None,
    }
}

fn timestamp(raw: &str, what: &str) -> Result<Timestamp, ArchonError> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| ArchonError::Corrupt(format!("{what} {raw:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_the_state_column_names() {
        for (kind, name) in [
            QueueStateKind::Queued,
            QueueStateKind::Dispatching,
            QueueStateKind::Running,
            QueueStateKind::Blocked,
            QueueStateKind::Succeeded,
            QueueStateKind::Failed,
            QueueStateKind::Cancelled,
        ]
        .into_iter()
        .zip(QueueState::KINDS)
        {
            assert_eq!(kind_name(kind), name);
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
        }
    }

    #[test]
    fn every_priority_rank_reads_back_and_others_are_corrupt() {
        for priority in [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Critical,
        ] {
            assert_eq!(priority_of(priority_rank(priority)).unwrap(), priority);
        }
        assert!(matches!(priority_of(4), Err(ArchonError::Corrupt(_))));
    }

    #[test]
    fn a_limit_outside_one_to_the_most_is_refused() {
        assert_eq!(graph_limit(None).unwrap(), TASK_GRAPH_LIMIT as usize);
        assert_eq!(graph_limit(Some(MAX_TASK_GRAPH_LIMIT)).unwrap(), 2000);
        for bad in [0, MAX_TASK_GRAPH_LIMIT + 1] {
            assert!(matches!(
                graph_limit(Some(bad)),
                Err(ArchonError::Invalid(_))
            ));
        }
    }
}
