//! The history retention pass (ARCHITECTURE §16): invisible feed rows and
//! settled items, with their runs, stage runs, gates, spend and per-item `kv`,
//! go once older than `storage.retention_days`. Outcomes and lesson uses stay
//! for learning, and so does every item something live still names: a live
//! item through a dependency or stack, an open gate, an open question, an open
//! merge request, an operator correction, a live initiative, a report record
//! or a distillation whose lessons are not applied yet.
//! An item stays while a workspace of its runs is on disk, as gc finds it
//! through the item. Spend of the days budgets still count stays, and what
//! goes is carried in `spend_pruned`.

use std::collections::BTreeSet;
use std::path::Path;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::outcome::Outcome;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::Store;
use super::codec::{SqlContext, from_json, ts};
use super::spend::local_day;

/// What one call deleted, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPruned {
    pub feed: usize,
    pub items: usize,
    pub runs: usize,
    pub stage_runs: usize,
    pub gates: usize,
    pub spend: usize,
    pub kv: usize,
}

impl HistoryPruned {
    pub fn total(&self) -> usize {
        self.feed + self.items + self.runs + self.stage_runs + self.gates + self.spend + self.kv
    }

    pub fn absorb(&mut self, other: HistoryPruned) {
        self.feed += other.feed;
        self.items += other.items;
        self.runs += other.runs;
        self.stage_runs += other.stage_runs;
        self.gates += other.gates;
        self.spend += other.spend;
        self.kv += other.kv;
    }
}

/// Settled items older than the cutoff `?1` whose spend has no day from the
/// cutoff's day `?2` on, and that nothing open or live names.
const CANDIDATES: &str = "
INSERT INTO temp.prune_items (id)
SELECT i.id FROM items i
WHERE i.state IN ('succeeded', 'failed', 'cancelled') AND i.updated_at < ?1
  AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.item = i.id AND r.terminal = 0)
  AND NOT EXISTS (SELECT 1 FROM gates g WHERE g.item = i.id AND g.open = 1)
  AND NOT EXISTS (SELECT 1 FROM questions q
                  WHERE q.open = 1 AND json_extract(q.data, '$.item') = i.id)
  AND NOT EXISTS (SELECT 1 FROM merge_requests m WHERE m.item = i.id
                  AND m.state IN ('awaiting_operator', 'queued', 'merging'))
  AND NOT EXISTS (SELECT 1 FROM operator_feedback f WHERE f.item = i.id)
  AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.item = i.id
                  AND EXISTS (SELECT 1 FROM json_each(i.data, '$.tags')
                              WHERE value = 'archon:distillation')
                  AND NOT EXISTS (SELECT 1 FROM kv WHERE key = 'learning/applied/' || r.id))
  AND i.id NOT IN (SELECT item FROM (SELECT CASE WHEN json_valid(value)
                                    THEN json_extract(value, '$.item') END AS item
                                    FROM kv WHERE key >= 'report:' AND key < 'report;')
                   WHERE item IS NOT NULL)
  AND (i.initiative_id IS NULL OR EXISTS (SELECT 1 FROM initiatives n
       WHERE n.id = i.initiative_id AND n.state IN ('done', 'failed', 'cancelled')))
  AND NOT EXISTS (SELECT 1 FROM spend s WHERE s.item = i.id AND s.at >= ?1)
  AND NOT EXISTS (SELECT 1 FROM spend s JOIN spend_daily d ON d.agent = s.agent
                  WHERE s.item = i.id AND d.day >= ?2)";

/// Candidates an item staying still names, through a dependency or a stack.
const STILL_NAMED: &str = "
DELETE FROM temp.prune_items WHERE id IN (
    SELECT d.depends_on FROM item_deps d
    WHERE d.item NOT IN (SELECT id FROM temp.prune_items)
    UNION SELECT s.parent_item FROM stacks s
    WHERE s.item NOT IN (SELECT id FROM temp.prune_items)
    UNION SELECT r.item FROM stacks s JOIN runs r ON r.id = s.run
    WHERE s.item NOT IN (SELECT id FROM temp.prune_items))";

