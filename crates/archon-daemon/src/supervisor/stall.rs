//! Recovering the orchestrator from a turn that makes no progress. A model
//! request or tool call that never answers shows nowhere else: no event ends
//! the turn, and mail sent to it queues unread behind it (issues #223, #393).
//! archond nudges such a turn, tells the operator, and interrupts it, as Esc
//! would, when the nudge brings no progress within [`INTERRUPT_GRACE`].

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
/// The same silence for a turn the operator's mail is waiting on: past the
/// ten-minute foreground cap, so a command still running would have ended.
pub const MAIL_STALL_AFTER: Duration = Duration::from_secs(10 * 60);
/// How long a reported stall may stay silent before its turn is interrupted.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(5 * 60);
pub const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Nudges one orchestrator generation gets in total, however often it stalls.
pub const MAX_NUDGES: u8 = 3;
/// Interrupts one orchestrator generation gets in total, as for nudges.
pub const MAX_INTERRUPTS: u8 = 3;

/// A stalled turn found by [`Liveness::take_stall`].
#[derive(Debug, PartialEq, Eq)]
pub struct Stall {
    pub agent: AetherAgentId,
    pub silent: Duration,
    pub nudge: bool,
    /// Whether an interrupt is left for this generation to follow the grace.
    pub interrupt: bool,
}

/// A reported stall still silent after [`INTERRUPT_GRACE`], found by
/// [`Liveness::take_interrupt`].
#[derive(Debug, PartialEq, Eq)]
pub struct Hung {
    pub agent: AetherAgentId,
    pub silent: Duration,
}

/// What the event stream last showed of the orchestrator's turn.
#[derive(Debug, Default)]
pub struct Liveness {
    agent: Option<AetherAgentId>,
    turn_open: bool,
    last_event: Option<Timestamp>,
    /// When this silence was reported; cleared by any progress event.
    reported_at: Option<Timestamp>,
    /// Whether this silence was already interrupted.
    interrupted: bool,
    /// Operator mail was delivered during this silence.
    operator_waiting: bool,
    nudges: u8,
    interrupts: u8,
    probed: bool,
}

fn since(earlier: Timestamp, now: Timestamp) -> Duration {
    (now - earlier).to_std().unwrap_or(Duration::ZERO)
}

