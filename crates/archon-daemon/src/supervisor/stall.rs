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

/// What the event stream last showed of the orchestrator's turn.
#[derive(Debug, Default)]
pub struct Liveness {
    agent: Option<AetherAgentId>,
    turn_open: bool,
    last_event: Option<Timestamp>,
    reported: bool,
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
        match kind {
            EventKind::AgentStarted { .. } => self.turn_open = true,
            EventKind::AgentTurnCompleted { .. }
            | EventKind::AgentFailed { .. }
            | EventKind::AgentCancelled { .. }
            | EventKind::AgentCompleted => self.turn_open = false,
            _ => {}
        }
        self.last_event = Some(now);
        self.reported = false;
    }

    /// How long the open turn has been silent, once, when it passes
    /// `STALL_AFTER`. Reported once per silence: any event rearms it.
    pub fn take_stall(&mut self, now: Timestamp) -> Option<Duration> {
        let last = self.last_event?;
        if !self.turn_open || self.reported {
            return None;
        }
        let silent = (now - last).to_std().unwrap_or(Duration::ZERO);
        if silent < STALL_AFTER {
            return None;
        }
        self.reported = true;
        Some(silent)
    }
}

pub(super) fn stall_text(silent: Duration) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "a stall is minutes long");
    format!(
        "The orchestrator has been in one turn for {minutes} minutes without a message, tool \
         result or usage update; its model request may be hung, and nothing will wake it by \
         itself. Send it a message to see if it answers, or run `archon orchestrator restart` \
         to start a fresh one. Ignore this if you know it is waiting on a long command."
    )
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
    let silent = ctx
        .supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .take_stall(now);
    let Some(silent) = silent else {
        return Ok(false);
    };
    feed::notify(
        ctx,
        NotificationKind::Blocker,
        stall_text(silent),
        None,
        None,
    )
    .await?;
    Ok(true)
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
        assert_eq!(live.take_stall(at(STALL)), Some(STALL_AFTER));
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
    fn the_notice_says_what_happened_and_what_to_do() {
        let text = stall_text(Duration::from_secs(3 * 3600));
        assert!(text.contains("180 minutes"), "{text}");
        assert!(text.contains("archon orchestrator restart"), "{text}");
    }
}
