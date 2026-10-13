//! The orchestrators Archon replaced (ARCHITECTURE §16): on record for their
//! late events, and owed a sweep of what they left running until it is made.

use std::sync::Arc;

use archon_aether::AetherClient;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_queue::store::Store;
use serde::{Deserialize, Serialize};

use super::revival::pause_after;
use crate::context::DaemonContext;
use crate::feed;

/// `kv`: every [`Retired`] orchestrator still inside the retention window.
const RETIRED_KEY: &str = "orchestrator_retired";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Retired {
    agent: AetherAgentId,
    at: Timestamp,
    /// Aether has yet to acknowledge cancelling what it left running.
    owed: bool,
    /// The operator was told that this sweep keeps failing.
    #[serde(default)]
    reported: bool,
}

/// Whether what a retired orchestrator left running is still to be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sweep {
    Owed,
    /// Aether already cancelled its subtree, with it or at Archon's request.
    Done,
}

/// The stored record, or the value that could not be read as one.
enum Stored {
    Readable(Vec<Retired>),
    Unreadable { raw: String, why: String },
}

/// Cancel what `replaced`, a stopped orchestrator, still has running. Aether
/// cancels a stopped agent's live descendants.
pub(super) async fn cancel_orphans(
    client: &AetherClient,
    replaced: &AetherAgentId,
) -> Result<(), ArchonError> {
    let reason = "its parent orchestrator was replaced";
    match client.cancel(replaced, reason).await {
        Ok(()) | Err(ArchonError::NotFound(_) | ArchonError::InvalidTransition(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Put `agents` on record as replaced. Recorded before the replacement is
/// adopted, so no outcome of the adoption loses a sweep.
pub(super) async fn retire(
    ctx: &DaemonContext,
    agents: &[AetherAgentId],
    sweep: Sweep,
) -> Result<(), ArchonError> {
    assert!(!agents.is_empty(), "a replacement retires an agent");
    let now = archon_core::now();
    update(ctx, |record| {
        for agent in agents {
            match record.iter_mut().find(|known| &known.agent == agent) {
                // A sweep once owed stays owed until Aether acknowledges it.
                Some(known) => known.owed |= sweep == Sweep::Owed,
                None => record.push(Retired {
                    agent: agent.clone(),
                    at: now,
                    owed: sweep == Sweep::Owed,
                    reported: false,
                }),
            }
        }
    })
    .await?;
    if sweep == Sweep::Owed {
        ctx.supervisor.sweeps.notify_one();
    }
    Ok(())
}

/// Whether `agent` is an orchestrator Archon replaced within the retention
/// window: its children and its spend are still Archon's.
pub(crate) async fn is_retired(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<bool, ArchonError> {
    update(ctx, |record| {
        record.iter().any(|known| &known.agent == agent)
    })
    .await
}

/// Make every owed sweep but the live orchestrator's own; `true` once none is
/// owed. One Aether refuses is said once and left to [`sweep_when_owed`].
pub(super) async fn sweep(ctx: &DaemonContext, client: &AetherClient) -> Result<bool, ArchonError> {
    let live = super::current_orchestrator(ctx).await?;
    let record = update(ctx, |record| record.clone()).await?;
    let mut swept = Vec::new();
    let mut failed = Vec::new();
    for retired in record.into_iter().filter(|retired| retired.owed) {
        // An adoption that did not complete leaves its predecessor in charge.
        if live.as_ref() == Some(&retired.agent) {
            continue;
        }
        match cancel_orphans(client, &retired.agent).await {
            Ok(()) => swept.push(retired),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => failed.push((retired, error)),
        }
    }
    settle(ctx, &swept, &failed).await
}

/// Record what one pass did and tell the operator what changed: a sweep
/// that failed for the first time, and failing sweeps that went through.
async fn settle(
    ctx: &DaemonContext,
    swept: &[Retired],
    failed: &[(Retired, ArchonError)],
) -> Result<bool, ArchonError> {
    let none_owed = update(ctx, |record| {
        for known in record.iter_mut() {
            if swept.iter().any(|done| done.agent == known.agent) {
                known.owed = false;
                known.reported = false;
            }
            known.reported |= failed.iter().any(|(owed, _)| owed.agent == known.agent);
        }
        !record.iter().any(|known| known.owed)
    })
    .await?;
    for (retired, error) in failed.iter().filter(|(retired, _)| !retired.reported) {
        tracing::warn!(agent = %retired.agent, %error, "a replaced orchestrator's children were not swept");
        let text = format!(
            "Archon could not cancel what the replaced orchestrator {} left running ({error}); \
             it keeps retrying. `archon aether agents` shows what still runs.",
            retired.agent
        );
        feed::system(ctx, text).await?;
    }
    if swept.iter().any(|retired| retired.reported) {
        feed::system(ctx, "Cancelled what a replaced orchestrator left running.").await?;
    }
    Ok(none_owed)
}

/// One pass over the owed sweeps, without the lifecycle lock. `true` once none
/// is owed; `false` also while Aether is unreachable.
pub async fn sweep_retired(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    if !update(ctx, |record| record.iter().any(|known| known.owed)).await? {
        return Ok(true);
    }
    // Unreachable Aether: the sweeps stay owed and the next pass asks again.
    let Ok(client) = ctx.aether.client() else {
        return Ok(false);
    };
    sweep(ctx, &client).await
}

/// Makes owed sweeps from archond's start on, pacing a failing streak like
/// revivals, waiting for [`sweep_soon`] without Aether. Ends on a store error.
pub async fn sweep_when_owed(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut failures = 0u32;
    loop {
        if !ctx.aether.is_connected() || sweep_retired(&ctx).await? {
            failures = 0;
            ctx.supervisor.sweeps.notified().await;
            continue;
        }
        failures = failures.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(pause_after(failures)) => {}
            // A newly owed sweep is tried at once.
            () = ctx.supervisor.sweeps.notified() => {}
        }
    }
}

/// Have [`sweep_when_owed`] look now: Aether is reachable again.
pub fn sweep_soon(ctx: &DaemonContext) {
    ctx.supervisor.sweeps.notify_one();
}

/// Forget the orchestrators replaced before `cutoff` whose sweep is made:
/// none of their events can still arrive. Returns how many were forgotten.
pub(crate) async fn prune(ctx: &DaemonContext, cutoff: Timestamp) -> Result<usize, ArchonError> {
    update(ctx, |record| {
        let before = record.len();
        record.retain(|retired| retired.owed || retired.at >= cutoff);
        before - record.len()
    })
    .await
}

/// Read, change and write the record under one store lock. An unreadable
/// record is reset, and the operator is told what that loses.
async fn update<T>(
    ctx: &DaemonContext,
    change: impl FnOnce(&mut Vec<Retired>) -> T,
) -> Result<T, ArchonError> {
    let (result, lost) = {
        let mut store = ctx.store.lock().await;
        let (mut record, lost) = match read(&store)? {
            Stored::Readable(record) => (record, None),
            Stored::Unreadable { raw, why } => (Vec::new(), Some((raw, why))),
        };
        let before = record.clone();
        let result = change(&mut record);
        if record != before || lost.is_some() {
            write(&mut store, &record)?;
        }
        (result, lost)
    };
    if let Some((raw, why)) = lost {
        tracing::error!(%raw, %why, "the record of replaced orchestrators was unreadable; reset");
        let text = format!(
            "Archon's record of replaced orchestrators was unreadable ({why}) and was reset. \
             Agents they delegated to may still run, and their spend is no longer counted: \
             check `archon aether agents`. The record was: {raw}"
        );
        feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    }
    Ok(result)
}

fn read(store: &Store) -> Result<Stored, ArchonError> {
    let Some(raw) = store.kv_get(RETIRED_KEY)?.filter(|v| !v.is_empty()) else {
        return Ok(Stored::Readable(Vec::new()));
    };
    Ok(match serde_json::from_str(&raw) {
        Ok(record) => Stored::Readable(record),
        Err(error) => Stored::Unreadable {
            raw,
            why: error.to_string(),
        },
    })
}

fn write(store: &mut Store, record: &[Retired]) -> Result<(), ArchonError> {
    if record.is_empty() {
        return store.kv_delete(RETIRED_KEY);
    }
    store.kv_set(RETIRED_KEY, &serde_json::to_string(record)?)
}
