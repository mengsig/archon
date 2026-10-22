//! Telling the operator when the orchestrator sits in a turn that makes no
//! progress. A model request that never answers shows nowhere else: no
//! event ends the turn, so nothing revives or wakes the orchestrator
//! (issue #223: a turn hung for almost three hours after a `git push`).

use std::sync::Arc;
use std::time::Duration;

use archon_aether::events::EventKind;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;

use crate::context::DaemonContext;
use crate::feed;

/// Silence in an open turn after which the operator is told. Longer than any
/// foreground command (Aether caps those at ten minutes); the orchestrator
/// otherwise emits a message or a usage update at least every few minutes.
pub const STALL_AFTER: Duration = Duration::from_secs(30 * 60);
pub const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Nudges one orchestrator generation gets in total, however often it stalls.
pub const MAX_NUDGES: u8 = 3;

/// A stalled turn found by [`Liveness::take_stall`].
#[derive(Debug, PartialEq, Eq)]
pub struct Stall {
    pub agent: AetherAgentId,
    pub silent: Duration,
    pub nudge: bool,
}

/// What the event stream last showed of the orchestrator's turn.
#[derive(Debug, Default)]
pub struct Liveness {
    agent: Option<AetherAgentId>,
    turn_open: bool,
    last_event: Option<Timestamp>,
    reported: bool,
    nudges: u8,
    probed: bool,
}

impl Liveness {
    /// Record an event of `agent`, the current orchestrator, seen at `now`.
    pub fn saw(&mut self, agent: &AetherAgentId, kind: &EventKind, now: Timestamp) {
        if self.agent.as_ref() != Some(agent) {
            *self = Self {
                agent: Some(agent.clone()),
                ..Self::default()
            };
        }
        if matches!(kind, EventKind::InboxDelivered { .. }) {
            // Mail arriving proves nothing about a turn that is not reading it.
            return;
        }
        match kind {
            // A message or a consumed inbox item also proves a turn is open, as
            // after an archond restart that skipped its `agent.started`.
            EventKind::AgentStarted { .. }
            | EventKind::AgentMessage {
                assistant: true, ..
            }
            | EventKind::InboxConsumed { .. } => self.turn_open = true,
            EventKind::AgentTurnCompleted { .. }
            | EventKind::AgentFailed { .. }
            | EventKind::AgentCancelled { .. }
            | EventKind::AgentCompleted => self.turn_open = false,
            _ => {}
        }
        self.last_event = Some(now);
        self.reported = false;
    }

    /// Whether `agent`'s state is still unknown: no event seen and no probe
    /// of Aether made since archond started.
    pub fn needs_probe(&self, agent: &AetherAgentId) -> bool {
        self.agent.as_ref() != Some(agent) || !self.probed
    }

    /// Start watching `agent` from `now` after archond restarted into a turn
    /// already open: the events that opened it were consumed before.
    pub fn probed(&mut self, agent: &AetherAgentId, turn_open: bool, now: Timestamp) {
        if self.agent.as_ref() != Some(agent) {
            *self = Self {
                agent: Some(agent.clone()),
                ..Self::default()
            };
        }
        self.probed = true;
        if !turn_open {
            return;
        }
        self.turn_open = true;
        self.last_event.get_or_insert(now);
    }

    /// The open turn, once, when it has been silent past `STALL_AFTER`.
    /// Reported once per silence: any progress event rearms it. It asks for
    /// a nudge for the first `MAX_NUDGES` stalls of this agent only.
    pub fn take_stall(&mut self, now: Timestamp) -> Option<Stall> {
        let last = self.last_event?;
        if !self.turn_open || self.reported {
            return None;
        }
        let silent = (now - last).to_std().unwrap_or(Duration::ZERO);
        if silent < STALL_AFTER {
            return None;
        }
        self.reported = true;
        let nudge = self.nudges < MAX_NUDGES;
        self.nudges += u8::from(nudge);
        Some(Stall {
            agent: self.agent.clone()?,
            silent,
            nudge,
        })
    }
}