/// Delete the history older than `before`, at most `limit` items with all
/// they own, dependents first, in one transaction. `on_disk` says whether a run
/// workspace still exists.
pub(super) fn prune(
    store: &mut Store,
    before: &Timestamp,
    limit: usize,
    on_disk: &dyn Fn(&Path) -> bool,
) -> Result<HistoryPruned, ArchonError> {
    assert!(limit > 0, "a call prunes something");
    let limit = i64::try_from(limit).expect("a batch fits an i64");
    // Unchecked so the outcomes can still be settled through the store; `&mut`
    // keeps any other use of the connection out meanwhile.
    let store = &*store;
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)
        .ctx("begin the history retention pass")?;
    let cutoff = ts(before);
    let mut pruned = HistoryPruned {
        feed: tx
            .execute(
                "DELETE FROM feed WHERE seq IN
                 (SELECT seq FROM feed WHERE visible = 0 AND at < ?1 ORDER BY seq LIMIT ?2)",
                params![cutoff, limit],
            )
            .ctx("prune invisible feed rows")?,
        ..HistoryPruned::default()
    };
    pruned.kv += prune_confirmations(&tx, &cutoff)?;
    let chosen = choose_items(&tx, &cutoff, &local_day(before), limit, on_disk)?;
    if chosen > 0 {
        settle_outcomes(store, &tx)?;
        delete_chosen(&tx, chosen, &mut pruned)?;
    }
    tx.execute_batch("DROP TABLE temp.prune_items; DROP TABLE temp.prune_runs;")
        .ctx("drop the pruning tables")?;
    tx.commit().ctx("commit the history retention pass")?;
    Ok(pruned)
}

/// Fill `temp.prune_items` with at most `limit` items that may go, and
/// `temp.prune_runs` with their runs. Returns how many items it chose.
fn choose_items(
    tx: &Transaction<'_>,
    cutoff: &str,
    cutoff_day: &str,
    limit: i64,
    on_disk: &dyn Fn(&Path) -> bool,
) -> Result<usize, ArchonError> {
    tx.execute_batch(
        "CREATE TEMP TABLE prune_items (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE prune_runs (id TEXT PRIMARY KEY);",
    )
    .ctx("create the pruning tables")?;
    tx.execute(CANDIDATES, params![cutoff, cutoff_day])
        .ctx("choose settled items to prune")?;
    leave_out_on_disk(tx, on_disk)?;
    keep_named(tx)?;
    choose_dependents_first(tx, limit)?;
    let named = tx.execute(STILL_NAMED, []).ctx("check the chosen items")?;
    assert_eq!(named, 0, "only a chosen item names a chosen item");
    tx.execute(
        "INSERT INTO temp.prune_runs (id)
         SELECT id FROM runs WHERE item IN (SELECT id FROM temp.prune_items)",
        [],
    )
    .ctx("choose the runs of the pruned items")?;
    let chosen: i64 = tx
        .query_row("SELECT COUNT(*) FROM temp.prune_items", [], |r| r.get(0))
        .ctx("count the items to prune")?;
    assert!(chosen <= limit, "at most a batch is chosen");
    Ok(usize::try_from(chosen).expect("a count is not negative"))
}

/// Leave out every candidate a workspace of whose runs is still on disk.
fn leave_out_on_disk(
    tx: &Transaction<'_>,
    on_disk: &dyn Fn(&Path) -> bool,
) -> Result<(), ArchonError> {
    let mut runs = tx
        .prepare(
            "SELECT r.item, json_extract(r.data, '$.workspace'), json_extract(r.data, '$.team_root')
             FROM runs r WHERE r.item IN (SELECT id FROM temp.prune_items)",
        )
        .ctx("prepare the workspaces of the items to prune")?;
    let mut rows = runs
        .query([])
        .ctx("read the workspaces of the items to prune")?;
    let mut held = BTreeSet::new();
    while let Some(row) = rows.next().ctx("read a workspace to prune")? {
        let item: String = row.get(0).ctx("read the item to prune")?;
        let workspace: Option<String> = row.get(1).ctx("read a run workspace")?;
        let team_root: Option<String> = row.get(2).ctx("read a run's team root")?;
        if [workspace, team_root]
            .iter()
            .flatten()
            .any(|path| on_disk(Path::new(path)))
        {
            held.insert(item);
        }
    }
    drop(rows);
    let mut leave = tx
        .prepare("DELETE FROM temp.prune_items WHERE id = ?1")
        .ctx("prepare leaving out an item on disk")?;
    for item in &held {
        let left = leave.execute([item]).ctx("leave out an item on disk")?;
        assert_eq!(left, 1, "a held item was a candidate");
    }
    Ok(())
}

/// Candidates nothing still waiting names, through a dependency or a stack,
/// newest first: choosing them never leaves out an item that names one chosen.
const LEAVES: &str = "
WITH waiting (id) AS (SELECT id FROM temp.prune_items EXCEPT SELECT id FROM temp.prune_chosen)
INSERT INTO temp.prune_chosen (id)
SELECT w.id FROM waiting w JOIN items i ON i.id = w.id
WHERE NOT EXISTS (SELECT 1 FROM item_deps d JOIN waiting o ON o.id = d.item
                  WHERE d.depends_on = w.id AND d.item <> w.id)
  AND NOT EXISTS (SELECT 1 FROM stacks s JOIN waiting o ON o.id = s.item
                  WHERE s.parent_item = w.id AND s.item <> w.id)
  AND NOT EXISTS (SELECT 1 FROM stacks s JOIN waiting o ON o.id = s.item
                  JOIN runs r ON r.id = s.run WHERE r.item = w.id AND s.item <> w.id)
