//! The orchestrator's context economy: a fresh generation once an upgrade
//! changed its prompt or tools, and an idle, large context compacted before
//! its prompt cache expires. Both act only while no turn runs.

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_aether::backend::{LIVE_STATES, agent_state};
use archon_aether::wire::Agent;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::{MAX_COMPACTION_TOKENS, OrchestratorConfig};
use archon_core::error::ArchonError;
use archon_core::protocol::OrchestratorFreshness;
use archon_queue::Store;
use archon_queue::store::keys;
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
/// `kv`: the [`RefreshTried`] of the latest refresh attempt.
const REFRESH_TRIED_KEY: &str = "orchestrator_refresh_tried";
/// `kv`: the generation a refresh replaces, kept until a successor is
/// adopted, however many generations a failed creation advanced meanwhile.
pub(super) const REFRESH_UNFINISHED_KEY: &str = "orchestrator_refresh_unfinished";
/// How long the orchestrator must have logged nothing before either rule
/// acts, so a message Aether just accepted is never cut off.
const QUIET_BEFORE_ACTING: TimeDelta = TimeDelta::minutes(2);
/// Failed attempts at refreshing one generation before the operator is told;
/// retries go on, so the outdated prompt never outlives a recovered Aether.
const REFRESH_TRIES: u32 = 3;
const REFRESH_RETRY: TimeDelta = TimeDelta::minutes(10);
/// The prompt cache lasts an hour; a fold later than this would read it cold.
const CACHE_STILL_WARM: TimeDelta = TimeDelta::minutes(57);

/// What one check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Economy {
    Nothing,
    /// A fresh generation replaced one whose prompt or tools were outdated.
    Refreshed,
    /// Aether was asked to compact the idle orchestrator.
    Compacted,
}

/// What a refresh attempt found under the switch guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// None is owed.
    NotDue,
    /// One is owed, but the orchestrator stopped being idle.
    Busy,
    Done,
}

/// The latest refresh attempt: its generation, how many tries it had, when
/// the last one began, and why it failed (`None` while it runs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshTried {
    pub generation: u64,
    pub tries: u32,
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    let generation = super::generation(ctx).await?;
    let tried = read_kv::<RefreshTried>(ctx, REFRESH_TRIED_KEY).await?;
    let unfinished = read_kv::<u64>(ctx, REFRESH_UNFINISHED_KEY).await?;
    let client = ctx.aether.client()?;
    let agent = match super::current_orchestrator(ctx).await? {
        Some(id) => Some(client.get_agent(&id).await?),
        None => None,
    };
    if refresh_left_behind(unfinished, agent.as_ref()) {
        let id = super::ensure_locked(ctx).await?;
        tracing::info!(%id, "started the orchestrator a failed refresh left out");
        return Ok(Economy::Refreshed);
    }
    let Some(agent) = agent.filter(|agent| is_idle(agent, now)) else {
        return Ok(Economy::Nothing);
    };
    if ctx.config.orchestrator.refresh_on_upgrade {
        let owed = Owed {
            generation,
            unfinished,
            tried,
        };
        match refresh_if_owed(ctx, &client, now, owed).await? {
            Refresh::Done => return Ok(Economy::Refreshed),
            Refresh::Busy => return Ok(Economy::Nothing),
            Refresh::NotDue => {}
        }
    }
    match compact_if_idle(ctx, &client, &agent, now).await? {
        true => Ok(Economy::Compacted),
        false => Ok(Economy::Nothing),
    }
}

