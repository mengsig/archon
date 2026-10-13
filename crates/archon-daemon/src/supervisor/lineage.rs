//! Following the orchestrator through Aether's restarts (ARCHITECTURE §16):
//! each leaves `successor` on the old agent, and several leave a chain.

use archon_aether::AetherClient;
use archon_aether::wire::Agent;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;

use super::ORCHESTRATOR_ROLE;

/// The orchestrator a stored id leads to, and the agents left behind on the
/// way: each was restarted into the next, so only `newest` is in charge.
#[derive(Debug)]
pub(super) struct Lineage {
    pub newest: Agent,
    /// Oldest first, starting with the stored agent.
    pub retired: Vec<AetherAgentId>,
}

/// Follow `id` to its newest incarnation: the last of its chain that is an
/// orchestrator Aether still knows. `NotFound` only when Aether forgot `id`.
pub(super) async fn newest_incarnation(
    client: &AetherClient,
    id: &AetherAgentId,
) -> Result<Lineage, ArchonError> {
    let mut newest = client.get_agent(id).await?;
    let mut retired = Vec::new();
    while let Some(next) = newest.successor.clone() {
        // Each restart makes a new agent, so a chain never repeats one.
        if next == newest.id || retired.contains(&next) {
            return Err(ArchonError::Protocol(format!(
                "the restarts of orchestrator {id} lead back to {next}"
            )));
        }
        let successor = match client.get_agent(&next).await {
            Ok(successor) => successor,
            Err(ArchonError::NotFound(why)) => {
                tracing::warn!(from = %newest.id, to = %next, %why,
                    "Aether no longer knows the orchestrator's successor; the chain ends before it");
                break;
            }
            Err(error) => return Err(error),
        };
        if successor.role != ORCHESTRATOR_ROLE {
            tracing::warn!(from = %newest.id, to = %next, role = %successor.role,
                "the orchestrator's successor is not an orchestrator; not adopting it");
            break;
        }
        retired.push(newest.id);
        newest = successor;
    }
    assert!(
        !retired.contains(&newest.id),
        "the agent in charge is never among those it replaced"
    );
    assert!(
        retired.first().is_none_or(|first| first == id),
        "the chain starts at the stored agent"
    );
    Ok(Lineage { newest, retired })
}
