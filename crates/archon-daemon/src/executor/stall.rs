//! The stage watchdog: a `running` stage whose agent tree has gone silent
//! with no engine behind it would otherwise wait for the stage timeout
//! (issue #251: agents left `running` with no engine after a database
//! lock). The sweep judges each one; a stalled stage settles as an
//! infrastructure failure, so `limits.infra_retries` re-spawns it once on the
//! same worktree and a second stall is final.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::LimitsConfig;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::ports::TreePulse;
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_queue::Store;

use crate::context::DaemonContext;

/// The key of a stage the watchdog is stopping, holding the reason.
const INTENT_KEY_PREFIX: &str = "stall_intent:";

/// How often one stage's tree may be read from Aether: the watchdog must not
/// add much load to an Aether whose store is already contended.
const PULSE_EVERY: chrono::Duration = chrono::Duration::minutes(1);

/// When archond last heard an Aether event of each agent, kept in memory:
/// a restart starts the watch over rather than guessing. Entries older than
/// any silence the watchdog waits for are pruned.
#[derive(Debug, Default)]
pub struct Activity {
    agents: std::sync::Mutex<BTreeMap<AetherAgentId, Timestamp>>,
    /// When each lead's tree was last read from Aether.
    looked: std::sync::Mutex<BTreeMap<AetherAgentId, Timestamp>>,
}

impl Activity {
    /// Record an event of `agent` seen at `now`; its end counts too, as its
    /// parent may be about to act on it. Any event makes the last read of
    /// its tree stale.
    pub fn saw(&self, agent: &AetherAgentId, now: Timestamp) {
        self.looked.lock().expect("activity lock").remove(agent);
        let mut agents = self.agents.lock().expect("activity lock");
        let at = agents.entry(agent.clone()).or_insert(now);
        *at = (*at).max(now);
    }

    /// Forget what was heard or read before `cutoff`: an entry that old
    /// cannot keep any tree from stalling.
    fn prune(&self, cutoff: Timestamp) {
        let mut agents = self.agents.lock().expect("activity lock");
        agents.retain(|_, at| *at >= cutoff);
        drop(agents);
        let mut looked = self.looked.lock().expect("activity lock");
        looked.retain(|_, at| *at >= cutoff);
    }

    /// The newest event of `lead` and the agents `beneath` it. The first
    /// look at a lead starts its silence at `now`; an agent beneath it that
    /// was never heard of adds nothing.
    fn newest(&self, lead: &AetherAgentId, beneath: &[AetherAgentId], now: Timestamp) -> Timestamp {
        assert!(!beneath.contains(lead), "a lead is not beneath itself");
        let mut book = self.agents.lock().expect("activity lock");
        let first = *book.entry(lead.clone()).or_insert(now);
        beneath
            .iter()
            .filter_map(|agent| book.get(agent).copied())
            .fold(first, Timestamp::max)
    }

    /// Whether `lead`'s tree may be read from Aether at `now`, recording the
    /// read when it may.
    pub(super) fn may_read(&self, lead: &AetherAgentId, now: Timestamp) -> bool {
        let mut looked = self.looked.lock().expect("activity lock");
        if looked.get(lead).is_some_and(|at| now < *at + PULSE_EVERY) {
            return false;
        }
        looked.insert(lead.clone(), now);
        true
    }
}

/// How long a stage may be silent, from `limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// With the engine reported dead.
    pub silence: Duration,
    /// With no report of the engine at all.
    pub fallback: Duration,
}

impl Policy {
    /// `None` when the watchdog is off.
    pub fn of(limits: &LimitsConfig) -> Option<Self> {
        if limits.stall_minutes == 0 {
            return None;
        }
        let minutes = |m: u32| Duration::from_secs(u64::from(m) * 60);
        let silence = minutes(limits.stall_minutes);
        // A fallback shorter than the reported-dead wait would fire sooner
        // on less evidence.
        let fallback = minutes(limits.stall_fallback_minutes).max(silence);
        Some(Self { silence, fallback })
    }
}

/// Everything the verdict on one stage's agent tree rests on.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub now: Timestamp,
    pub last_event: Timestamp,
    /// Whether any engine of the tree is alive; `None` when Aether does not
    /// report it.
    pub engine_alive: Option<bool>,
    /// Aether holds an agent of the tree `running`: it owes events. A tree
    /// with none waits on something that wakes it, or is done.
    pub running: bool,
    /// A gate or a question of the stage's run waits for the operator, or
    /// an agent of the tree waits for an approval or is `blocked`.
    pub waiting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    Stalled {
        silent: Duration,
        engine_known: bool,
    },
}

