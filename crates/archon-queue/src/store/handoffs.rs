//! What a kept agent (§6.7) and the agents beneath it had been charged when it passed
//! to a later attempt: where that attempt's cost starts, as the ledger holds only totals.

use std::collections::BTreeMap;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::run::StageRun;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::AgentHandoff;
use super::codec::{SqlContext, from_json, to_json};

/// `kv` keys: `<PREFIX><agent>/<attempt, zero-padded>`, so key order is attempt order.
const PREFIX: &str = "agent-handoff/";

#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    attempt: u32,
    /// Each charged agent of the subtree: `(cost, estimate)`.
    spent: BTreeMap<String, (f64, f64)>,
}

fn key(agent: &AetherAgentId, attempt: u32) -> String {
    format!("{PREFIX}{}/{attempt:010}", agent.as_str())
}

/// Record what `agent` and every agent beneath it had been charged as it
/// passes to `next`, a later attempt or the replacement of its own that woke
/// it; the first record of an attempt stands.
pub(super) fn record(
    conn: &Connection,
    agent: &AetherAgentId,
    next: &StageRun,
) -> Result<(), ArchonError> {
    assert_eq!(next.aether_agent.as_ref(), Some(agent), "it passes to next");
    assert!(next.attempt >= 1, "attempts are 1-based");
    let mut statement = conn
        .prepare_cached(
            "WITH RECURSIVE subtree(agent) AS (
                 SELECT ?1
                 UNION
                 SELECT d.agent FROM delegated_agents d JOIN subtree t ON d.parent = t.agent
             ) SELECT s.agent, s.cost_usd, s.estimated_cost_usd
               FROM subtree t JOIN spend s ON s.agent = t.agent",
        )
        .ctx("prepare a kept agent's charges")?;
    let rows = statement
        .query_map([agent.as_str()], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))
        })
        .ctx("read a kept agent's charges")?;
    let spent: BTreeMap<String, (f64, f64)> = rows
        .collect::<Result<_, _>>()
        .ctx("read a kept agent's charges")?;
    assert!(
        spent
            .values()
            .all(|&(cost, estimate)| cost >= 0.0 && estimate >= 0.0),
        "the ledger holds no negative charge"
    );
    let recorded = Recorded {
        attempt: next.attempt,
        spent,
    };
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO NOTHING",
        params![
            key(agent, next.attempt),
            to_json(&recorded, "agent hand-off")?
        ],
    )
    .ctx("record an agent hand-off")?;
    Ok(())
}

/// Every hand-off of `agent`, by attempt; a charge counts its estimate when `estimates`.
pub(super) fn of_agent(
    conn: &Connection,
    agent: &AetherAgentId,
    estimates: bool,
) -> Result<Vec<AgentHandoff>, ArchonError> {
    let from = format!("{PREFIX}{}/", agent.as_str());
    // `0` is the character after `/`: every key under `from` sorts before it.
    let until = format!("{PREFIX}{}0", agent.as_str());
    let mut statement = conn
        .prepare_cached("SELECT value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key")
        .ctx("prepare agent hand-offs")?;
    let rows = statement
        .query_map(params![from, until], |r| r.get::<_, String>(0))
        .ctx("read agent hand-offs")?;
    let mut handoffs = Vec::new();
    for raw in rows {
        let recorded: Recorded = from_json(&raw.ctx("read an agent hand-off")?, "agent hand-off")?;
        handoffs.push(decode(recorded, estimates)?);
    }
    assert!(
        handoffs.windows(2).all(|w| w[0].attempt < w[1].attempt),
        "hand-offs come in attempt order, one per attempt"
    );
    Ok(handoffs)
}

fn decode(recorded: Recorded, estimates: bool) -> Result<AgentHandoff, ArchonError> {
    let mut spent = BTreeMap::new();
    for (agent, (cost, estimate)) in recorded.spent {
        let agent = AetherAgentId::parse(&agent)
            .map_err(|e| ArchonError::Store(format!("agent hand-off: {e}")))?;
        let charged = cost + if estimates { estimate } else { 0.0 };
        if !charged.is_finite() || charged < 0.0 {
            return Err(ArchonError::Store(format!(
                "agent hand-off of {agent} holds the invalid charge {charged}"
            )));
        }
        spent.insert(agent, charged);
    }
    Ok(AgentHandoff {
        attempt: recorded.attempt,
        spent,
    })
}
