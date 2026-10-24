//! The stage watchdog: a `running` stage whose agent tree has gone silent
//! with no engine behind it would otherwise wait for the stage timeout
//! (issue #251: agents left `running` with no engine after a database
//! lock). The sweep judges each one; a stalled stage settles as an
//! infrastructure failure, so `limits.infra_retries` re-spawns it once on the
//! same worktree and a second stall is final.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use archon_aether::events::EventKind;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::LimitsConfig;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::ports::TreePulse;
use archon_core::run::{StageRun, StageState};

use crate::context::DaemonContext;

/// When archond last heard an Aether event of each agent, kept in memory:
/// a restart starts the watch over rather than guessing.
#[derive(Debug, Default)]
pub struct Activity {
    agents: std::sync::Mutex<BTreeMap<AetherAgentId, Timestamp>>,
}

impl Activity {
    /// Record an event of `agent` seen at `now`. An ended agent is forgotten.
    pub fn saw(&self, agent: &AetherAgentId, kind: &EventKind, now: Timestamp) {
        let mut agents = self.agents.lock().expect("activity lock");
        if matches!(
            kind,
            EventKind::AgentCompleted
                | EventKind::AgentFailed { .. }
                | EventKind::AgentCancelled { .. }
        ) {
            agents.remove(agent);
            return;
        }
        let at = agents.entry(agent.clone()).or_insert(now);
        *at = (*at).max(now);
    }

    /// The newest event over `agents`. The first look at an agent starts
    /// its silence at `now`.
    fn newest(&self, agents: &[AetherAgentId], now: Timestamp) -> Timestamp {
        assert!(!agents.is_empty(), "a tree has a lead");
        let mut book = self.agents.lock().expect("activity lock");
        let newest = agents
            .iter()
            .map(|agent| *book.entry(agent.clone()).or_insert(now))
            .max();
        newest.expect("a non-empty tree has an event")
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

/// The failure a stalled stage settles with.
pub fn reason(silent: Duration, engine_known: bool) -> String {
    let minutes = silent.as_secs() / 60;
    match engine_known {
        true => format!("agent stalled: no engine and no events for {minutes} min"),
        false => format!("agent stalled: no events for {minutes} min (engine state not reported)"),
    }
}

/// The running stages of `stages` that are stalled at `now`, with the
/// failure each settles with.
pub(super) async fn scan(
    ctx: &DaemonContext,
    stages: &[StageRun],
    now: Timestamp,
) -> Result<Vec<(StageRunId, String)>, ArchonError> {
    let Some(policy) = Policy::of(&ctx.config.limits) else {
        return Ok(Vec::new());
    };
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
    let last_event = ctx.activity.newest(std::slice::from_ref(lead), now);
    let quiet = policy.silence.min(policy.fallback);
    if waiting || (now - last_event).to_std().unwrap_or(Duration::ZERO) < quiet {
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
    let mut tree = vec![lead.clone()];
    tree.extend(
        ctx.backend
            .descendants(lead)
            .await?
            .into_iter()
            .map(|d| d.agent),
    );
    let seen = ctx.activity.newest(&tree, now);
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
        book.saw(&lead, &EventKind::AgentStarted { turn: 1 }, at(0));
        book.saw(&child, &EventKind::AgentWaiting { idle: false }, at(3));
        book.saw(&lead, &EventKind::AgentStarted { turn: 1 }, at(1));
        assert_eq!(book.newest(&[lead.clone(), child.clone()], at(10)), at(3));
        assert_eq!(
            book.newest(std::slice::from_ref(&lead), at(10)),
            at(1),
            "never older"
        );
        book.saw(&child, &EventKind::AgentCompleted, at(4));
        assert_eq!(
            book.newest(std::slice::from_ref(&child), at(10)),
            at(10),
            "an ended agent is forgotten, then looked at anew"
        );
    }
}
