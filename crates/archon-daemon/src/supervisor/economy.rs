//! The orchestrator's context economy: a fresh generation once an upgrade
//! changed its prompt or tools, and an idle, large context compacted before
//! its prompt cache expires. Both act only while no turn runs.

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_aether::wire::Agent;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::protocol::OrchestratorFreshness;
use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;

/// How often the orchestrator's economy is checked.
pub const ECONOMY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Aether's state for an agent that waits for its next message.
const IDLE_STATE: &str = "waiting";
/// `kv`: the [`IdleCompaction`] last asked for.
const IDLE_COMPACTION_KEY: &str = "orchestrator_idle_compaction";
/// `kv`: the generation a freshness refresh was last tried for.
const REFRESH_TRIED_KEY: &str = "orchestrator_refresh_tried";

/// What one check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Economy {
    Nothing,
    /// A fresh generation replaced one whose prompt or tools were outdated.
    Refreshed,
    /// Aether was asked to compact the idle orchestrator.
    Compacted,
}

/// One idle stretch an idle compaction was asked for: the agent and the end
/// of the turn it has been idle since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IdleCompaction {
    agent: AetherAgentId,
    turn_ended_at: Timestamp,
}

/// Checks every [`ECONOMY_CHECK_INTERVAL`]. Only a store failure ends it.
pub async fn watch_economy(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::time::sleep(ECONOMY_CHECK_INTERVAL).await;
        if let Err(error) = economize(&ctx, archon_core::now()).await {
            if error.is_store() {
                return Err(error);
            }
            tracing::warn!(%error, "checking the orchestrator's context economy failed");
        }
    }
}

/// Apply both rules to the orchestrator as of `now`: refresh an outdated
/// generation, else compact an idle, large context.
pub async fn economize(ctx: &DaemonContext, now: Timestamp) -> Result<Economy, ArchonError> {
    let _serial = ctx.supervisor.serial.lock().await;
    let Some(id) = super::current_orchestrator(ctx).await? else {
        return Ok(Economy::Nothing);
    };
    let client = ctx.aether.client()?;
    let agent = client.get_agent(&id).await?;
    assert_eq!(agent.id, id, "Aether answers for the agent asked");
    if super::state_of(&agent) != IDLE_STATE {
        return Ok(Economy::Nothing);
    }
    if refresh_if_outdated(ctx, &client).await? {
        return Ok(Economy::Refreshed);
    }
    match compact_if_idle(ctx, &client, &agent, now).await? {
        true => Ok(Economy::Compacted),
        false => Ok(Economy::Nothing),
    }
}

/// Whether a generation `freshness` describes, not yet tried, is owed a
/// fresh one: its prompt or tools differ from what this archond starts.
pub fn refresh_due(
    freshness: Option<&OrchestratorFreshness>,
    generation: u64,
    tried: Option<u64>,
) -> bool {
    assert!(generation > 0, "generations start at 1");
    assert!(
        tried.is_none_or(|tried| tried <= generation),
        "a refresh is tried only for a generation that existed"
    );
    let Some(freshness) = freshness else {
        return false;
    };
    let outdated = freshness.prompt_outdated == Some(true) || freshness.tools_outdated;
    outdated && tried != Some(generation)
}

/// Start a fresh generation when the running one's prompt or tools are
/// outdated. Tried once per generation: a failed refresh leaves the old one.
async fn refresh_if_outdated(
    ctx: &DaemonContext,
    client: &AetherClient,
) -> Result<bool, ArchonError> {
    let generation = super::generation(ctx).await?;
    let freshness = super::freshness(ctx, generation).await?;
    let tried = read_kv::<u64>(ctx, REFRESH_TRIED_KEY).await?;
    if !refresh_due(freshness.as_ref(), generation, tried) {
        return Ok(false);
    }
    write_kv(ctx, REFRESH_TRIED_KEY, &generation).await?;
    let reason = "refreshed: its prompt or tools are outdated";
    let why = "is a fresh generation, as its prompt or tools changed in an upgrade";
    let id = super::replace_orchestrator(ctx, client, reason, why).await?;
    tracing::info!(%id, old_generation = generation, "orchestrator refreshed after an upgrade");
    Ok(true)
}

/// Whether an orchestrator in `agent`'s state is due an idle compaction at
/// `now`: idle for `after` since its last turn ended, with more than `floor`
/// tokens of context, and not yet compacted for this idle stretch (`done`).
pub fn idle_compaction_due(
    agent: &Agent,
    now: Timestamp,
    after: TimeDelta,
    floor: u64,
    done: Option<Timestamp>,
) -> bool {
    assert!(after >= TimeDelta::zero(), "an idle time is never negative");
    assert!(floor > 0, "the floor is half a positive threshold");
    let (Some(ended), Some(tokens)) = (agent.stats.last_turn_at, agent.stats.context_tokens) else {
        return false;
    };
    let switched_on = after > TimeDelta::zero();
    let idle_long = now - ended >= after;
    switched_on && idle_long && tokens > floor && done != Some(ended)
}

