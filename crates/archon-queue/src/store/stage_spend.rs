//! Stage allocations of a cumulative agent charge. A fresh agent's first
//! binding can claim usage that arrived before the create reply. For a kept
//! agent, the next window opens before it is told to start the new turn.

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::run::StageRun;
use rusqlite::{Connection, OptionalExtension, params};

use super::SpendRow;
use super::codec::{SqlContext, to_i64, ts, write_tx};

const ANCESTORS: &str = "ancestors(agent, child_seq) AS (
    SELECT ?1, NULL
    UNION
    SELECT d.parent, d.created_seq FROM ancestors a JOIN delegated_agents d ON d.agent = a.agent
)";

pub(super) fn bind(
    conn: &Connection,
    agent: &AetherAgentId,
    stage: &StageRun,
    opened: &Timestamp,
    after_seq: Option<u64>,
) -> Result<(), ArchonError> {
    assert!(stage.attempt >= 1, "attempts are one-based");
    assert!(
        !conn.is_autocommit(),
        "a binding is part of a stage transaction"
    );
    super::spend::link_run(conn, agent, &stage.run)?;
    let after = after_seq
        .map(|seq| to_i64(seq, "spend handoff sequence"))
        .transpose()?;
    // The first window covers usage recorded before the create reply.
    let inserted = conn
        .execute(
            "INSERT INTO stage_spend_windows (root, stage, opens_at, after_seq)
             VALUES (?1, ?2, CASE
                WHEN EXISTS (SELECT 1 FROM stage_spend_windows WHERE root = ?1) THEN ?3
                ELSE '' END, ?4)
             ON CONFLICT (root, stage) DO NOTHING",
            params![agent.as_str(), stage.id.as_str(), ts(opened), after],
        )
        .ctx("bind spend to stage")?;
    if inserted != 0 {
        claim_unambiguous(conn, agent)?;
    }
    Ok(())
}

pub(super) fn bind_kept(
    conn: &mut Connection,
    stage: &StageRunId,
    agent: &AetherAgentId,
    at: &Timestamp,
    after_seq: Option<u64>,
) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    let stage = super::runs::stage(&tx, stage)?;
    let holder = super::runs::stage_by_agent(&tx, agent)?;
    let taking_over = holder.is_some_and(|holder| {
        holder.run == stage.run
            && holder.stage_id == stage.stage_id
            && holder.fan_index == stage.fan_index
            && holder.id != stage.id
            && holder.state.is_terminal()
    });
    if !taking_over || stage.state != archon_core::run::StageState::Spawning {
        return Err(ArchonError::InvalidTransition(format!(
            "stage {} cannot take spend from kept agent {agent}",
            stage.id
        )));
    }
    bind(&tx, agent, &stage, at, after_seq)?;
    tx.commit().ctx("commit kept agent's spend binding")
}

pub(super) fn bind_spawned(
    conn: &mut Connection,
    stage: &StageRunId,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    let tx = write_tx(conn)?;
    let stage = super::runs::stage(&tx, stage)?;
    bind(&tx, agent, &stage, &stage.started_at, None)?;
    tx.commit().ctx("commit late agent's spend binding")
}

/// Resolve an existing ledger row only when the entire subtree has one
/// possible stage for that charge. This covers usage received before create
/// replies and ancestry learned late for a stage that never reused its agent.
pub(super) fn claim_unambiguous(
    conn: &Connection,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    assert!(!conn.is_autocommit(), "reconcile with the ownership write");
    let sql = format!(
        "WITH RECURSIVE {ANCESTORS},
         sole_stage(stage) AS (
             SELECT MIN(w.stage) FROM ancestors a JOIN stage_spend_windows w ON w.root = a.agent
             HAVING COUNT(*) = 1 AND MIN(w.opens_at) = ''
         ),
         subtree(agent) AS (
             SELECT ?1
             UNION
             SELECT d.agent FROM subtree t JOIN delegated_agents d ON d.parent = t.agent
         )
         INSERT INTO stage_spend (agent, stage, cost_usd, estimated_cost_usd)
         SELECT s.agent, sole.stage, s.cost_usd, s.estimated_cost_usd
         FROM subtree t JOIN spend s ON s.agent = t.agent CROSS JOIN sole_stage sole
         JOIN stage_runs st ON st.id = sole.stage JOIN runs r ON r.id = st.run
         WHERE s.item = r.item AND s.orchestration = 0
           AND NOT EXISTS (SELECT 1 FROM stage_spend x WHERE x.agent = s.agent)"
    );
    conn.execute(&sql, [agent.as_str()])
        .ctx("link spend recorded before its stage")?;
    Ok(())
}