/// Whether the tree is stalled: an agent is `running` yet nothing has been
/// heard of it for the policy's silence. A live engine (a turn in flight),
/// a waiting gate, question or approval, and a tree with no `running` agent
/// all clear it. Unreported engines are judged by the longer fallback.
pub fn judge(policy: &Policy, reading: &Reading) -> Verdict {
    assert!(
        reading.last_event <= reading.now + chrono::Duration::seconds(5),
        "an event is not from the future"
    );
    assert!(
        policy.silence <= policy.fallback,
        "the fallback never fires sooner on less evidence"
    );
    if reading.waiting || !reading.running || reading.engine_alive == Some(true) {
        return Verdict::Healthy;
    }
    let silent = (reading.now - reading.last_event)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let engine_known = reading.engine_alive.is_some();
    let limit = match engine_known {
        true => policy.silence,
        false => policy.fallback,
    };
    match silent >= limit {
        true => Verdict::Stalled {
            silent,
            engine_known,
        },
        false => Verdict::Healthy,
    }
}

/// Opens every failure a stalled stage settles with.
const STALLED: &str = "agent stalled";

/// The failure a stalled stage settles with.
pub fn reason(silent: Duration, engine_known: bool) -> String {
    let minutes = silent.as_secs() / 60;
    match engine_known {
        true => format!("{STALLED}: no engine and no events for {minutes} min"),
        false => format!("{STALLED}: no events for {minutes} min (engine state not reported)"),
    }
}

/// Whether `error` is a stall the watchdog stopped the agent for.
pub(super) fn is_stall(error: &str) -> bool {
    error.starts_with(STALLED)
}

/// The running stages of `stages` that are stalled at `now`, with the
/// failure each settles with.
pub(super) async fn scan(
    ctx: &DaemonContext,
    stages: &[StageRun],
    now: Timestamp,
) -> Result<Vec<(StageRunId, String)>, ArchonError> {
    forget_settled_intents(&mut *ctx.store.lock().await)?;
    let Some(policy) = Policy::of(&ctx.config.limits) else {
        // Off: the fleet still records events, so nothing may be kept.
        ctx.activity.prune(now);
        return Ok(Vec::new());
    };
    let kept = chrono::Duration::from_std(policy.fallback * 2).expect("minutes fit a duration");
    ctx.activity.prune(now - kept);
    let waiting = Waiting::read(ctx).await?;
    let mut stalled = Vec::new();
    for stage in stages {
        assert_eq!(stage.state, StageState::Running, "only running stages scan");
        let Some(lead) = &stage.aether_agent else {
            continue;
        };
        let waits = waiting.holds(&stage.run, ctx, stage).await?;
        match judge_stage(ctx, &policy, lead, waits, now).await? {
            Verdict::Stalled {
                silent,
                engine_known,
            } => stalled.push((stage.id.clone(), reason(silent, engine_known))),
            Verdict::Healthy => {}
        }
    }
    Ok(stalled)
}

/// Judge by the lead alone first, which costs no call; only a lead that
/// looks stalled has its tree and Aether's pulse read.
async fn judge_stage(
    ctx: &DaemonContext,
    policy: &Policy,
    lead: &AetherAgentId,
    waiting: bool,
    now: Timestamp,
) -> Result<Verdict, ArchonError> {
    let last_event = ctx.activity.newest(lead, &[], now);
    let quiet = policy.silence.min(policy.fallback);
    if waiting || (now - last_event).to_std().unwrap_or(Duration::ZERO) < quiet {
        return Ok(Verdict::Healthy);
    }
    if !ctx.activity.may_read(lead, now) {
        return Ok(Verdict::Healthy);
    }
    let pulse = match ctx.backend.pulse(lead).await {
        Ok(Some(pulse)) => pulse,
        Ok(None) => return Ok(Verdict::Healthy),
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%lead, %why, "pulse unanswered; the next sweep looks again");
            return Ok(Verdict::Healthy);
        }
        Err(error) => return Err(error),
    };
    let beneath: Vec<AetherAgentId> = ctx
        .backend
        .descendants(lead)
        .await?
        .into_iter()
        .map(|d| d.agent)
        .collect();
    let seen = ctx.activity.newest(lead, &beneath, now);
    Ok(judge(policy, &read_tree(now, seen, waiting, pulse)))
}

