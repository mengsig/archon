//! Citations are looked up in archond's own durable event observations.

use archon_core::aether_ids::AetherAgentId;
use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;

use crate::context::DaemonContext;

const PREFIX: &str = "operator_message:";
pub(crate) const MAX_MESSAGE_CHARS: usize = 4000;

fn key(agent: &AetherAgentId, seq: u64) -> String {
    format!("{PREFIX}{agent}:{seq}")
}

pub(crate) async fn record(
    ctx: &DaemonContext,
    message: &OperatorMessage,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let key = key(&message.agent, message.seq);
    // Replays cannot change the original provenance or renew consent.
    if store.kv_get(&key)?.is_none() {
        store.kv_set(&key, &serde_json::to_string(message)?)?;
    }
    let cutoff = archon_core::now() - chrono::Duration::days(7);
    for (key, raw) in store.kv_prefix(PREFIX)? {
        let old: OperatorMessage = serde_json::from_str(&raw)?;
        if old.at < cutoff {
            store.kv_delete(&key)?;
        }
    }
    Ok(())
}