impl Liveness {
    /// Record an event of `agent`, the current orchestrator, seen at `now`.
    /// True when it is operator mail that makes a stall due at once.
    pub fn saw(&mut self, agent: &AetherAgentId, kind: &EventKind, now: Timestamp) -> bool {
        if self.agent.as_ref() != Some(agent) {
            *self = Self {
                agent: Some(agent.clone()),
                ..Self::default()
            };
        }
        if let EventKind::InboxDelivered { from_operator, .. } = kind {
            // Mail arriving proves nothing about a turn that is not reading it.
            return *from_operator && self.mail_waits_on_a_stall(now);
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
        self.reported_at = None;
        self.interrupted = false;
        self.operator_waiting = false;
        false
    }

    /// Note operator mail delivered at `now`; true when the open turn has
    /// already been silent past [`MAIL_STALL_AFTER`] and is not yet reported.
    fn mail_waits_on_a_stall(&mut self, now: Timestamp) -> bool {
        let Some(last) = self.last_event.filter(|_| self.turn_open) else {
            return false;
        };
        self.operator_waiting = true;
        self.reported_at.is_none() && since(last, now) >= MAIL_STALL_AFTER
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

    /// The open turn, once, when it has been silent past `STALL_AFTER`, or
    /// past `MAIL_STALL_AFTER` with operator mail waiting. Reported once per
    /// silence: any progress event rearms it. It asks for a nudge for the
    /// first `MAX_NUDGES` stalls of this agent only.
    pub fn take_stall(&mut self, now: Timestamp) -> Option<Stall> {
        let last = self.last_event?;
        if !self.turn_open || self.reported_at.is_some() {
            return None;
        }
        let silent = since(last, now);
        let limit = match self.operator_waiting {
            true => MAIL_STALL_AFTER,
            false => STALL_AFTER,
        };
        if silent < limit {
            return None;
        }
        assert!(self.nudges <= MAX_NUDGES, "nudges stay bounded");
        assert!(
            !self.interrupted,
            "an unreported silence was never interrupted"
        );
        self.reported_at = Some(now);
        let nudge = self.nudges < MAX_NUDGES;
        self.nudges += u8::from(nudge);
        Some(Stall {
            agent: self.agent.clone()?,
            silent,
            nudge,
            interrupt: self.interrupts < MAX_INTERRUPTS,
        })
    }

    /// `current`'s reported stall, once, when it is still silent
    /// `INTERRUPT_GRACE` after the report, for the first `MAX_INTERRUPTS`.
    pub fn take_interrupt(&mut self, current: &AetherAgentId, now: Timestamp) -> Option<Hung> {
        let (Some(last), Some(reported)) = (self.last_event, self.reported_at) else {
            return None;
        };
        assert!(reported >= last, "a silence is reported after it began");
        assert!(self.interrupts <= MAX_INTERRUPTS, "interrupts stay bounded");
        let spent = self.interrupts >= MAX_INTERRUPTS;
        if self.agent.as_ref() != Some(current) || !self.turn_open || self.interrupted || spent {
            return None;
        }
        if since(reported, now) < INTERRUPT_GRACE {
            return None;
        }
        self.interrupted = true;
        self.interrupts += 1;
        Some(Hung {
            agent: current.clone(),
            silent: since(last, now),
        })
    }
}

/// What the operator reads; `nudge` says how the nudge to the orchestrator
/// went and `interrupt` whether an interrupt follows the grace.
pub(super) fn stall_text(silent: Duration, nudge: Nudge, interrupt: bool) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "a stall is minutes long");
    let nudge = match nudge {
        Nudge::Sent => "Archon sent it a nudge to continue.".to_owned(),
        Nudge::Spent => {
            format!("Archon has used its {MAX_NUDGES} nudge attempts on it and sends no more.")
        }
        Nudge::Failed(error) => format!("Archon could not nudge it ({error})."),
    };
    let grace = INTERRUPT_GRACE.as_secs() / 60;
    let next = match interrupt {
        true => format!(
            "If it is still silent in {grace} minutes, Archon interrupts the turn, as Esc in \
             its terminal would, so that waiting messages reach a fresh turn; if that fails"
        ),
        false => format!(
            "Archon has used its {MAX_INTERRUPTS} interrupts on this orchestrator; to recover it"
        ),
    };
    format!(
        "The orchestrator has been in one turn for {minutes} minutes without a message, tool \
         result or usage update; its model request or a tool call may be hung, and nothing \
         will wake it by itself. {nudge} {next}, run `archon orchestrator restart` to start a \
         fresh one."
    )
}

pub(super) enum Nudge {
    Sent,
    Spent,
    Failed(ArchonError),
}

/// What the interrupt of a hung turn did.
pub(super) enum Interrupted {
    Stopped,
    NoTurn,
    Failed(ArchonError),
}

/// What the operator reads after archond tried to interrupt a hung turn.
pub(super) fn interrupt_text(silent: Duration, outcome: &Interrupted) -> String {
    let minutes = silent.as_secs() / 60;
    assert!(minutes > 0, "an interrupt follows minutes of silence");
    let grace = INTERRUPT_GRACE.as_secs() / 60;
    let head = format!(
        "The orchestrator's turn was silent for {minutes} minutes, with no message, tool \
         result or usage update in the {grace} minutes after Archon's notice"
    );
    match outcome {
        Interrupted::Stopped => format!(
            "{head}, so Archon interrupted it, as Esc in its terminal would. Messages waiting \
             for it, yours included, are delivered in a fresh turn. If it stays silent, run \
             `archon orchestrator restart` to start a fresh orchestrator."
        ),
        Interrupted::NoTurn => format!(
            "{head}. Archon tried to interrupt it, but Aether reports no turn running, so \
             nothing was interrupted. If it stays silent, run `archon orchestrator restart` \
             to start a fresh one."
        ),
        Interrupted::Failed(error) => format!(
            "{head}. Archon could not interrupt it ({error}). Its model request or a tool call \
             may be hung, and nothing will wake it by itself: run `archon orchestrator \
             restart` to start a fresh one."
        ),
    }
}

