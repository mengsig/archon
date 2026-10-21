//! The blockers posted for an item, kept so they can be resolved when the
//! item moves on: nothing else closes a blocker archond posted, so one
//! would count as waiting in Aether's center long after it was acted on.

use archon_aether::AetherClient;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QueueItemId};
use archon_queue::store::keys;

use crate::context::DaemonContext;

fn key(item: &QueueItemId) -> String {
    format!("{}{item}", keys::NOTIFICATION_CENTER_BLOCKERS)
}

fn load(store: &archon_queue::Store, item: &QueueItemId) -> Result<Vec<String>, ArchonError> {
    let Some(raw) = store.kv_get(&key(item))? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("blockers posted for item {item}: {e}")))
}

/// Note that the blocker `posted` is open for `item`.
pub(super) async fn remember(
    ctx: &DaemonContext,
    item: &QueueItemId,
    posted: &NotificationId,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut keys = load(&store, item)?;
    if !keys.iter().any(|k| k == posted.as_str()) {
        keys.push(posted.as_str().to_owned());
    }
    assert!(!keys.is_empty(), "the blocker just posted is listed");
    store.kv_set(&key(item), &serde_json::to_string(&keys)?)
}

/// Resolve every blocker posted for `item`. The list is forgotten only once
/// all are resolved, so a failure part-way is repeated on the next pass.
pub(super) async fn resolve(
    ctx: &DaemonContext,
    client: &AetherClient,
    item: &QueueItemId,
) -> Result<(), ArchonError> {
    let keys = load(&*ctx.store.lock().await, item)?;
    for posted in &keys {
        client.resolve_notification(posted).await?;
    }
    if keys.is_empty() {
        return Ok(());
    }
    ctx.store.lock().await.kv_delete(&key(item))
}

/// Resolve every tracked blocker, then every other open blocker and outcome
/// Aether lists. A tracked list is forgotten only once all its keys are
/// resolved, so a blocker older than Aether's listing window is still closed.
pub(super) async fn resolve_all(
    ctx: &DaemonContext,
    client: &AetherClient,
) -> Result<(), ArchonError> {
    let tracked = ctx
        .store
        .lock()
        .await
        .kv_prefix(keys::NOTIFICATION_CENTER_BLOCKERS)?;
    for (listed, raw) in &tracked {
        let posted: Vec<String> = serde_json::from_str(raw)
            .map_err(|e| ArchonError::Corrupt(format!("blockers listed under {listed}: {e}")))?;
        for key in &posted {
            client.resolve_notification(key).await?;
        }
        ctx.store.lock().await.kv_delete(listed)?;
    }
    let held = client
        .archon_notifications(archon_aether::notifications::MAX_LIST)
        .await?;
    let open = held
        .iter()
        .filter(|n| n.is_open() && matches!(n.kind.as_str(), "blocker" | "outcome"))
        .filter_map(|n| n.key.as_deref());
    for key in open {
        client.resolve_notification(key).await?;
    }
    Ok(())
}