ORDER BY i.seq DESC LIMIT ?1";

/// Keep at most `limit` candidates, each chosen only once every candidate
/// that names it is: a dependency chain longer than a batch still goes.
fn choose_dependents_first(tx: &Transaction<'_>, limit: i64) -> Result<(), ArchonError> {
    tx.execute_batch("CREATE TEMP TABLE prune_chosen (id TEXT PRIMARY KEY)")
        .ctx("create the chosen items table")?;
    let mut chosen = 0i64;
    let mut rounds = 0i64;
    while chosen < limit {
        let added = tx
            .execute(LEAVES, [limit - chosen])
            .ctx("choose items to prune")?;
        if added == 0 {
            break;
        }
        chosen += i64::try_from(added).expect("a batch fits an i64");
        rounds += 1;
        assert!(rounds <= limit, "every round chooses at least one item");
    }
    assert!(chosen <= limit, "at most a batch is chosen");
    tx.execute_batch(
        "DELETE FROM temp.prune_items WHERE id NOT IN (SELECT id FROM temp.prune_chosen);
         DROP TABLE temp.prune_chosen;",
    )
    .ctx("keep the chosen items")
}

/// Drop candidates an item staying names until none is left: each round
/// drops at least one, so it ends.
fn keep_named(tx: &Transaction<'_>) -> Result<(), ArchonError> {
    let mut rounds = 0usize;
    while tx.execute(STILL_NAMED, []).ctx("keep named items")? > 0 {
        rounds += 1;
        assert!(rounds < 1_000_000, "every round drops a candidate");
    }
    Ok(())
}

/// Write each pruned run's outcome as learning reads it now: its cost is read
/// again from the stage runs and spend about to go.
fn settle_outcomes(store: &Store, tx: &Transaction<'_>) -> Result<(), ArchonError> {
    let mut stmt = tx
        .prepare("SELECT data FROM outcomes WHERE run IN (SELECT id FROM temp.prune_runs)")
        .ctx("prepare the outcomes to settle")?;
    let stored = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .ctx("read the outcomes to settle")?
        .map(|raw| from_json::<Outcome>(&raw.ctx("read an outcome to settle")?, "outcome"))
        .collect::<Result<Vec<_>, _>>()?;
    for outcome in stored {
        let run = outcome.run.clone();
        let settled = store.settled_outcome(outcome)?;
        assert_eq!(settled.run, run, "settling keeps the run");
        assert!(settled.check().is_ok(), "a settled outcome is valid");
        super::outcomes::write(tx, &settled)?;
    }
    Ok(())
}

/// Delete what the chosen items own, children before parents, so every
/// foreign key holds at each statement.
fn delete_chosen(
    tx: &Transaction<'_>,
    chosen: usize,
    pruned: &mut HistoryPruned,
) -> Result<(), ArchonError> {
    pruned.kv += prune_item_kv(tx)?;
    let run_owned = [
        "DELETE FROM run_edits WHERE run IN (SELECT id FROM temp.prune_runs)",
        "DELETE FROM agent_runs WHERE run IN (SELECT id FROM temp.prune_runs)",
        "DELETE FROM run_pull_requests WHERE run IN (SELECT id FROM temp.prune_runs)",
        "DELETE FROM merge_requests WHERE item IN (SELECT id FROM temp.prune_items)",
        "DELETE FROM stacks WHERE item IN (SELECT id FROM temp.prune_items)",
    ];
    for sql in run_owned {
        tx.execute(sql, []).ctx("prune what a run owns")?;
    }
    pruned.gates = tx
        .execute(
            "DELETE FROM gates WHERE item IN (SELECT id FROM temp.prune_items)",
            [],
        )
        .ctx("prune gates")?;
    pruned.stage_runs = tx
        .execute(
            "DELETE FROM stage_runs WHERE run IN (SELECT id FROM temp.prune_runs)",
            [],
        )
        .ctx("prune stage runs")?;
    pruned.spend = prune_spend(tx)?;
    pruned.runs = tx
        .execute(
            "DELETE FROM runs WHERE id IN (SELECT id FROM temp.prune_runs)",
            [],
        )
        .ctx("prune runs")?;
    tx.execute(
        "DELETE FROM item_deps WHERE item IN (SELECT id FROM temp.prune_items)",
        [],
    )
    .ctx("prune item dependencies")?;
    pruned.items = tx
        .execute(
            "DELETE FROM items WHERE id IN (SELECT id FROM temp.prune_items)",
            [],
        )
        .ctx("prune items")?;
    assert_eq!(pruned.items, chosen, "every chosen item is deleted");
    Ok(())
}

