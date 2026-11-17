//! Read the backend's delegated-agent lifecycle without changing agent authority.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::protocol::{ItemAgentState, ItemAgentStates, ItemAgentStatus, Reply};

use crate::context::DaemonContext;

const MAX_ROOTS: usize = 64;
const MAX_AGENTS: usize = 2048;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn get(ctx: &DaemonContext, item: &QueueItemId) -> Result<Reply, ArchonError> {
    let roots = stage_agents(ctx, item).await?;
    let agents = tokio::time::timeout(READ_TIMEOUT, read_agents(ctx, &roots))
        .await
        .map_err(|_| {
            ArchonError::Unavailable(format!(
                "item_agent_states for {item} exceeded {} seconds",
                READ_TIMEOUT.as_secs()
            ))
        })??;
    assert!(agents.len() <= MAX_AGENTS, "the response is bounded");
    assert!(
        agents.windows(2).all(|pair| pair[0].agent < pair[1].agent),
        "agent ids are unique and sorted"
    );
    let cost_usd = recorded_costs(ctx, &roots, &agents).await?;
    Ok(Reply::ItemAgentStates(ItemAgentStates { agents, cost_usd }))
}

async fn stage_agents(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<BTreeSet<AetherAgentId>, ArchonError> {
    let store = ctx.read_store().await?;
    let stored = store.item(item)?;
    assert_eq!(&stored.id, item, "the requested item exists");
    let mut roots = BTreeSet::new();
    for run in store.runs_for_item(item)? {
        assert_eq!(&run.item, item, "only this item's runs are queried");
        for stage in store.stage_runs_strict(&run.id)? {
            assert_eq!(stage.run, run.id, "only this run's stages are queried");
            if let Some(agent) = stage.aether_agent {
                roots.insert(agent);
                if roots.len() > MAX_ROOTS {
                    return Err(ArchonError::Unavailable(format!(
                        "item_agent_states for {item} exceeds {MAX_ROOTS} stage agents"
                    )));
                }
            }
        }
    }
    Ok(roots)
}

async fn read_agents(
    ctx: &DaemonContext,
    roots: &BTreeSet<AetherAgentId>,
) -> Result<Vec<ItemAgentStatus>, ArchonError> {
    assert!(
        roots.len() <= MAX_ROOTS,
        "stage roots were bounded before I/O"
    );
    let mut agents = BTreeMap::new();
    for root in roots {
        let descendants = ctx.backend.descendants(root).await.map_err(|error| {
            error.with_detail(&format!("reading item_agent_states beneath {root}"))
        })?;
        validate_tree(root, &descendants)?;
        for child in descendants {
            if roots.contains(&child.agent) {
                continue;
            }
            let state = match child.state {
                DescendantState::Live => ItemAgentState::Live,
                DescendantState::Completed => ItemAgentState::Completed,
                DescendantState::Stopped => ItemAgentState::Stopped,
            };
            if let Some(previous) = agents.insert(child.agent, state)
                && previous != state
            {
                return Err(ArchonError::Unavailable(
                    "item_agent_states changed between overlapping stage trees; retry".into(),
                ));
            }
            if agents.len() > MAX_AGENTS {
                return Err(ArchonError::Unavailable(format!(
                    "item_agent_states exceeds {MAX_AGENTS} delegated agents"
                )));
            }
        }
    }
    assert!(agents.keys().all(|agent| !roots.contains(agent)));
    Ok(agents
        .into_iter()
        .map(|(agent, state)| ItemAgentStatus { agent, state })
        .collect())
}

fn validate_tree(root: &AetherAgentId, descendants: &[Descendant]) -> Result<(), ArchonError> {
    if descendants.len() > MAX_AGENTS {
        return Err(ArchonError::Unavailable(format!(
            "item_agent_states beneath {root} exceeds {MAX_AGENTS} delegated agents"
        )));
    }
    let mut children: BTreeMap<&AetherAgentId, Vec<&AetherAgentId>> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for child in descendants {
        if child.agent == *root || !ids.insert(&child.agent) {
            return Err(ArchonError::Aether(format!(
                "item_agent_states beneath {root} contains a duplicate or its own root"
            )));
        }
        children
            .entry(&child.parent)
            .or_default()
            .push(&child.agent);
    }
    let mut pending = vec![root];
    let mut visited = BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !visited.insert(parent) {
            return Err(ArchonError::Aether(format!(
                "item_agent_states beneath {root} contains a cycle"
            )));
        }
        if let Some(next) = children.get(parent) {
            pending.extend(next.iter().copied());
        }
    }
    if visited.len() != descendants.len() + 1 {
        return Err(ArchonError::Aether(format!(
            "item_agent_states beneath {root} contains agents outside that tree"
        )));
    }
    assert!(visited.contains(root));
    assert!(ids.iter().all(|agent| visited.contains(agent)));
    Ok(())
}

#[cfg(test)]
#[path = "item_agents_tests.rs"]
mod tests;

async fn recorded_costs(
    ctx: &DaemonContext,
    roots: &BTreeSet<AetherAgentId>,
    agents: &[ItemAgentStatus],
) -> Result<BTreeMap<AetherAgentId, f64>, ArchonError> {
    let mut ids = roots.clone();
    ids.extend(agents.iter().map(|agent| agent.agent.clone()));
    if let Some((agent, _)) = super::control::recorded_orchestrator(ctx).await? {
        ids.insert(agent);
    }
    assert!(ids.len() <= MAX_ROOTS + MAX_AGENTS + 1);
    let store = ctx.read_store().await?;
    let mut costs = BTreeMap::new();
    for agent in ids {
        let cost = store.spend_for_agent(&agent, ctx.config.limits.count_estimated_cost)?;
        assert!(
            cost.is_finite() && cost >= 0.0,
            "recorded costs are nonnegative"
        );
        costs.insert(agent, cost);
    }
    Ok(costs)
}
