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
    let now = archon_core::now();
    const PRUNED: &str = "operator_messages_pruned_at";
    if let Some(last) = store.kv_get(PRUNED)? {
        let last: archon_core::Timestamp = serde_json::from_str(&last)?;
        if now - last < chrono::Duration::hours(1) {
            return Ok(());
        }
    }
    let cutoff = now - chrono::Duration::days(7);
    for (key, raw) in store.kv_prefix(PREFIX)? {
        let old: OperatorMessage = serde_json::from_str(&raw)?;
        if old.at < cutoff {
            store.kv_delete(&key)?;
        }
    }
    store.kv_set(PRUNED, &serde_json::to_string(&now)?)?;
    Ok(())
}

pub(crate) async fn cite(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    seq: u64,
    quote: &str,
) -> Result<OperatorMessage, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(&key(agent, seq))?.ok_or_else(|| {
        ArchonError::Forbidden("the cited inbox message is not a verified operator message; ask the operator again".into())
    })?;
    let message: OperatorMessage = serde_json::from_str(&raw)?;
    if !message.channel.is_github()
        && ctx.config.aether.mode != archon_core::config::AetherMode::Dedicated
    {
        return Err(ArchonError::Forbidden(
            "operator chat citations are disabled in shared mode".into(),
        ));
    }
    let age = archon_core::now() - message.at;
    if age < chrono::Duration::zero() || age > chrono::Duration::minutes(60) {
        return Err(ArchonError::Forbidden(
            "the operator citation is outside its 60-minute window; ask again".into(),
        ));
    }
    if quote.trim().is_empty() || !message.text.contains(quote) {
        return Err(ArchonError::Forbidden(
            "quote must occur verbatim in the verified operator message".into(),
        ));
    }
    Ok(message)
}
