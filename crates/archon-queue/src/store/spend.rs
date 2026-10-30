//! The spend ledger. `agent.usage_updated` carries absolute totals, so the
//! `spend` table keeps one row per agent with what has been charged for it.
//! `spend_daily` splits each agent's charge into the local calendar days in
//! which it grew, which is what the daily budget needs: an agent that spent
//! $40 yesterday and $1 today counts $1 today.
//!
//! A turn's list-price estimate gives way to the provider's figure when it ends:
//! only such a settled total lowers a charge, and never below an earlier settled one.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::cost::CostBasis;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use rusqlite::{Connection, OptionalExtension, params};

use super::SpendRow;
use super::codec::{SqlContext, to_i64, ts, write_tx};

/// A day may hold this much less than a correction takes: float rounding.
const ROUNDING_USD: f64 = 1e-9;

/// The local calendar day of `at`, as `YYYY-MM-DD`.
pub(super) fn local_day(at: &Timestamp) -> String {
    at.with_timezone(&chrono::Local)
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

fn check_row(row: &SpendRow) -> Result<(), ArchonError> {
    for (name, value) in [
        ("cost_usd", row.cost_usd),
        ("estimated_cost_usd", row.estimated_cost_usd),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(ArchonError::Invalid(format!(
                "{name} {value} for agent {} is not a non-negative amount",
                row.agent
            )));
        }
    }
    Ok(())
}

/// What an agent's row holds: its charge, when and on what basis, and the
/// highest settled total, which the charge never falls below.
struct Charged {
    cost: f64,
    estimate: f64,
    at: String,
    basis: String,
    settled: f64,
}

fn current(conn: &Connection, row: &SpendRow) -> Result<Option<Charged>, ArchonError> {
    conn.query_row(
        "SELECT cost_usd, estimated_cost_usd, at, cost_basis, settled_cost_usd
         FROM spend WHERE agent = ?1",
        [row.agent.as_str()],
        |r| {
            Ok(Charged {
                cost: r.get(0)?,
                estimate: r.get(1)?,
                at: r.get(2)?,
                basis: r.get(3)?,
                settled: r.get(4)?,
            })
        },
    )
    .optional()
    .ctx("read spend row")
}

fn upsert_latest(conn: &Connection, row: &SpendRow, settled: f64) -> Result<(), ArchonError> {
    conn.execute(
        "INSERT INTO spend (agent, item, cost_usd, estimated_cost_usd, input_tokens,
                            output_tokens, at, cost_basis, settled_cost_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (agent) DO UPDATE SET item = excluded.item,
             cost_usd = excluded.cost_usd, estimated_cost_usd = excluded.estimated_cost_usd,
             input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens,
             at = excluded.at, cost_basis = excluded.cost_basis,
             settled_cost_usd = excluded.settled_cost_usd",
        params![
            row.agent.as_str(),
            row.item.as_ref().map(QueueItemId::as_str),
            row.cost_usd,
            row.estimated_cost_usd,
            to_i64(row.input_tokens, "input tokens")?,
            to_i64(row.output_tokens, "output tokens")?,
            ts(&row.at),
            row.cost_basis.name(),
            settled,
        ],
    )
    .ctx("write spend row")?;
    Ok(())
}

fn add_to_day(
    conn: &Connection,
    row: &SpendRow,
    cost: f64,
    estimate: f64,
) -> Result<(), ArchonError> {
    let (billed, list_price, unclassified) = match row.cost_basis {
        CostBasis::ProviderReported => (cost, 0.0, 0.0),
        CostBasis::ListPrice => (0.0, cost, 0.0),
        CostBasis::Unknown | CostBasis::Mixed => (0.0, 0.0, cost),
    };
    conn.execute(
        "INSERT INTO spend_daily (agent, day, cost_usd, estimated_cost_usd,
                                  billed_cost_usd, list_price_cost_usd, unclassified_cost_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (agent, day) DO UPDATE SET
             cost_usd = cost_usd + excluded.cost_usd,
             estimated_cost_usd = estimated_cost_usd + excluded.estimated_cost_usd,
             billed_cost_usd = billed_cost_usd + excluded.billed_cost_usd,
             list_price_cost_usd = list_price_cost_usd + excluded.list_price_cost_usd,
             unclassified_cost_usd = unclassified_cost_usd + excluded.unclassified_cost_usd",
        params![
            row.agent.as_str(),
            local_day(&row.at),
            cost,
            estimate,
            billed,
            list_price,
            unclassified
        ],
    )
    .ctx("write daily spend")?;
    Ok(())
}