/// What the operator reads; `nudge` says how the nudge to the orchestrator went.
pub(super) fn stall_text(silent: Duration, nudge: Nudge) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "a stall is minutes long");
    let nudge = match nudge {
        Nudge::Sent => "Archon sent it a nudge to continue; if it stays silent".to_owned(),
        Nudge::Spent => {
            format!("Archon already nudged it {MAX_NUDGES} times and sends no more; to recover it")
        }
        Nudge::Failed(error) => format!("Archon could not nudge it ({error}); to recover it"),
    };
    format!(
        "The orchestrator has been in one turn for {minutes} minutes without a message, tool \
         result or usage update; its model request may be hung, and nothing will wake it by \
         itself. {nudge}, run `archon orchestrator restart` to start a fresh one. Ignore this \
         if you know it is waiting on a long command."
    )
}

pub(super) enum Nudge {
    Sent,
    Spent,
    Failed(ArchonError),
}

/// Checks every [`STALL_CHECK_INTERVAL`]. Only a store failure ends it.
pub async fn watch_stalls(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::time::sleep(STALL_CHECK_INTERVAL).await;
        if let Err(error) = report_stall(&ctx, archon_core::now()).await {
            if error.is_store() {
                return Err(error);
            }
            tracing::warn!(%error, "reporting a stalled orchestrator failed");
        }
    }
}

/// Tell the operator if the orchestrator's turn is stalled as of `now`.
/// Returns whether it did.
pub async fn report_stall(ctx: &DaemonContext, now: Timestamp) -> Result<bool, ArchonError> {
    probe_after_restart(ctx, now).await?;
    let stall = ctx
        .supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .take_stall(now);
    let Some(stall) = stall else {
        return Ok(false);
    };
    let outcome = if stall.nudge {
        match nudge(ctx, &stall, now).await {
            Ok(()) => Nudge::Sent,
            Err(error) => {
                tracing::warn!(%error, "nudging the stalled orchestrator failed");
                Nudge::Failed(error)
            }
        }
    } else {
        Nudge::Spent
    };
    feed::notify(
        ctx,
        NotificationKind::Blocker,
        stall_text(stall.silent, outcome),
        None,
        None,
    )
    .await?;
    Ok(true)
}

async fn nudge(ctx: &DaemonContext, stall: &Stall, now: Timestamp) -> Result<(), ArchonError> {
    assert!(stall.nudge, "only a stall that asked for a nudge gets one");
    let minutes = stall.silent.as_secs() / 60;
    let text = format!(
        "Continue: your turn has made no progress for {minutes} minutes. If you ended it with \
         work still pending, such as watching a release build, pick that work up now."
    );
    let key = format!("stall-nudge:{}:{}", stall.agent, now.timestamp());
    ctx.backend.send_message(&stall.agent, &text, &key).await
}