/// Ask Aether to compact the idle orchestrator once per idle stretch. A
/// refused request is reported and not retried for the same stretch.
async fn compact_if_idle(
    ctx: &DaemonContext,
    client: &AetherClient,
    agent: &Agent,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let config = &ctx.config.orchestrator;
    let after = TimeDelta::minutes(i64::from(config.idle_compact_after_minutes));
    let floor = config.idle_compact_floor();
    let done = read_kv::<IdleCompaction>(ctx, IDLE_COMPACTION_KEY)
        .await?
        .filter(|done| done.agent == agent.id)
        .map(|done| done.turn_ended_at);
    if !idle_compaction_due(agent, now, after, floor, done) {
        return Ok(false);
    }
    let turn_ended_at = agent.stats.last_turn_at.expect("checked by the rule");
    let mark = IdleCompaction {
        agent: agent.id.clone(),
        turn_ended_at,
    };
    write_kv(ctx, IDLE_COMPACTION_KEY, &mark).await?;
    let tokens = agent.stats.context_tokens.unwrap_or_default();
    match client.compact(&agent.id).await {
        Ok(()) => {
            tracing::info!(agent = %agent.id, tokens, "asked Aether to compact the idle orchestrator");
            Ok(true)
        }
        Err(error) => {
            tracing::warn!(agent = %agent.id, tokens, %error, "Aether refused to compact the idle orchestrator; its context is kept");
            Ok(false)
        }
    }
}

async fn read_kv<T: serde::de::DeserializeOwned>(
    ctx: &DaemonContext,
    key: &str,
) -> Result<Option<T>, ArchonError> {
    let Some(raw) = ctx.store.lock().await.kv_get(key)? else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))
}

async fn write_kv<T: Serialize>(
    ctx: &DaemonContext,
    key: &str,
    value: &T,
) -> Result<(), ArchonError> {
    let raw = serde_json::to_string(value)?;
    ctx.store.lock().await.kv_set(key, &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z").unwrap();
        start.with_timezone(&chrono::Utc) + TimeDelta::minutes(minutes)
    }

    fn orchestrator(tokens: Option<u64>, turn_ended_at: Option<Timestamp>) -> Agent {
        let mut raw = archon_aether::fake::agent("agt_0001", None);
        raw["status"] = serde_json::json!({ "state": IDLE_STATE });
        raw["stats"] = serde_json::json!({
            "context_tokens": tokens,
            "last_turn_at": turn_ended_at,
        });
        serde_json::from_value(raw).unwrap()
    }

    /// A fake clock walks one idle stretch: the rule fires once, at 50
    /// minutes, only above half the threshold.
    #[test]
    fn the_idle_rule_fires_once_per_idle_stretch_above_half_the_threshold() {
        let after = TimeDelta::minutes(50);
        let floor = 80_000;
        let large = orchestrator(Some(120_000), Some(at(0)));
        assert!(
            !idle_compaction_due(&large, at(49), after, floor, None),
            "not idle long enough"
        );
        assert!(
            idle_compaction_due(&large, at(50), after, floor, None),
            "idle 50 minutes"
        );
        let done = Some(at(0));
        assert!(
            !idle_compaction_due(&large, at(51), after, floor, done),
            "once per stretch"
        );
        let next_stretch = orchestrator(Some(120_000), Some(at(70)));
        assert!(!idle_compaction_due(
            &next_stretch,
            at(100),
            after,
            floor,
            done
        ));
        assert!(idle_compaction_due(
            &next_stretch,
            at(120),
            after,
            floor,
            done
        ));

        let small = orchestrator(Some(80_000), Some(at(0)));
        assert!(
            !idle_compaction_due(&small, at(90), after, floor, None),
            "at half, kept"
        );
        let unknown = orchestrator(None, Some(at(0)));
        assert!(
            !idle_compaction_due(&unknown, at(90), after, floor, None),
            "no usage yet"
        );
        let off = TimeDelta::zero();
        assert!(
            !idle_compaction_due(&large, at(90), off, floor, None),
            "0 turns it off"
        );
    }

    #[test]
    fn a_changed_prompt_or_tool_fingerprint_is_refreshed_once_per_generation() {
        let fresh = OrchestratorFreshness {
            created_by: "1.6.4".into(),
            prompt_outdated: Some(false),
            tools_outdated: false,
            new_lessons: 3,
            settings_drift: None,
        };
        assert!(
            !refresh_due(Some(&fresh), 4, None),
            "up to date; lessons wait"
        );
        let prompt = OrchestratorFreshness {
            prompt_outdated: Some(true),
            ..fresh.clone()
        };
        let tools = OrchestratorFreshness {
            tools_outdated: true,
            ..fresh.clone()
        };
        for outdated in [&prompt, &tools] {
            assert!(refresh_due(Some(outdated), 4, None));
            assert!(
                refresh_due(Some(outdated), 4, Some(3)),
                "tried for an older one"
            );
            assert!(!refresh_due(Some(outdated), 4, Some(4)), "tried once");
        }
        let unknown = OrchestratorFreshness {
            prompt_outdated: None,
            ..fresh
        };
        assert!(
            !refresh_due(Some(&unknown), 4, None),
            "no prompt to compare"
        );
        assert!(!refresh_due(None, 4, None), "no origin recorded");
    }
}