/// Takes `amount` of cost back from the agent's own days, newest first, so
/// a correction never credits a day with what another agent spent in it.
fn take_from_days(
    conn: &Connection,
    agent: &AetherAgentId,
    amount: f64,
) -> Result<(), ArchonError> {
    assert!(amount.is_finite() && amount > 0.0, "nothing to take back");
    let days: Vec<(String, f64)> = conn
        .prepare_cached("SELECT day, cost_usd FROM spend_daily WHERE agent = ?1 ORDER BY day DESC")
        .ctx("prepare the agent's days")?
        .query_map([agent.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .ctx("read the agent's days")?
        .collect::<Result<_, _>>()
        .ctx("read a day of the agent")?;
    let mut left = amount;
    for (day, cost) in days {
        if left <= 0.0 {
            break;
        }
        let taken = left.min(cost);
        conn.execute(
            "UPDATE spend_daily SET cost_usd = ?3 WHERE agent = ?1 AND day = ?2",
            params![agent.as_str(), day, cost - taken],
        )
        .ctx("correct daily spend")?;
        left -= taken;
    }
    if left > ROUNDING_USD {
        return Err(ArchonError::Store(format!(
            "the days of agent {agent} hold ${left} less than it was charged"
        )));
    }
    Ok(())
}

/// Every dollar of a settled total was reported by the provider, so each day
/// the agent was charged in is billed in full.
fn bill_days(conn: &Connection, agent: &AetherAgentId) -> Result<(), ArchonError> {
    conn.execute(
        "UPDATE spend_daily SET billed_cost_usd = cost_usd, list_price_cost_usd = 0,
                                unclassified_cost_usd = 0
         WHERE agent = ?1",
        [agent.as_str()],
    )
    .ctx("bill the agent's days")?;
    Ok(())
}

/// Whether `row` is Aether's settled figure: every dollar in it reported by
/// the provider, and no invocation left out.
fn settles(row: &SpendRow) -> bool {
    row.cost_basis == CostBasis::ProviderReported && row.cost_complete == Some(true)
}

/// What a new total does to an agent's charge: the row to store, the settled
/// floor after it, the change in cost and the growth in estimate.
struct Charge {
    kept: SpendRow,
    settled: f64,
    cost: f64,
    estimate: f64,
}

/// A settled total replaces the charge down to the settled floor; any other
/// total only raises it. The estimate is never settled, so it only grows.
fn charge(row: &SpendRow, held: &Charged) -> Charge {
    assert!(held.settled <= held.cost, "a charge is at least its floor");
    let settles = settles(row);
    let floor = if settles { held.settled } else { held.cost };
    let mut kept = SpendRow {
        cost_usd: row.cost_usd.max(floor),
        estimated_cost_usd: row.estimated_cost_usd.max(held.estimate),
        ..row.clone()
    };
    if kept != *row {
        tracing::warn!(
            agent = %row.agent,
            cost = row.cost_usd,
            estimate = row.estimated_cost_usd,
            "usage total went down; keeping what was already charged"
        );
    } else if kept.cost_usd < held.cost {
        tracing::debug!(
            agent = %row.agent,
            cost = row.cost_usd,
            "the provider's total replaces a higher estimate"
        );
    }
    if !settles && row.cost_usd < held.cost && row.cost_basis.name() != held.basis {
        // The new provenance describes a smaller amount, not the charge retained
        // by the budget. Its entire split can no longer be established.
        kept.cost_basis = CostBasis::Unknown;
    }
    let settled = if settles { kept.cost_usd } else { held.settled };
    let (cost, estimate) = (
        kept.cost_usd - held.cost,
        kept.estimated_cost_usd - held.estimate,
    );
    assert!(settled <= kept.cost_usd && estimate >= 0.0);
    assert!(
        cost >= 0.0 || settles,
        "only a settled total lowers a charge"
    );
    Charge {
        kept,
        settled,
        cost,
        estimate,
    }
}

/// Charges the agent's growth since its last row. An older row is a replayed event
/// and changes nothing, as does the same row again; only a settled total lowers it.
pub(super) fn record(conn: &mut Connection, row: &SpendRow) -> Result<(), ArchonError> {
    check_row(row)?;
    let tx = write_tx(conn)?;
    if let Some(item) = &row.item {
        super::items::load(&tx, item)?;
    }
    let held = match current(&tx, row)? {
        Some(held) if held.at > ts(&row.at) => {
            tracing::debug!(agent = %row.agent, "ignoring an older usage total");
            return Ok(());
        }
        Some(held) => held,
        None => Charged {
            cost: 0.0,
            estimate: 0.0,
            at: ts(&row.at),
            basis: row.cost_basis.name().to_owned(),
            settled: 0.0,
        },
    };
    let charge = charge(row, &held);
    upsert_latest(&tx, &charge.kept, charge.settled)?;
    add_to_day(&tx, &charge.kept, charge.cost.max(0.0), charge.estimate)?;
    if charge.cost < 0.0 {
        take_from_days(&tx, &row.agent, -charge.cost)?;
    }
    if settles(row) {
        bill_days(&tx, &row.agent)?;
    }
    tx.commit().ctx("commit spend")
}

pub(super) fn for_item(
    conn: &Connection,
    item: &QueueItemId,
    estimates: bool,
) -> Result<f64, ArchonError> {
    super::items::load(conn, item)?;
    let total: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd + CASE WHEN ?2 THEN estimated_cost_usd ELSE 0 END), 0)
             FROM spend WHERE item = ?1",
            params![item.as_str(), estimates],
            |r| r.get(0),
        )
        .ctx("sum item spend")?;
    debug_assert!(total.is_finite());
    Ok(total)
}