/// Learn once from Aether whether the orchestrator is mid-turn, for an
/// archond that restarted into a turn whose events it had already consumed.
async fn probe_after_restart(ctx: &DaemonContext, now: Timestamp) -> Result<(), ArchonError> {
    let Some(agent) = super::current_orchestrator(ctx).await? else {
        return Ok(());
    };
    let unknown = ctx
        .supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .needs_probe(&agent);
    if !unknown {
        return Ok(());
    }
    let found = match ctx.aether.client() {
        Ok(client) => client.get_agent(&agent).await,
        Err(error) => Err(error),
    };
    let running = match found {
        Ok(found) => found.status["state"] == "running",
        Err(error) => {
            tracing::warn!(%error, "could not read the orchestrator's state for the stall check");
            return Ok(());
        }
    };
    ctx.supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .probed(&agent, running, now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn agent(id: &str) -> AetherAgentId {
        AetherAgentId::parse(id).unwrap()
    }

    const A: &str = "agt_01M5H1GBDP193XE44D522J0DN6";
    const B: &str = "agt_01M5H1SFJV2J2RNXYAH7HB6M6B";
    const STALL: i64 = STALL_AFTER.as_secs() as i64;

    fn started() -> EventKind {
        EventKind::AgentStarted { turn: 241 }
    }

    fn ended() -> EventKind {
        EventKind::AgentTurnCompleted {
            turn: 241,
            next: Some("ready".into()),
        }
    }

    #[test]
    fn a_turn_that_goes_silent_is_reported_once() {
        let mut live = Liveness::default();
        live.saw(&agent(A), &started(), at(0));
        assert_eq!(live.take_stall(at(STALL - 1)), None);
        assert_eq!(live.take_stall(at(STALL)).unwrap().silent, STALL_AFTER);
        assert_eq!(live.take_stall(at(STALL + 60)), None, "told once");
    }

    #[test]
    fn any_event_rearms_the_report() {
        let mut live = Liveness::default();
        live.saw(&agent(A), &started(), at(0));
        assert!(live.take_stall(at(STALL)).is_some());
        let message = EventKind::AgentMessage {
            assistant: true,
            text: "still here".into(),
        };
        live.saw(&agent(A), &message, at(STALL + 10));
        assert_eq!(live.take_stall(at(STALL + 20)), None);
        assert!(live.take_stall(at(2 * STALL + 10)).is_some());
    }

    #[test]
    fn a_turn_seen_only_by_its_messages_after_a_restart_is_watched() {
        let mut live = Liveness::default();
        let message = EventKind::AgentMessage {
            assistant: true,
            text: "pushing the tag".into(),
        };
        live.saw(&agent(A), &message, at(0));
        assert!(live.take_stall(at(STALL)).is_some());
    }

    #[test]
    fn an_idle_orchestrator_is_not_stalled() {
        let mut live = Liveness::default();
        assert_eq!(live.take_stall(at(10 * STALL)), None, "nothing seen yet");
        live.saw(&agent(A), &started(), at(0));
        live.saw(&agent(A), &ended(), at(5));
        assert_eq!(live.take_stall(at(10 * STALL)), None, "the turn ended");
    }

    #[test]
    fn a_replaced_orchestrator_starts_clean() {
        let mut live = Liveness::default();
        live.saw(&agent(A), &started(), at(0));
        live.saw(&agent(B), &EventKind::AgentCompleted, at(10));
        assert_eq!(live.take_stall(at(10 * STALL)), None);
    }

    #[test]
    fn mail_delivered_to_a_hung_turn_does_not_rearm_the_report() {
        let mut live = Liveness::default();
        live.saw(&agent(A), &started(), at(0));
        let mail = EventKind::InboxDelivered {
            seq: 7,
            from_operator: false,
            message: None,
        };
        live.saw(&agent(A), &mail, at(STALL - 10));
        assert!(live.take_stall(at(STALL)).is_some());
    }

    #[test]
    fn nudges_are_bounded_per_agent() {
        let mut live = Liveness::default();
        let mut nudged = 0;
        for round in 0..10 {
            let base = round * (STALL + 100);
            live.saw(&agent(A), &started(), at(base));
            let stall = live.take_stall(at(base + STALL)).expect("stalls again");
            nudged += i32::from(stall.nudge);
        }
        assert_eq!(nudged, i32::from(MAX_NUDGES));
    }

    #[test]
    fn a_turn_found_open_after_a_restart_is_watched_from_the_probe() {
        let mut live = Liveness::default();
        assert!(live.needs_probe(&agent(A)));
        live.probed(&agent(A), true, at(0));
        assert!(!live.needs_probe(&agent(A)));
        assert!(live.take_stall(at(STALL - 1)).is_none());
        assert!(live.take_stall(at(STALL)).is_some());
    }

    #[test]
    fn a_progress_event_before_the_probe_does_not_skip_it() {
        let mut live = Liveness::default();
        let usage = EventKind::Other {
            type_name: "tool.result".into(),
        };
        live.saw(&agent(A), &usage, at(0));
        assert!(live.needs_probe(&agent(A)));
        live.probed(&agent(A), true, at(5));
        assert!(live.take_stall(at(STALL)).is_some());
    }

    #[test]
    fn an_idle_orchestrator_found_by_the_probe_is_not_watched() {
        let mut live = Liveness::default();
        live.probed(&agent(A), false, at(0));
        assert!(live.take_stall(at(10 * STALL)).is_none());
    }

    #[test]
    fn the_notice_says_what_happened_and_what_to_do() {
        let hours = Duration::from_secs(3 * 3600);
        let text = stall_text(hours, Nudge::Sent);
        assert!(text.contains("180 minutes"), "{text}");
        assert!(text.contains("archon orchestrator restart"), "{text}");
        assert!(text.contains("sent it a nudge"), "{text}");
        let spent = stall_text(hours, Nudge::Spent);
        assert!(spent.contains("sends no more"), "{spent}");
        let failed = stall_text(hours, Nudge::Failed(ArchonError::Invalid("no".into())));
        assert!(failed.contains("could not nudge"), "{failed}");
    }
}
