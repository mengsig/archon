//! Builds the dispatcher's view of the store in one read transaction.

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::queue::{Caps, QueueItem};
use rusqlite::Connection;

use super::codec::{SqlContext, from_json, to_u64};
use super::{settings, spend};
use crate::dispatch::{DependencyState, DispatchSnapshot, QueuedCandidate};

struct Running {
    total: u32,
    by_repo: BTreeMap<RepoId, u32>,
    by_pipeline: BTreeMap<String, u32>,
}

/// Live (`spawning` or `running`) stage agents occupy capacity whether or
/// not their pipeline run is still live.
fn running_agents(conn: &Connection) -> Result<Running, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT i.repo, r.pipeline, COUNT(*) FROM stage_runs s
             JOIN runs r ON r.id = s.run JOIN items i ON i.id = r.item
             WHERE s.terminal = 0 GROUP BY i.repo, r.pipeline",
        )
        .ctx("count running agents")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })
        .ctx("count running agents")?;
    let mut running = Running {
        total: 0,
        by_repo: BTreeMap::new(),
        by_pipeline: BTreeMap::new(),
    };
    for row in rows {
        let (repo, pipeline, count) = row.ctx("read running agents")?;
        debug_assert!(count > 0, "GROUP BY produced an empty group");
        running.total += count;
        *running.by_pipeline.entry(pipeline).or_default() += count;
        if let Some(repo) = repo {
            *running.by_repo.entry(RepoId::parse(&repo)?).or_default() += count;
        }
    }
    Ok(running)
}

fn dependency_state(broken: u32, unmet: u32) -> DependencyState {
    debug_assert!(broken <= unmet, "a broken dependency is also unmet");
    match (broken, unmet) {
        (0, 0) => DependencyState::AllSucceeded,
        (0, _) => DependencyState::Waiting,
        _ => DependencyState::Broken,
    }
}

/// Queued items with their dependencies resolved. `first_stage_agents` is 1
/// here; the daemon replaces it with the first stage's fan-out.
fn queued(conn: &Connection) -> Result<Vec<QueuedCandidate>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT i.seq, i.data,
                 (SELECT COUNT(*) FROM item_deps d JOIN items x ON x.id = d.depends_on
                  WHERE d.item = i.id AND x.state IN ('failed', 'cancelled')),
                 (SELECT COUNT(*) FROM item_deps d JOIN items x ON x.id = d.depends_on
                  WHERE d.item = i.id AND x.state <> 'succeeded')
             FROM items i WHERE i.state = 'queued' AND NOT EXISTS
                (SELECT 1 FROM initiatives n WHERE n.id=i.initiative_id AND n.state IN ('cancelled','failed'))
             AND NOT EXISTS (SELECT 1 FROM items sibling WHERE sibling.initiative_id=i.initiative_id
                AND sibling.state IN ('failed','cancelled')) ORDER BY i.priority DESC, i.seq",
        )
        .ctx("list queued items")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })
        .ctx("list queued items")?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, data, broken, unmet) = row.ctx("read queued item")?;
        let item: QueueItem = from_json(&data, "queue item")?;
        assert_eq!(
            item.state.kind(),
            "queued",
            "state column disagrees for {}",
            item.id
        );
        out.push(QueuedCandidate {
            item: item.id,
            repo: item.repo,
            pipeline: item.pipeline,
            priority: item.priority,
            created_seq: to_u64(seq, "item seq")?,
            deps: dependency_state(broken, unmet),
            first_stage_agents: 1,
            gang: false,
        });
    }
    Ok(out)
}

/// Live runs plus `dispatching` items whose run is not inserted yet: such an
/// item is already creating its worktree, and a planning pass that runs in
/// that window must not start another item past `max_active_runs`.
fn active_runs(conn: &Connection) -> Result<u32, ArchonError> {
    let (live, starting): (u32, u32) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM runs WHERE terminal = 0),
                    (SELECT COUNT(*) FROM items i WHERE i.state = 'dispatching'
                     AND NOT EXISTS (SELECT 1 FROM runs r
                                     WHERE r.item = i.id AND r.terminal = 0))",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ctx("count active runs")?;
    live.checked_add(starting)
        .ok_or_else(|| ArchonError::Store("active run count overflows".into()))
}

/// Everything but `caps`, `repo_caps`, `low_disk`, `draining`, `pending_spawns` and the
/// fan-out of first stages, which need configuration, the disk and live
/// drivers. Spend counts
/// estimates (the default `count_estimated_cost = true`).
pub(super) fn build(conn: &Connection, now: &Timestamp) -> Result<DispatchSnapshot, ArchonError> {
    let tx = conn.unchecked_transaction().ctx("begin snapshot")?;
    let running = running_agents(&tx)?;
    let active_runs = active_runs(&tx)?;
    let (spend_today_usd, spend_total_usd) = spend::totals(&tx, now, true)?;
    let snapshot = DispatchSnapshot {
        caps: Caps::default(),
        repo_caps: BTreeMap::new(),
        repo_budgets_enabled: BTreeMap::new(),
        repo_max_per_repo: BTreeMap::new(),
        pause: settings::pause(&tx)?,
        low_disk: false,
        draining: false,
        running_total: running.total,
        running_by_repo: running.by_repo,
        running_by_pipeline: running.by_pipeline,
        active_runs,
        spend_today_usd,
        spend_total_usd,
        pending_spawns: Vec::new(),
        queued: queued(&tx)?,
    };
    tx.commit().ctx("end snapshot")?;
    Ok(snapshot)
}