/// What the agent has been charged; 0 before its first usage event.
pub(super) fn for_agent(
    conn: &Connection,
    agent: &AetherAgentId,
    estimates: bool,
) -> Result<f64, ArchonError> {
    let total: Option<f64> = conn
        .query_row(
            "SELECT cost_usd + CASE WHEN ?2 THEN estimated_cost_usd ELSE 0 END
             FROM spend WHERE agent = ?1",
            params![agent.as_str(), estimates],
            |r| r.get(0),
        )
        .optional()
        .ctx("read agent spend")?;
    let total = total.unwrap_or(0.0);
    debug_assert!(
        total.is_finite() && total >= 0.0,
        "rows are checked on write"
    );
    Ok(total)
}

pub(super) fn totals(
    conn: &Connection,
    now: &Timestamp,
    estimates: bool,
) -> Result<(f64, f64), ArchonError> {
    let today: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd + CASE WHEN ?2 THEN estimated_cost_usd ELSE 0 END), 0)
             FROM spend_daily WHERE day = ?1",
            params![local_day(now), estimates],
            |r| r.get(0),
        )
        .ctx("sum daily spend")?;
    // The history retention pass carries what it deletes in `spend_pruned`.
    let total: f64 = conn
        .query_row(
            "SELECT (SELECT COALESCE(SUM(cost_usd + CASE WHEN ?1 THEN estimated_cost_usd ELSE 0 END), 0)
                     FROM spend)
                  + (SELECT COALESCE(SUM(cost_usd + CASE WHEN ?1 THEN estimated_cost_usd ELSE 0 END), 0)
                     FROM spend_pruned)",
            [estimates],
            |r| r.get(0),
        )
        .ctx("sum total spend")?;
    debug_assert!(today.is_finite() && total.is_finite());
    Ok((today, total))
}

/// Bind before receiving usage, so descendants and late events retain ownership.
pub(super) fn link_run(
    conn: &Connection,
    agent: &AetherAgentId,
    run: &PipelineRunId,
) -> Result<(), ArchonError> {
    super::runs::load(conn, run)?;
    if let Some(owned) = agent_run(conn, agent)? {
        if owned != *run {
            return Err(ArchonError::Invalid(format!(
                "agent {agent} already belongs to run {owned}"
            )));
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO agent_runs (agent, run) VALUES (?1, ?2)",
        params![agent.as_str(), run.as_str()],
    )
    .ctx("link agent to run")?;
    Ok(())
}

pub(super) fn agent_run(
    conn: &Connection,
    agent: &AetherAgentId,
) -> Result<Option<PipelineRunId>, ArchonError> {
    let (stage, linked): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT st.run, a.run FROM (SELECT ?1 AS agent) input
         LEFT JOIN stage_runs st ON st.aether_agent = input.agent
         LEFT JOIN agent_runs a ON a.agent = input.agent",
            [agent.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ctx("read agent run")?;
    if stage
        .as_ref()
        .zip(linked.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(ArchonError::Store(format!(
            "agent {agent} has inconsistent run ownership"
        )));
    }
    let raw = stage.or(linked);
    raw.map(|raw| {
        PipelineRunId::parse(&raw)
            .map_err(|e| ArchonError::Store(format!("agent {agent} run: {e}")))
    })
    .transpose()
}

pub(super) fn for_run(
    conn: &Connection,
    run: &PipelineRunId,
    estimates: bool,
) -> Result<f64, ArchonError> {
    super::runs::load(conn, run)?;
    let total = conn.query_row(
        "SELECT COALESCE(SUM(s.cost_usd + CASE WHEN ?2 THEN s.estimated_cost_usd ELSE 0 END), 0)
         FROM spend s WHERE s.agent IN (
             SELECT aether_agent FROM stage_runs WHERE run = ?1 AND aether_agent IS NOT NULL
             UNION SELECT agent FROM agent_runs WHERE run = ?1
         )",
        params![run.as_str(), estimates], |r| r.get(0)).ctx("sum run spend")?;
    Ok(total)
}