/// The per-item `kv` families: an item's steers and steered stages, a
/// distillation run's applied mark, and how a pull request only
/// pruned runs delivered to ended. A reshaped run's outcome keeps saying so
/// once its run edits are gone, as learning leaves reshaped runs out.
fn prune_item_kv(tx: &Transaction<'_>) -> Result<usize, ArchonError> {
    tx.execute(
        "INSERT OR IGNORE INTO kv (key, value)
         SELECT DISTINCT 'learning/reshaped/' || e.run, '1' FROM run_edits e
         JOIN outcomes o ON o.run = e.run WHERE e.run IN (SELECT id FROM temp.prune_runs)",
        [],
    )
    .ctx("keep reshaped outcomes marked")?;
    let steers = tx
        .execute(
            "DELETE FROM kv WHERE key IN (SELECT 'steers:' || id FROM temp.prune_items
                                          UNION SELECT 'shape:' || id FROM temp.prune_items
                                          UNION SELECT 'learning/applied/' || id FROM temp.prune_runs)",
            [],
        )
        .ctx("prune steer histories")?;
    let settled = tx
        .execute(
            "DELETE FROM kv WHERE key IN (
                 SELECT 'pr_settled:' || p.host || '/' || p.repository || '#' || p.number
                 FROM run_pull_requests p
                 WHERE p.run IN (SELECT id FROM temp.prune_runs)
                   AND NOT EXISTS (SELECT 1 FROM run_pull_requests o
                       WHERE o.host = p.host AND o.repository = p.repository
                         AND o.number = p.number
                         AND o.run NOT IN (SELECT id FROM temp.prune_runs)))",
            [],
        )
        .ctx("prune settled pull requests")?;
    Ok(steers + settled)
}

/// Operator confirmations whose question closed and was asked before the
/// cutoff: nothing answers them any more.
fn prune_confirmations(tx: &Transaction<'_>, cutoff: &str) -> Result<usize, ArchonError> {
    const PREFIX: &str = "authority_confirmation:";
    tx.execute(
        "DELETE FROM kv WHERE key IN (SELECT ?1 || q.id FROM questions q
         WHERE q.open = 0 AND q.asked_at < ?2)",
        params![PREFIX, cutoff],
    )
    .ctx("prune operator confirmations")
}

/// Delete the chosen items' spend, adding it to `spend_pruned` first so the
/// total budget still counts it. Returns the agents' rows deleted.
fn prune_spend(tx: &Transaction<'_>) -> Result<usize, ArchonError> {
    let (cost, estimate): (f64, f64) = tx
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(estimated_cost_usd), 0)
             FROM spend WHERE item IN (SELECT id FROM temp.prune_items)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ctx("sum the spend to prune")?;
    assert!(
        cost.is_finite() && cost >= 0.0,
        "spend rows are checked on write"
    );
    assert!(
        estimate.is_finite() && estimate >= 0.0,
        "spend rows are checked on write"
    );
    tx.execute(
        "INSERT INTO spend_pruned (id, cost_usd, estimated_cost_usd) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET cost_usd = cost_usd + excluded.cost_usd,
             estimated_cost_usd = estimated_cost_usd + excluded.estimated_cost_usd",
        params![cost, estimate],
    )
    .ctx("carry the pruned spend")?;
    tx.execute(
        "DELETE FROM spend_daily WHERE agent IN
         (SELECT agent FROM spend WHERE item IN (SELECT id FROM temp.prune_items))",
        [],
    )
    .ctx("prune daily spend")?;
    tx.execute(
        "DELETE FROM spend WHERE item IN (SELECT id FROM temp.prune_items)",
        [],
    )
    .ctx("prune spend")
}

/// Hand the pages pruning emptied back to the file system, when the store
/// was made with incremental auto-vacuum; an older store keeps them for reuse.
pub(super) fn vacuum(conn: &Connection) -> Result<bool, ArchonError> {
    let mode: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .ctx("read auto_vacuum")?;
    assert!(
        (0..=2).contains(&mode),
        "auto_vacuum is none, full or incremental"
    );
    if mode != 2 {
        return Ok(false);
    }
    conn.execute_batch("PRAGMA incremental_vacuum")
        .ctx("vacuum freed pages")?;
    Ok(true)
}