/// The reading of a tree from archond's newest event of it, `seen`, and
/// what Aether reports.
fn read_tree(now: Timestamp, seen: Timestamp, waiting: bool, pulse: TreePulse) -> Reading {
    let last_event = pulse.last_event_at.map_or(seen, |at| at.max(seen));
    Reading {
        now,
        last_event: last_event.min(now),
        engine_alive: pulse.engine_alive,
        running: pulse.running,
        waiting: waiting || pulse.held,
    }
}

fn intent_key(stage: &StageRunId) -> String {
    format!("{INTENT_KEY_PREFIX}{stage}")
}

/// Record, before its cancel goes out, that the watchdog stops `stage` for
/// `reason`: the cancel's own events must not settle it as cancelled.
pub(super) fn mark_intent(
    store: &mut Store,
    stage: &StageRunId,
    reason: &str,
) -> Result<(), ArchonError> {
    assert!(is_stall(reason), "only the watchdog marks: {reason}");
    store.kv_set(&intent_key(stage), reason)
}

/// The outcome `stage` settles with: a cancel the watchdog caused is the
/// stall's infrastructure failure; any other outcome stands.
pub(super) fn intended(
    store: &Store,
    stage: &StageRunId,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    if !matches!(outcome, StageOutcome::Cancelled { .. }) {
        return Ok(outcome);
    }
    let Some(reason) = store.kv_get(&intent_key(stage))? else {
        return Ok(outcome);
    };
    assert!(!reason.is_empty(), "a marked stall names its reason");
    Ok(StageOutcome::Failed {
        error: reason,
        infra: true,
    })
}

/// Delete the marks of stages that settled or no longer exist.
fn forget_settled_intents(store: &mut Store) -> Result<(), ArchonError> {
    for (key, _) in store.kv_prefix(INTENT_KEY_PREFIX)? {
        let raw = key
            .strip_prefix(INTENT_KEY_PREFIX)
            .expect("ranged by prefix");
        let id = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("stall intent key {key}: {e}")))?;
        let settled = match store.stage_run(&id) {
            Ok(stage) => stage.state.is_terminal(),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if settled {
            store.kv_delete(&key)?;
        }
    }
    Ok(())
}

/// The runs and items an operator gate or question holds open.
struct Waiting {
    runs: BTreeSet<PipelineRunId>,
    items: BTreeSet<QueueItemId>,
}

impl Waiting {
    async fn read(ctx: &DaemonContext) -> Result<Self, ArchonError> {
        let store = ctx.store.lock().await;
        Ok(Self {
            runs: store.open_gates()?.into_iter().map(|g| g.run).collect(),
            items: store
                .questions(true)?
                .into_iter()
                .filter_map(|q| q.item)
                .collect(),
        })
    }

