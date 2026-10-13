//! The agents a stage agent delegated to, at any depth: who created each, on which
//! model and effort. A stage's cost is its agent's spend plus theirs, split by these rows.

use std::str::FromStr;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelRef};
use archon_core::protocol::DelegatedAgent;
use rusqlite::{Connection, OptionalExtension, params};

use super::codec::SqlContext;

/// Every agent beneath a root, a row per pair; `UNION` ends a cycle a
/// corrupted table could hold.
pub(super) const BENEATH: &str = "beneath(root, agent) AS (
    SELECT parent, agent FROM delegated_agents
    UNION
    SELECT b.root, d.agent FROM delegated_agents d JOIN beneath b ON d.parent = b.agent
)";

/// Record that `parent` delegated to `agent`. An effort already known is
/// kept when `effort` is `None`; an agent keeps the parent it was given.
pub(super) fn record(
    conn: &Connection,
    agent: &AetherAgentId,
    parent: &AetherAgentId,
    model: &ModelRef,
    effort: Option<Effort>,
) -> Result<(), ArchonError> {
    assert_ne!(agent, parent, "an agent is not its own parent");
    let known: Option<String> = conn
        .query_row(
            "SELECT parent FROM delegated_agents WHERE agent = ?1",
            [agent.as_str()],
            |r| r.get(0),
        )
        .optional()
        .ctx("read delegated agent")?;
    if known
        .as_deref()
        .is_some_and(|known| known != parent.as_str())
    {
        return Err(ArchonError::Invalid(format!(
            "agent {agent} was already delegated to by {}",
            known.unwrap_or_default()
        )));
    }
    conn.execute(
        "INSERT INTO delegated_agents (agent, parent, model, effort) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent) DO UPDATE SET
             model = excluded.model, effort = COALESCE(excluded.effort, effort)",
        params![
            agent.as_str(),
            parent.as_str(),
            model.to_string(),
            effort.map(|e| e.to_string())
        ],
    )
    .ctx("record delegated agent")?;
    Ok(())
}

/// Every agent beneath `root` with what the ledger charged it, in the order
/// they were first recorded.
pub(super) fn beneath(
    conn: &Connection,
    root: &AetherAgentId,
    estimates: bool,
) -> Result<Vec<DelegatedAgent>, ArchonError> {
    let sql = format!(
        "WITH RECURSIVE {BENEATH}
         SELECT d.agent, d.parent, d.model, d.effort,
                COALESCE(s.cost_usd + CASE WHEN ?2 THEN s.estimated_cost_usd ELSE 0 END, 0)
         FROM beneath b JOIN delegated_agents d ON d.agent = b.agent
         LEFT JOIN spend s ON s.agent = d.agent
         WHERE b.root = ?1 ORDER BY d.rowid"
    );
    let mut statement = conn.prepare(&sql).ctx("prepare delegated agents")?;
    let rows = statement
        .query_map(params![root.as_str(), estimates], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .ctx("query delegated agents")?;
    let mut found = Vec::new();
    for row in rows {
        found.push(decode(row.ctx("read delegated agent")?)?);
    }
    debug_assert!(found.iter().all(|d| d.agent != *root));
    Ok(found)
}

type Row = (String, String, String, Option<String>, f64);

/// A stored row; one that no longer parses is corrupted storage.
fn decode((agent, parent, model, effort, cost_usd): Row) -> Result<DelegatedAgent, ArchonError> {
    let corrupt = |what: &str, e: ArchonError| ArchonError::Store(format!("delegated {what}: {e}"));
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return Err(ArchonError::Store(format!(
            "delegated agent {agent} has invalid cost {cost_usd}"
        )));
    }
    Ok(DelegatedAgent {
        agent: AetherAgentId::parse(&agent).map_err(|e| corrupt("agent", e))?,
        parent: AetherAgentId::parse(&parent).map_err(|e| corrupt("parent", e))?,
        model: ModelRef::from_str(&model).map_err(|e| corrupt("model", e))?,
        effort: effort
            .map(|raw| Effort::from_str(&raw).map_err(|e| corrupt("effort", e)))
            .transpose()?,
        cost_usd,
    })
}