/// Checks every [`STALL_CHECK_INTERVAL`], and at once when operator mail
/// reaches a stalled turn. Only a store failure ends it.
pub async fn watch_stalls(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::select! {
            () = tokio::time::sleep(STALL_CHECK_INTERVAL) => {}
            () = ctx.supervisor.stall_check.notified() => {}
        }
        if let Err(error) = report_stall(&ctx, archon_core::now()).await {
            if error.is_store() {
                return Err(error);
            }
            tracing::warn!(%error, "reporting a stalled orchestrator failed");
        }
    }
}

/// Tell the operator if the orchestrator's turn is stalled as of `now`, and
/// interrupt a reported stall still silent after the grace. Returns whether
/// it did either.
pub async fn report_stall(ctx: &DaemonContext, now: Timestamp) -> Result<bool, ArchonError> {
    let current = probe_after_restart(ctx, now).await?;
    let (stall, hung) = {
        let mut liveness = ctx
            .supervisor
            .liveness
            .lock()
            .expect("liveness is never held across a panic");
        let stall = liveness.take_stall(now);
        let hung = match (&stall, &current) {
            (None, Some(current)) => liveness.take_interrupt(current, now),
            _ => None,
        };
        (stall, hung)
    };
    if let Some(stall) = stall {
        report(ctx, &stall, now).await?;
        return Ok(true);
    }
    let Some(hung) = hung else {
        return Ok(false);
    };
    interrupt(ctx, &hung).await?;
    Ok(true)
}

async fn report(ctx: &DaemonContext, stall: &Stall, now: Timestamp) -> Result<(), ArchonError> {
    let outcome = if stall.nudge {
        match nudge(ctx, stall, now).await {
            Ok(()) => Nudge::Sent,
            Err(error) => {
                tracing::warn!(%error, "nudging the stalled orchestrator failed");
                Nudge::Failed(error)
            }
        }
    } else {
        Nudge::Spent
    };
    let text = stall_text(stall.silent, outcome, stall.interrupt);
    feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    Ok(())
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

/// Interrupt the hung turn, as the operator's Esc would: Aether then delivers
/// the queued inbox in a fresh turn. A failure is told, never retried.
async fn interrupt(ctx: &DaemonContext, hung: &Hung) -> Result<(), ArchonError> {
    let minutes = hung.silent.as_secs() / 60;
    let outcome = match ctx.backend.interrupt(&hung.agent).await {
        Ok(true) => Interrupted::Stopped,
        Ok(false) => Interrupted::NoTurn,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => Interrupted::Failed(error),
    };
    let kind = match &outcome {
        Interrupted::Stopped => {
            tracing::warn!(agent = %hung.agent, minutes, "orchestrator turn hung; interrupted it");
            NotificationKind::Decision
        }
        Interrupted::NoTurn => {
            tracing::warn!(agent = %hung.agent, minutes, "orchestrator hung, but no turn to interrupt");
            NotificationKind::Blocker
        }
        Interrupted::Failed(error) => {
            tracing::warn!(agent = %hung.agent, minutes, %error, "interrupting the hung orchestrator failed");
            NotificationKind::Blocker
        }
    };
    feed::notify(ctx, kind, interrupt_text(hung.silent, &outcome), None, None).await?;
    Ok(())
}

/// Learn once from Aether whether the orchestrator is mid-turn, for an
/// archond that restarted into a turn whose events it had already consumed.
/// Returns the current orchestrator.
async fn probe_after_restart(
    ctx: &DaemonContext,
    now: Timestamp,
) -> Result<Option<AetherAgentId>, ArchonError> {
    let Some(agent) = super::current_orchestrator(ctx).await? else {
        return Ok(None);
    };
    let unknown = ctx
        .supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .needs_probe(&agent);
    if !unknown {
        return Ok(Some(agent));
    }
    let found = match ctx.aether.client() {
        Ok(client) => client.get_agent(&agent).await,
        Err(error) => Err(error),
    };
    let running = match found {
        Ok(found) => found.status["state"] == "running",
        Err(error) => {
            tracing::warn!(%error, "could not read the orchestrator's state for the stall check");
            return Ok(Some(agent));
        }
    };
    ctx.supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
        .probed(&agent, running, now);
    Ok(Some(agent))
}

#[cfg(test)]
mod tests;