    async fn holds(
        &self,
        run: &PipelineRunId,
        ctx: &DaemonContext,
        stage: &StageRun,
    ) -> Result<bool, ArchonError> {
        assert_eq!(&stage.run, run, "the stage belongs to the run asked about");
        if self.runs.contains(run) {
            return Ok(true);
        }
        if self.items.is_empty() {
            return Ok(false);
        }
        let item = ctx.store.lock().await.run(run)?.item;
        Ok(self.items.contains(&item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        "2026-10-01T00:00:00Z".parse::<Timestamp>().unwrap() + chrono::Duration::minutes(minutes)
    }

    fn policy() -> Policy {
        Policy::of(&LimitsConfig::default()).expect("on by default")
    }

    /// A `running` tree last heard of at minute 0, read at `silent_min`.
    fn reading(silent_min: i64, engine_alive: Option<bool>) -> Reading {
        Reading {
            now: at(silent_min),
            last_event: at(0),
            engine_alive,
            running: true,
            waiting: false,
        }
    }

    #[test]
    fn a_dead_engine_stalls_after_the_silence_and_not_before() {
        let p = policy();
        assert_eq!(judge(&p, &reading(19, Some(false))), Verdict::Healthy);
        let stalled = judge(&p, &reading(20, Some(false)));
        let silent = Duration::from_secs(20 * 60);
        assert_eq!(
            stalled,
            Verdict::Stalled {
                silent,
                engine_known: true
            }
        );
        assert_eq!(
            reason(silent, true),
            "agent stalled: no engine and no events for 20 min"
        );
    }

    #[test]
    fn a_turn_in_flight_never_stalls() {
        assert_eq!(
            judge(&policy(), &reading(600, Some(true))),
            Verdict::Healthy,
            "a live engine is a turn in flight"
        );
    }

    #[test]
    fn a_waiting_gate_question_or_approval_never_stalls() {
        let waiting = Reading {
            waiting: true,
            ..reading(600, Some(false))
        };
        assert_eq!(judge(&policy(), &waiting), Verdict::Healthy);
        assert_eq!(
            judge(
                &policy(),
                &Reading {
                    engine_alive: None,
                    ..waiting
                }
            ),
            Verdict::Healthy
        );
    }

    #[test]
    fn a_tree_with_no_running_agent_waits_on_its_wake() {
        let idle = Reading {
            running: false,
            ..reading(600, Some(false))
        };
        assert_eq!(judge(&policy(), &idle), Verdict::Healthy);
        assert_eq!(
            judge(
                &policy(),
                &Reading {
                    engine_alive: None,
                    ..idle
                }
            ),
            Verdict::Healthy
        );
    }

    #[test]
    fn without_the_engine_report_silence_alone_stalls_after_the_fallback() {
        let p = policy();
        assert_eq!(judge(&p, &reading(44, None)), Verdict::Healthy);
        let stalled = judge(&p, &reading(45, None));
        assert_eq!(
            stalled,
            Verdict::Stalled {
                silent: Duration::from_secs(45 * 60),
                engine_known: false
            }
        );
        assert!(
            reason(Duration::from_secs(45 * 60), false)
                .starts_with("agent stalled: no events for 45 min")
        );
    }

    #[test]
    fn aethers_newest_event_counts_beside_archonds() {
        let pulse = TreePulse {
            engine_alive: Some(false),
            last_event_at: Some(at(15)),
            held: false,
            running: true,
        };
        let r = read_tree(at(30), at(0), false, pulse);
        assert_eq!(r.last_event, at(15));
        assert_eq!(judge(&policy(), &r), Verdict::Healthy);
        let held = read_tree(
            at(30),
            at(0),
            false,
            TreePulse {
                held: true,
                ..pulse
            },
        );
        assert!(held.waiting);
        let future = read_tree(
            at(30),
            at(0),
            false,
            TreePulse {
                last_event_at: Some(at(31)),
                ..pulse
            },
        );
        assert_eq!(
            future.last_event,
            at(30),
            "a skewed clock is clamped to now"
        );
    }

    #[test]
    fn zero_minutes_turns_the_watchdog_off_and_the_fallback_never_undercuts_the_silence() {
        let mut limits = LimitsConfig {
            stall_minutes: 30,
            stall_fallback_minutes: 10,
            ..LimitsConfig::default()
        };
        let p = Policy::of(&limits).unwrap();
        assert_eq!(p.fallback, p.silence);
        limits.stall_minutes = 0;
        assert_eq!(Policy::of(&limits), None);
    }

    #[test]
    fn activity_tracks_the_newest_event_of_a_tree() {
        let book = Activity::default();
        let lead = AetherAgentId::parse("agt_01M5PSP3TDHS01R20WX39T6P5W").unwrap();
        let child = AetherAgentId::parse("agt_01M5PSP3TDHS01R20WX39T6P5X").unwrap();
        book.saw(&lead, at(0));
        book.saw(&child, at(3));
        book.saw(&lead, at(1));
        assert_eq!(
            book.newest(&lead, std::slice::from_ref(&child), at(10)),
            at(3)
        );
        assert_eq!(book.newest(&lead, &[], at(10)), at(1), "never older");
        book.saw(&child, at(4));
        assert_eq!(
            book.newest(&lead, std::slice::from_ref(&child), at(10)),
            at(4),
            "a child's end is the tree's newest event"
        );
        let stranger = AetherAgentId::parse("agt_01M5PSP3TDHS01R20WX39T6P5Y").unwrap();
        assert_eq!(
            book.newest(&stranger, &[], at(10)),
            at(10),
            "a lead never heard of starts its silence at the first look"
        );
        book.prune(at(2));
        assert_eq!(
            book.newest(&lead, std::slice::from_ref(&child), at(11)),
            at(11),
            "a pruned lead is looked at anew; its pruned events are gone"
        );
        book.prune(at(5));
        assert_eq!(
            book.newest(&child, &[], at(12)),
            at(12),
            "the child's end was pruned"
        );
    }

    #[test]
    fn a_trees_pulse_is_read_at_most_once_a_minute_until_it_is_heard_from() {
        let book = Activity::default();
        let lead = AetherAgentId::parse("agt_01M5PSP3TDHS01R20WX39T6P5W").unwrap();
        assert!(book.may_read(&lead, at(30)));
        assert!(!book.may_read(&lead, at(30)), "read moments ago");
        assert!(book.may_read(&lead, at(31)), "a minute later");
        book.saw(&lead, at(31));
        assert!(
            book.may_read(&lead, at(31)),
            "an event makes the read stale"
        );
    }
}