pub(super) fn record(
    conn: &Connection,
    row: &SpendRow,
    cost: f64,
    estimate: f64,
    event_seq: Option<u64>,
) -> Result<(), ArchonError> {
    assert!(
        !conn.is_autocommit(),
        "attribute within the ledger transaction"
    );
    assert!(cost.is_finite() && estimate.is_finite() && estimate >= 0.0);
    let stage = stage_for(conn, row, event_seq)?;
    if let Some(stage) = stage {
        conn.execute(
            "INSERT INTO stage_spend (agent, stage, cost_usd, estimated_cost_usd)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (agent, stage) DO UPDATE SET
                cost_usd = cost_usd + excluded.cost_usd,
                estimated_cost_usd = estimated_cost_usd + excluded.estimated_cost_usd",
            params![row.agent.as_str(), stage, cost.max(0.0), estimate],
        )
        .ctx("charge stage spend")?;
    }
    if cost < 0.0 {
        correct(conn, &row.agent, -cost)?;
    }
    Ok(())
}

fn stage_for(
    conn: &Connection,
    row: &SpendRow,
    event_seq: Option<u64>,
) -> Result<Option<String>, ArchonError> {
    let seq = event_seq
        .map(|seq| to_i64(seq, "spend event sequence"))
        .transpose()?;
    // The root uses its usage event; delegated subtrees stay with the first
    // child below that root, even if their usage arrives during a later turn.
    let sql = format!(
        "WITH RECURSIVE {ANCESTORS}
         SELECT w.stage FROM ancestors a JOIN stage_spend_windows w ON w.root = a.agent
         JOIN stage_runs st ON st.id = w.stage JOIN runs r ON r.id = st.run
         WHERE r.item = ?3
           AND (?4 IS NULL OR a.agent = ?1 OR a.child_seq IS NOT NULL OR NOT EXISTS (
               SELECT 1 FROM stage_spend_windows other WHERE other.root = w.root AND other.after_seq IS NOT NULL
           ))
           AND CASE WHEN w.after_seq IS NOT NULL AND ?4 IS NOT NULL
               THEN COALESCE(a.child_seq, ?4) > w.after_seq ELSE w.opens_at <= ?2 END
         ORDER BY CASE WHEN ?4 IS NOT NULL THEN w.after_seq END DESC,
                  w.opens_at DESC, st.started_at DESC, st.id DESC LIMIT 1"
    );
    conn.query_row(
        &sql,
        params![
            row.agent.as_str(),
            ts(&row.at),
            row.item.as_ref().map(|i| i.as_str()),
            seq,
        ],
        |r| r.get(0),
    )
    .optional()
    .ctx("find stage for usage")
}

