//! While Aether names no caller of Archon's tools (ARCHITECTURE §11): the
//! orchestrator gets no native delegation, so that its calls stay its own.

use archon_aether::wire::Agent;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;

use crate::context::DaemonContext;
use crate::feed;

/// Set by an unnamed call that changes Archon, cleared by the orchestrator's
/// next named call of any kind. Kept between them: the operator is told once.
const UNNAMED_KEY: &str = "mcp_callers_unnamed";
/// The orchestrator archond has seen to be unable to delegate natively.
const SOLE_CALLER_KEY: &str = "orchestrator_sole_caller";

/// Appended to the instructions of an orchestrator started without `delegate`.
pub(super) const NO_DELEGATION_NOTE: &str = "\n\n## Native delegation is off\n\nAether does not \
    name the agents that call Archon's tools, so you were started without `delegate`. Whatever \
    this prompt says about delegating, do the work yourself, queue it as an item, or use `ask`.\n";

/// Remember whether `agent`, the orchestrator from now on, can delegate
/// natively. Only when it cannot is an unnamed call certainly its own.
pub(super) async fn record_delegation(
    ctx: &DaemonContext,
    agent: &Agent,
) -> Result<(), ArchonError> {
    let sole = (!agent.config.can_delegate()).then(|| agent.id.to_string());
    let mut store = ctx.store.lock().await;
    if store.kv_get(SOLE_CALLER_KEY)? == sole {
        return Ok(());
    }
    match sole {
        Some(id) => store.kv_set(SOLE_CALLER_KEY, &id),
        None => store.kv_delete(SOLE_CALLER_KEY),
    }
}

/// Whether the next orchestrator may be given `delegate`.
pub(super) async fn are_named(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    Ok(ctx.read_store().await.kv_get(UNNAMED_KEY)?.is_none())
}

/// An unnamed call that changes Archon arrived. `true` when it can only be
/// `orchestrator`'s own; the operator is told the first time either way.
pub(super) async fn unnamed_is_from(
    ctx: &DaemonContext,
    orchestrator: Option<&AetherAgentId>,
) -> Result<bool, ArchonError> {
    let (first, sole) = {
        let mut store = ctx.store.lock().await;
        let first = store.kv_get(UNNAMED_KEY)?.is_none();
        store.kv_set(UNNAMED_KEY, "1")?;
        (first, store.kv_get(SOLE_CALLER_KEY)?)
    };
    if first {
        tracing::warn!("an MCP call that changes Archon named no caller");
        feed::notify(ctx, NotificationKind::Blocker, notice(ctx), None, None).await?;
    }
    Ok(sole.is_some() && sole.as_deref() == orchestrator.map(AetherAgentId::as_str))
}

/// The orchestrator made a named call, a read included: Aether names callers
/// (again), which only the registration that sends them can do.
pub(super) async fn named(
    ctx: &DaemonContext,
    orchestrator: &AetherAgentId,
) -> Result<(), ArchonError> {
    let sole = {
        let mut store = ctx.store.lock().await;
        if store.kv_get(UNNAMED_KEY)?.is_none() {
            return Ok(());
        }
        store.kv_delete(UNNAMED_KEY)?;
        store.kv_get(SOLE_CALLER_KEY)?
    };
    let mut text = "Aether names the agents that call Archon's tools again.".to_owned();
    if sole.as_deref() == Some(orchestrator.as_str()) {
        text.push_str(
            " This orchestrator was started without native delegation; \
             `archon orchestrator restart` starts one with it.",
        );
    }
    feed::system(ctx, text).await.map(drop)
}

/// What the operator is told, once, and how to fix it in their mode.
fn notice(ctx: &DaemonContext) -> String {
    format!(
        "Aether is not naming the agents that call Archon's tools, so archond cannot tell its \
         orchestrator from an agent it delegated to. {} Until then Archon accepts such calls \
         only from an orchestrator that cannot delegate natively, and starts new orchestrators \
         without native delegation.",
        fix(ctx.config.aether.mode)
    )
}

/// Aether reads its MCP settings only as it starts, so every fix ends in
/// a restart of the daemon that runs the `archon` server.
pub(super) fn fix(mode: AetherMode) -> &'static str {
    match mode {
        AetherMode::Dedicated => {
            "Restart Archon's own aetherd, which reads its MCP settings only as it starts: \
             `archon daemon stop --aether`, then `archon daemon start` starts it again and \
             brings the registration up to date first."
        }
        AetherMode::Shared => {
            "Add `send_caller_meta = true` to the `archon` MCP server in Aether's config \
             (`archon doctor` names the file), then restart aetherd: it reads its MCP \
             settings only as it starts, so the edit alone changes nothing."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator edited the file and nothing changed: Aether had not
    /// been restarted. No fix may read as done with the edit.
    #[test]
    fn every_fix_ends_in_a_restart_of_aether() {
        for mode in [AetherMode::Dedicated, AetherMode::Shared] {
            let fix = fix(mode).to_lowercase();
            assert!(fix.contains("restart"), "{fix}");
            assert!(fix.contains("only as it starts"), "{fix}");
        }
        assert!(fix(AetherMode::Dedicated).contains("`archon daemon stop --aether`"));
        assert!(fix(AetherMode::Shared).contains("the edit alone changes nothing"));
    }
}
