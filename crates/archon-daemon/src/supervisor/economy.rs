//! The orchestrator's context economy: an idle, large context is compacted
//! before its prompt cache expires, and only while no turn runs or starts.

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_aether::wire::Agent;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::{MAX_COMPACTION_TOKENS, OrchestratorConfig};
use archon_core::error::ArchonError;
use archon_queue::Store;
use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;

/// How often the orchestrator's economy is checked.
pub const ECONOMY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Aether's state for an agent that waits for its next message.
const IDLE_STATE: &str = "waiting";
/// `kv`: the [`IdleCompaction`] last asked for.
const IDLE_COMPACTION_KEY: &str = "orchestrator_idle_compaction";
/// Aether's wait condition for an agent that waits for any inbox item.
const INBOX_WAIT: &str = "inbox";
/// How long the orchestrator must have logged nothing before the rule acts,
/// so a message Aether just accepted is never cut off.
const QUIET_BEFORE_ACTING: TimeDelta = TimeDelta::minutes(2);
/// The prompt cache lasts an hour; a fold later than this would read it cold.
const CACHE_STILL_WARM: TimeDelta = TimeDelta::minutes(57);

/// What one check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Economy {
    Nothing,
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

/// Compact the orchestrator as of `now` when it is idle with a large
/// context; a turn that runs or is about to start leaves it alone.
pub async fn economize(ctx: &DaemonContext, now: Timestamp) -> Result<Economy, ArchonError> {
    let clock = Clock::starting(now);
    let _serial = ctx.supervisor.serial.lock().await;
    let Some(id) = super::current_orchestrator(ctx).await? else {
        return Ok(Economy::Nothing);
    };
    let client = ctx.aether.client()?;
    let agent = client.get_agent(&id).await?;
    if !is_idle(&agent, clock.now()) {
        return Ok(Economy::Nothing);
    }
    match compact_if_idle(ctx, &client, &agent, clock).await? {
        true => Ok(Economy::Compacted),
        false => Ok(Economy::Nothing),
    }
}

/// The caller's `now`, carried across the waits for locks and for Aether, so
/// a check made after them is made at the time it is by then.
#[derive(Clone, Copy)]
struct Clock {
    at: Timestamp,
    since: tokio::time::Instant,
}

impl Clock {
    fn starting(at: Timestamp) -> Self {
        let since = tokio::time::Instant::now();
        Self { at, since }
    }

    fn now(&self) -> Timestamp {
        let waited = TimeDelta::from_std(self.since.elapsed()).expect("a wait of this process");
        self.at + waited
    }
}

/// Whether `agent` waits only for its next message, unpaused, and has logged
/// nothing for `QUIET_BEFORE_ACTING` (two minutes): no turn runs and none is starting.
/// A wait on a task, an approval or a child is outstanding work, not idleness.
pub fn is_idle(agent: &Agent, now: Timestamp) -> bool {
    let waits_for_mail =
        agent.status["state"] == IDLE_STATE && agent.status["condition"]["on"] == INBOX_WAIT;
    let quiet = agent
        .last_event_at
        .is_some_and(|at| now - at >= QUIET_BEFORE_ACTING);
    waits_for_mail && !agent.paused && quiet
}

/// Whether held news is due at the time it is now that the caller holds `store`,
/// so the wait for it counts and the caller's write follows with none between.
fn news_due(store: &Store, clock: Clock) -> Result<bool, ArchonError> {
    crate::triggers::delivering(store, clock.now())
}

/// Read again under the switch guard, where no Archon trigger can be on its
/// way: the orchestrator, not restarted into a successor, is still idle.
/// Returns the agent as so read; due news is checked where the act is recorded.
async fn still_idle(
    ctx: &DaemonContext,
    client: &AetherClient,
    clock: Clock,
) -> Result<Option<Agent>, ArchonError> {
    let Some(id) = super::current_orchestrator(ctx).await? else {
        return Ok(None);
    };
    let agent = super::newest_incarnation(client, &id).await?.newest;
    let idle = agent.id == id && is_idle(&agent, clock.now());
    Ok(idle.then_some(agent))
}

/// Whether an orchestrator in `agent`'s state is due an idle compaction at
/// `now`: idle for `after` since its last turn ended and still inside the
/// prompt cache's lifetime, with more than `floor` tokens of context, and
/// not yet compacted for this idle stretch (`done`).
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
    let idle = now - ended;
    let in_window = after > TimeDelta::zero() && idle >= after && idle < CACHE_STILL_WARM;
    in_window && tokens > floor && done != Some(ended)
}

/// The context size above which the idle `agent` is compacted: half the
/// threshold Aether applied to it, else half the configured one. A threshold
/// past every model's window is capped as Aether caps it.
fn compaction_floor(config: &OrchestratorConfig, agent: &Agent) -> u64 {
    let applied = agent.config.compaction_tokens.filter(|tokens| *tokens > 0);
    let applied = applied.map(|tokens| tokens.min(MAX_COMPACTION_TOKENS));
    let floor = applied.map_or_else(|| config.idle_compact_floor(), |t| u64::from(t / 2));
    assert!(floor > 0, "a threshold is positive");
    floor
}