/// Whether a refresh is under way. The marker and a successor's id change
/// in one transaction, so the agent stored beside a marker is the outdated
/// one: a stopped one is replaced, never restarted from its checkpoint.
pub(super) async fn refresh_owed_by(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    let store = ctx.store.lock().await;
    let generation = super::parse_generation(store.kv_get(keys::ORCHESTRATOR_GENERATION)?)?;
    let Some(unfinished) = parse_kv::<u64>(&store, REFRESH_UNFINISHED_KEY)? else {
        return Ok(false);
    };
    if unfinished == 0 || unfinished > generation {
        tracing::warn!(
            unfinished,
            generation,
            "the unfinished refresh names no generation that was; it is finished fresh all the same"
        );
    }
    Ok(true)
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

/// Whether a refresh that began (`unfinished`, whatever generation it
/// names: a stored value is never trusted to be sane) stopped before a
/// successor was adopted and left none live (`agent`), even when a successor
/// that stopped before its briefing advanced the generation again.
pub fn refresh_left_behind(unfinished: Option<u64>, agent: Option<&Agent>) -> bool {
    unfinished.is_some()
        && agent.is_none_or(|agent| !LIVE_STATES.contains(&agent_state(agent).as_str()))
}

/// Whether the stored generation, as `freshness` describes it, is owed a
/// fresh one: another build (`running`) created it and its prompt or tools
/// differ from this build's, or a refresh of it began and adopted no
/// successor (`unfinished`).
pub fn refresh_owed(
    freshness: Option<&OrchestratorFreshness>,
    unfinished: Option<u64>,
    running: &str,
) -> bool {
    assert!(!running.is_empty(), "this build has a version");
    let outdated = freshness.is_some_and(|freshness| {
        assert!(
            !freshness.created_by.is_empty(),
            "a recorded creator has a version"
        );
        let upgraded = freshness.created_by != running;
        upgraded && (freshness.prompt_outdated == Some(true) || freshness.tools_outdated)
    });
    outdated || unfinished.is_some()
}

/// Whether a refresh owed to generation `generation` may be tried at `now`:
/// a failed try (`tried`) waits out its pause, however many came before.
pub fn retry_due(generation: u64, tried: Option<&RefreshTried>, now: Timestamp) -> bool {
    assert!(generation > 0, "generations start at 1");
    match tried.filter(|tried| tried.generation == generation) {
        None => true,
        Some(tried) => {
            assert!(tried.tries > 0, "a recorded try was made");
            now - tried.at >= REFRESH_RETRY
        }
    }
}

/// Why the refresh generation `generation` is owed has not happened yet,
/// for `status`: `None` when none is owed or `refresh_on_upgrade` is off.
pub(super) async fn refresh_delay(
    ctx: &DaemonContext,
    generation: u64,
    freshness: &OrchestratorFreshness,
) -> Result<Option<String>, ArchonError> {
    if !ctx.config.orchestrator.refresh_on_upgrade {
        return Ok(None);
    }
    let (unfinished, tried) = {
        let store = ctx.store.lock().await;
        let unfinished = parse_kv::<u64>(&store, REFRESH_UNFINISHED_KEY)?;
        let tried = parse_kv::<RefreshTried>(&store, REFRESH_TRIED_KEY)?;
        (unfinished, tried)
    };
    if !refresh_owed(Some(freshness), unfinished, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let tried = tried.filter(|tried| tried.generation == generation);
    Ok(Some(delay_reason(tried.as_ref())))
}

/// The reason `status` gives for an owed refresh: the latest failure, else
/// the idle moment it waits for.
fn delay_reason(tried: Option<&RefreshTried>) -> String {
    let failed = tried.and_then(|tried| tried.error.as_ref().map(|error| (tried.tries, error)));
    let Some((tries, error)) = failed else {
        return "it waits until the orchestrator is idle with no news due".to_owned();
    };
    assert!(tries > 0, "a failed try was made");
    assert!(!error.trim().is_empty(), "a failure says why");
    let attempts = if tries == 1 { "attempt" } else { "attempts" };
    format!(
        "{tries} {attempts} failed, the last with: {error}; retried every {} minutes while it is idle",
        REFRESH_RETRY.num_minutes()
    )
}

/// What a check read about the refresh the stored generation may be owed.
struct Owed {
    generation: u64,
    unfinished: Option<u64>,
    tried: Option<RefreshTried>,
}

/// Start a fresh generation when an upgrade outdated the running one's
/// prompt or tools, or a refresh of it began. Fenced against trigger
/// delivery; a failure is recorded for `status` and tried again.
async fn refresh_if_owed(
    ctx: &DaemonContext,
    client: &AetherClient,
    now: Timestamp,
    owed: Owed,
) -> Result<Refresh, ArchonError> {
    let generation = owed.generation;
    let freshness = super::compared_with_today(ctx, generation).await?;
    let running = env!("CARGO_PKG_VERSION");
    let due = refresh_owed(freshness.as_ref(), owed.unfinished, running)
        && retry_due(generation, owed.tried.as_ref(), now);
    if !due {
        return Ok(Refresh::NotDue);
    }
    let _switch = super::switching(ctx).await;
    let Some(checked) = still_idle(ctx, client, now).await? else {
        return Ok(Refresh::Busy);
    };
    let attempt = begin_refresh(ctx, generation, owed.tried, now).await?;
    let reason = "refreshed: an upgrade changed its prompt or tools";
    let why = "is a fresh generation, as an upgrade changed its prompt or tools";
    let checked = Some(&checked.id);
    match super::replace_orchestrator(ctx, client, reason, why, checked).await {
        Ok(id) => {
            let tries = attempt.tries;
            tracing::info!(%id, old_generation = generation, tries, "orchestrator refreshed after an upgrade");
            Ok(Refresh::Done)
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            let failed = RefreshTried {
                error: Some(crate::executor::clip(&error.to_string(), 200)),
                ..attempt
            };
            write_kv(ctx, REFRESH_TRIED_KEY, &failed).await?;
            Err(error)
        }
    }
}

/// Record that a refresh of generation `generation` starts: the attempt and
/// the unfinished marker in one transaction, so a stop leaves both or
/// neither. The operator is told once [`REFRESH_TRIES`] attempts failed.
async fn begin_refresh(
    ctx: &DaemonContext,
    generation: u64,
    tried: Option<RefreshTried>,
    now: Timestamp,
) -> Result<RefreshTried, ArchonError> {
    assert!(generation > 0, "only a created generation is refreshed");
    let tries = tried
        .filter(|tried| tried.generation == generation)
        .map_or(1, |tried| tried.tries.saturating_add(1));
    assert!(tries > 0, "this try counts");
    let attempt = RefreshTried {
        generation,
        tries,
        at: now,
        error: None,
    };
    let recorded = serde_json::to_string(&attempt)?;
    let marker = generation.to_string();
    let set = [
        (REFRESH_TRIED_KEY, recorded.as_str()),
        (REFRESH_UNFINISHED_KEY, marker.as_str()),
    ];
    ctx.store.lock().await.kv_update(&set, &[])?;
    if tries == REFRESH_TRIES + 1 {
        let text = format!(
            "The orchestrator's refresh after the upgrade failed {REFRESH_TRIES} times; Archon keeps trying every {} minutes while it is idle, or `archon orchestrator restart` refreshes it now.",
            REFRESH_RETRY.num_minutes()
        );
        crate::feed::system(ctx, text).await?;
    }
    Ok(attempt)
}

/// Read again under the switch guard, where no Archon trigger can be on its
/// way: the orchestrator, not restarted into a successor, is still idle and
/// no news is due to wake it. Returns the agent as so read.
async fn still_idle(
    ctx: &DaemonContext,
    client: &AetherClient,
    now: Timestamp,
) -> Result<Option<Agent>, ArchonError> {
    let Some(id) = super::current_orchestrator(ctx).await? else {
        return Ok(None);
    };
    if crate::triggers::delivering(&*ctx.store.lock().await, now)? {
        return Ok(None);
    }
    let agent = super::newest_incarnation(client, &id).await?.newest;
    let idle = agent.id == id && is_idle(&agent, now);
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
    now: Timestamp,
) -> Result<bool, ArchonError> {
    if !compaction_due(ctx, agent, now).await? {
        return Ok(false);
    }
    let _switch = super::switching(ctx).await;
    let Some(checked) = still_idle(ctx, client, now).await? else {
        return Ok(false);
    };
    if checked.id != agent.id || !compaction_due(ctx, &checked, now).await? {
        return Ok(false);
    }
    compact(ctx, client, &checked).await
}

/// Whether `agent`, as read at `now`, is owed its idle compaction.
async fn compaction_due(
    ctx: &DaemonContext,
    agent: &Agent,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let config = &ctx.config.orchestrator;
    let after = TimeDelta::minutes(i64::from(config.idle_compact_after_minutes));
    let floor = compaction_floor(config, agent);
    let done = read_kv::<IdleCompaction>(ctx, IDLE_COMPACTION_KEY)
        .await?
        .filter(|done| done.agent == agent.id)
        .map(|done| done.turn_ended_at);
    Ok(idle_compaction_due(agent, now, after, floor, done))
}

/// Mark this idle stretch done, then ask Aether to compact `agent`.
async fn compact(
    ctx: &DaemonContext,
    client: &AetherClient,
    agent: &Agent,
) -> Result<bool, ArchonError> {
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
        Err(error) if error.is_store() => Err(error),
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
    parse_kv(&*ctx.store.lock().await, key)
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
    fn only_a_refresh_that_left_no_live_orchestrator_is_finished() {
        let mut cancelled = orchestrator(None, None);
        cancelled.status =
            serde_json::from_value(serde_json::json!({ "state": "cancelled" })).unwrap();
        let live = orchestrator(None, None);
        assert!(
            refresh_left_behind(Some(3), None),
            "cancelled, none created"
        );
        assert!(
            refresh_left_behind(Some(3), Some(&cancelled)),
            "cancelled, not advanced"
        );
        assert!(
            refresh_left_behind(Some(0), Some(&cancelled)),
            "a marker naming no generation is a refresh owed, never a panic"
        );
        assert!(
            !refresh_left_behind(Some(3), Some(&live)),
            "the old one still runs"
        );
        assert!(!refresh_left_behind(None, None), "no refresh is unfinished");
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

    fn created_by_an_older_build() -> OrchestratorFreshness {
        OrchestratorFreshness {
            created_by: "1.6.3".into(),
            prompt_outdated: Some(false),
            tools_outdated: false,
            new_lessons: 3,
            settings_drift: None,
            refresh_delayed: None,
        }
    }

    #[test]
    fn an_upgrade_that_outdated_the_prompt_or_tools_owes_a_refresh() {
        let running = "1.6.4";
        let fresh = created_by_an_older_build();
        assert!(
            !refresh_owed(Some(&fresh), None, running),
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
        assert!(refresh_owed(Some(&prompt), None, running));
        assert!(refresh_owed(Some(&tools), None, running));
        let edited = OrchestratorFreshness {
            created_by: running.into(),
            ..prompt
        };
        assert!(
            !refresh_owed(Some(&edited), None, running),
            "no upgrade: an edit waits"
        );
        let unknown = OrchestratorFreshness {
            prompt_outdated: None,
            ..fresh.clone()
        };
        assert!(
            !refresh_owed(Some(&unknown), None, running),
            "no prompt to compare"
        );
        assert!(!refresh_owed(None, None, running), "no origin recorded");
        for freshness in [Some(&fresh), Some(&edited), None] {
            assert!(
                refresh_owed(freshness, Some(4), running),
                "a refresh that began is owed until a successor is adopted"
            );
        }
    }

    #[test]
    fn a_failed_refresh_waits_out_its_pause_and_is_never_given_up() {
        let now = at(0);
        let tried = |generation, tries, minutes| RefreshTried {
            generation,
            tries,
            at: at(minutes),
            error: Some("busy".into()),
        };
        assert!(retry_due(4, None, now), "never tried");
        let older = tried(3, REFRESH_TRIES, 0);
        assert!(
            retry_due(4, Some(&older), now),
            "another generation's tries"
        );
        let failed = tried(4, 1, -5);
        assert!(!retry_due(4, Some(&failed), now), "backs off");
        assert!(retry_due(4, Some(&failed), at(5)), "retried");
        let spent = tried(4, REFRESH_TRIES + 20, -10);
        assert!(retry_due(4, Some(&spent), now), "never gives up for good");
    }

    #[test]
    fn status_is_told_why_an_owed_refresh_has_not_happened() {
        let waiting = delay_reason(None);
        assert!(waiting.contains("waits until the orchestrator is idle"));
        let running = RefreshTried {
            generation: 4,
            tries: 1,
            at: at(0),
            error: None,
        };
        assert_eq!(delay_reason(Some(&running)), waiting, "no failure yet");
        let failed = RefreshTried {
            tries: 5,
            error: Some("unavailable: busy".into()),
            ..running
        };
        assert_eq!(
            delay_reason(Some(&failed)),
            "5 attempts failed, the last with: unavailable: busy; retried every 10 minutes \
             while it is idle"
        );
    }
}