/// What each agent beneath `root` was charged on `stage`, by the attribution model
/// evidence reads: `None` when its usage went to other stages. Agents whose ledger
/// total is not fully attributed are left out, as only their handoff spans place them.
pub(super) fn delegated_on_stage(
    conn: &Connection,
    root: &AetherAgentId,
    stage: &StageRunId,
    estimates: bool,
) -> Result<BTreeMap<AetherAgentId, Option<f64>>, ArchonError> {
    let mut statement = conn
        .prepare_cached(
            "WITH RECURSIVE subtree(agent) AS (
                 SELECT ?1
                 UNION
                 SELECT d.agent FROM delegated_agents d JOIN subtree t ON d.parent = t.agent
             )
             SELECT s.agent, SUM(CASE WHEN x.stage = ?2
                    THEN x.cost_usd + CASE WHEN ?3 THEN x.estimated_cost_usd ELSE 0 END END)
             FROM subtree t JOIN spend s ON s.agent = t.agent JOIN stage_spend x ON x.agent = s.agent
             WHERE s.agent <> ?1 AND s.orchestration = 0
             GROUP BY s.agent
             HAVING ABS(MAX(s.cost_usd) - SUM(x.cost_usd)) <= 1e-9
                AND ABS(MAX(s.estimated_cost_usd) - SUM(x.estimated_cost_usd)) <= 1e-9",
        )
        .ctx("prepare delegated stage spend")?;
    let rows = statement
        .query_map(params![root.as_str(), stage.as_str(), estimates], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?))
        })
        .ctx("query delegated stage spend")?;
    let mut charged = BTreeMap::new();
    for row in rows {
        let (raw, cost) = row.ctx("read delegated stage spend")?;
        let agent = AetherAgentId::parse(&raw)
            .map_err(|e| ArchonError::Store(format!("delegated spender {raw:?}: {e}")))?;
        if cost.is_some_and(|cost| !cost.is_finite() || cost < 0.0) {
            return Err(ArchonError::Store(format!(
                "agent {agent} holds the invalid stage charge {cost:?}"
            )));
        }
        charged.insert(agent, cost);
    }
    assert!(!charged.contains_key(root), "a root is not beneath itself");
    Ok(charged)
}

pub(super) fn has_window(
    conn: &Connection,
    stage: &StageRunId,
    agent: &AetherAgentId,
) -> Result<bool, ArchonError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM stage_spend_windows WHERE stage = ?1 AND root = ?2)",
        params![stage.as_str(), agent.as_str()],
        |r| r.get(0),
    )
    .ctx("check for a spend window")
}

/// Like the daily ledger, refund the newest attributed charges first. If
/// they run out, the rest reduces the agent's unassigned historical balance.
fn correct(conn: &Connection, agent: &AetherAgentId, mut refund: f64) -> Result<(), ArchonError> {
    assert!(
        !conn.is_autocommit(),
        "refund within the ledger transaction"
    );
    assert!(refund.is_finite() && refund > 0.0);
    let mut statement = conn
        .prepare_cached(
            "SELECT x.stage, x.cost_usd FROM stage_spend x
             JOIN stage_runs st ON st.id = x.stage WHERE x.agent = ?1 AND x.cost_usd > 0
             ORDER BY st.started_at DESC, st.id DESC",
        )
        .ctx("prepare stage refunds")?;
    let charges: Vec<(String, f64)> = statement
        .query_map([agent.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .ctx("query stage charges to refund")?
        .collect::<Result<_, _>>()
        .ctx("read stage charges to refund")?;
    for (stage, cost) in charges {
        let take = refund.min(cost);
        conn.execute(
            "UPDATE stage_spend SET cost_usd = ?3 WHERE agent = ?1 AND stage = ?2",
            params![agent.as_str(), stage, cost - take],
        )
        .ctx("refund stage charge")?;
        refund -= take;
        if refund == 0.0 {
            break;
        }
    }
    Ok(())
}

pub(super) fn unlinked(conn: &Connection) -> Result<Vec<AetherAgentId>, ArchonError> {
    let mut stmt = conn
        .prepare(
            "SELECT s.agent FROM spend s LEFT JOIN stage_spend x ON x.agent = s.agent
             WHERE s.orchestration = 0
             GROUP BY s.agent
             HAVING COUNT(x.stage) = 0
                 OR ABS(s.cost_usd - SUM(x.cost_usd)) > 1e-9
                 OR ABS(s.estimated_cost_usd - SUM(x.estimated_cost_usd)) > 1e-9
             ORDER BY s.agent",
        )
        .ctx("prepare check for unlinked spend")?;
    let mut agents = Vec::new();
    for raw in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .ctx("query unlinked spend")?
    {
        let raw = raw.ctx("read unlinked spend")?;
        let agent = AetherAgentId::parse(&raw)
            .map_err(|e| ArchonError::Store(format!("unlinked spender {raw:?}: {e}")))?;
        agents.push(agent);
    }
    debug_assert!(agents.windows(2).all(|w| w[0].as_str() < w[1].as_str()));
    Ok(agents)
}