/// Ask Aether to compact the idle orchestrator once per idle stretch, after
/// reading it again under the switch guard, where no Archon trigger or
/// operator message can start a turn. A refused request is not retried.
async fn compact_if_idle(
    ctx: &DaemonContext,
    client: &AetherClient,
    agent: &Agent,
    clock: Clock,
) -> Result<bool, ArchonError> {
    let config = &ctx.config.orchestrator;
    if !compaction_due(&*ctx.store.lock().await, config, agent, clock.now())? {
        return Ok(false);
    }
    let _switch = super::switching(ctx).await;
    let Some(checked) = still_idle(ctx, client, clock).await? else {
        return Ok(false);
    };
    if checked.id != agent.id || !claim_compaction(ctx, &checked, clock).await? {
        return Ok(false);
    }
    compact(client, &checked).await
}

/// Whether `agent`, as read at `now`, is owed its idle compaction.
fn compaction_due(
    store: &Store,
    config: &OrchestratorConfig,
    agent: &Agent,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let after = TimeDelta::minutes(i64::from(config.idle_compact_after_minutes));
    let floor = compaction_floor(config, agent);
    let done = parse_kv::<IdleCompaction>(store, IDLE_COMPACTION_KEY)?
        .filter(|done| done.agent == agent.id)
        .map(|done| done.turn_ended_at);
    Ok(idle_compaction_due(agent, now, after, floor, done))
}

/// Under one hold of the store, at the time it is once held: no news is due,
/// `agent` is still owed its compaction, and this idle stretch is marked done.
async fn claim_compaction(
    ctx: &DaemonContext,
    agent: &Agent,
    clock: Clock,
) -> Result<bool, ArchonError> {
    let mut store = ctx.store.lock().await;
    let owed = compaction_due(&store, &ctx.config.orchestrator, agent, clock.now())?;
    if !owed || news_due(&store, clock)? {
        return Ok(false);
    }
    let turn_ended_at = agent.stats.last_turn_at.expect("checked by the rule");
    let mark = IdleCompaction {
        agent: agent.id.clone(),
        turn_ended_at,
    };
    store.kv_set(IDLE_COMPACTION_KEY, &serde_json::to_string(&mark)?)?;
    Ok(true)
}

/// Ask Aether to compact `agent`, whose idle stretch is marked done.
async fn compact(client: &AetherClient, agent: &Agent) -> Result<bool, ArchonError> {
    let tokens = agent.stats.context_tokens.unwrap_or_default();
    match client.compact(&agent.id).await {
        Ok(()) => {
            tracing::info!(agent = %agent.id, tokens, "asked Aether to compact the idle orchestrator");
            Ok(true)
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(agent = %agent.id, tokens, %error, "Aether refused to compact the idle orchestrator; its context is kept");
            Ok(false)
        }
    }
}

fn parse_kv<T: serde::de::DeserializeOwned>(
    store: &Store,
    key: &str,
) -> Result<Option<T>, ArchonError> {
    let Some(raw) = store.kv_get(key)? else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))
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
        raw["status"] = serde_json::json!({ "state": IDLE_STATE, "condition": { "on": "inbox" } });
        raw["last_event_at"] = serde_json::json!(turn_ended_at);
        raw["stats"] = serde_json::json!({
            "context_tokens": tokens,
            "last_turn_at": turn_ended_at,
        });
        serde_json::from_value(raw).unwrap()
    }

    /// A fake clock walks one idle stretch: the rule fires once, from 50
    /// minutes until the cache is nearly cold, only above half the threshold.
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
        assert!(
            !idle_compaction_due(&large, at(57), after, floor, None),
            "the cache is cold"
        );
        assert!(
            !idle_compaction_due(&large, at(180), after, floor, None),
            "a late reconnect"
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
            !idle_compaction_due(&small, at(52), after, floor, None),
            "at half, kept"
        );
        let unknown = orchestrator(None, Some(at(0)));
        assert!(
            !idle_compaction_due(&unknown, at(52), after, floor, None),
            "no usage yet"
        );
        let off = TimeDelta::zero();
        assert!(
            !idle_compaction_due(&large, at(52), off, floor, None),
            "0 turns it off"
        );
    }

    #[test]
    fn the_floor_is_half_the_threshold_aether_applied_else_the_configured_one() {
        let config = OrchestratorConfig::default();
        let mut agent = orchestrator(Some(1), Some(at(0)));
        assert_eq!(
            compaction_floor(&config, &agent),
            80_000,
            "no override reported"
        );
        agent.config.compaction_tokens = Some(180_000);
        assert_eq!(
            compaction_floor(&config, &agent),
            90_000,
            "the running agent's own"
        );
        agent.config.compaction_tokens = Some(2_000_000);
        assert_eq!(
            compaction_floor(&config, &agent),
            100_000,
            "capped as Aether caps it"
        );
    }

    #[test]
    fn only_an_unpaused_quiet_inbox_wait_is_idle() {
        let idle = orchestrator(Some(1), Some(at(0)));
        assert!(is_idle(&idle, at(2)), "waits for mail, quiet two minutes");
        assert!(
            !is_idle(&idle, at(1)),
            "an event a minute ago: a wake may be starting"
        );
        let mut paused = idle.clone();
        paused.paused = true;
        assert!(!is_idle(&paused, at(10)), "an operator pause is kept");
        let mut on_task = idle.clone();
        on_task.status = serde_json::json!({
            "state": IDLE_STATE, "condition": { "on": "task", "id": "tsk_01" }
        });
        assert!(!is_idle(&on_task, at(10)), "it waits on outstanding work");
        let mut running = idle.clone();
        running.status = serde_json::json!({ "state": "running" });
        assert!(!is_idle(&running, at(10)));
        let mut unknown = idle;
        unknown.last_event_at = None;
        assert!(
            !is_idle(&unknown, at(10)),
            "no activity record: leave it be"
        );
    }
}
