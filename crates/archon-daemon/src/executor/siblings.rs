//! Stopping the agents of team roles cancelled because a sibling failed.
//! Each stop is stored with its stage run's cancel, so a crash or an
//! unreachable Aether only delays it: the sweep repeats it until Aether
//! confirms or no longer knows the agent.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::FollowUp;
use crate::context::DaemonContext;

/// `kv`: `sibling_stop:<stage run>` → the JSON [`OwedStop`].
const STOP_KEY_PREFIX: &str = "sibling_stop:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OwedStop {
    agent: AetherAgentId,
    reason: String,
}

fn stop_key(stage: &StageRunId) -> String {
    format!("{STOP_KEY_PREFIX}{stage}")
}

/// Owe a stop of `agent`, the cancelled `stage`'s; store it in the same
/// store lock as the cancel.
pub(super) fn owe(
    store: &mut Store,
    stage: &StageRunId,
    agent: &AetherAgentId,
    reason: &str,
) -> Result<(), ArchonError> {
    assert!(!reason.trim().is_empty(), "a sibling stop says why");
    let owed = OwedStop {
        agent: agent.clone(),
        reason: reason.to_owned(),
    };
    store.kv_set(&stop_key(stage), &serde_json::to_string(&owed)?)
}

/// Whether the stop of `stage`'s agent is still owed.
pub(super) fn owes(store: &Store, stage: &StageRunId) -> Result<bool, ArchonError> {
    Ok(store.kv_get(&stop_key(stage))?.is_some())
}

/// Stage runs whose agent stop is still owed.
pub(super) fn owed(store: &Store) -> Result<Vec<StageRunId>, ArchonError> {
    let mut stages = Vec::new();
    for (key, _) in store.kv_prefix(STOP_KEY_PREFIX)? {
        let raw = key.strip_prefix(STOP_KEY_PREFIX).expect("ranged by prefix");
        let stage = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("sibling stop key {key}: {e}")))?;
        stages.push(stage);
    }
    Ok(stages)
}

/// Cancel the owed agent of `stage`; forget the stop once Aether confirms,
/// no longer knows the agent, or refuses because it already ended.
pub(super) async fn stop(ctx: &DaemonContext, stage: &StageRunId) -> Result<FollowUp, ArchonError> {
    let key = stop_key(stage);
    let Some(raw) = ctx.store.lock().await.kv_get(&key)? else {
        return Ok(FollowUp::default());
    };
    let owed: OwedStop = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("sibling stop of stage run {stage}: {e}")))?;
    match ctx.backend.cancel(&owed.agent, &owed.reason).await {
        Ok(()) => {}
        Err(ArchonError::NotFound(why)) => {
            tracing::info!(agent = %owed.agent, %why, "cancelled sibling already gone");
        }
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => {
            tracing::info!(agent = %owed.agent, %error, "sibling stop failed; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(refusal) => {
            tracing::warn!(agent = %owed.agent, %refusal, "Aether refused to stop a cancelled sibling; it has most likely ended");
        }
    }
    let build = {
        let mut store = ctx.store.lock().await;
        store.kv_delete(&key)?;
        let run = store.stage_run(stage)?.run;
        super::finish::run_build(&store, &run)?
    };
    // Its item's build directory waited for the stop too.
    Ok(FollowUp {
        release_builds: build.into_iter().collect(),
        ..FollowUp::default()
    })
}
