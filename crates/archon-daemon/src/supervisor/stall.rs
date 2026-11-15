//! Recovering the orchestrator from a turn that makes no progress. A model
//! request or tool call that never answers shows nowhere else: no event ends
//! the turn, and mail sent to it queues unread behind it (issues #223, #393).
//! Operator mail waits at most [`MAIL_SILENCE`], or the running call's own
//! bound, before archond interrupts the turn as Esc would. Without mail, a
//! turn silent for [`STALL_AFTER`] is nudged and told, then interrupted when
//! the nudge brings no progress within [`NUDGE_GRACE`].

use std::sync::Arc;
use std::time::Duration;

use archon_aether::events::EventKind;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ports::OpenToolCall;

use crate::context::DaemonContext;
use crate::feed;

mod notice;

use notice::{Interrupted, Nudge, interrupt_text, nudge_text, stall_text};

/// Silence in an open turn, with no operator mail waiting, after which the
/// operator is told and the turn nudged.
pub const STALL_AFTER: Duration = Duration::from_secs(30 * 60);
/// Silence after which waiting operator mail interrupts a turn with no tool
/// call running, or one whose own bound ends sooner.
pub const MAIL_SILENCE: Duration = Duration::from_secs(3 * 60);
/// How long past its declared timeout a running call may keep mail waiting.
pub const PAST_TIMEOUT: Duration = Duration::from_secs(60);
/// The bound for a running call that declares no timeout.
pub const UNDECLARED_CALL_BOUND: Duration = Duration::from_secs(4 * 60);
/// Aether's cap on a foreground command: no declared timeout runs longer.
pub const FOREGROUND_CAP: Duration = Duration::from_secs(10 * 60);
/// The bound while the open call cannot be read: the longest a foreground
/// call may take, so no healthy command is cut off.
pub const UNKNOWN_CALL_BOUND: Duration = Duration::from_secs(11 * 60);
/// How long a nudged stall may stay silent before its turn is interrupted.
pub const NUDGE_GRACE: Duration = Duration::from_secs(2 * 60);
/// The least time between two interrupts of the orchestrator.
pub const INTERRUPT_SPACING: Duration = Duration::from_secs(5 * 60);
pub const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Nudges one orchestrator generation gets in total, however often it stalls.
pub const MAX_NUDGES: u8 = 3;
/// Archon's own notes awaiting their delivery, at most: a few per silence.
const PENDING_NOTES: usize = 8;

/// A stalled turn found by [`Liveness::take_stall`].
#[derive(Debug, PartialEq, Eq)]
pub struct Stall {
    pub agent: AetherAgentId,
    pub silent: Duration,
    pub nudge: bool,
}

/// A silence an interrupt may be due for, found by [`Liveness::candidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silence {
    pub agent: AetherAgentId,
    pub since: Timestamp,
    /// Operator mail waits on it; else it is a nudged stall.
    pub mail: bool,
}

/// A silent turn archond interrupts, from [`Liveness::take_interrupt`].
#[derive(Debug, PartialEq, Eq)]
pub struct Hung {
    pub agent: AetherAgentId,
    pub silent: Duration,
    pub mail: bool,
}

/// What the event stream last showed of the orchestrator's turn.
#[derive(Debug, Default)]
pub struct Liveness {
    agent: Option<AetherAgentId>,
    turn_open: bool,
    last_event: Option<Timestamp>,
    /// When this silence was reported as a stall; cleared by progress.
    reported_at: Option<Timestamp>,
    /// Whether this silence was already interrupted.
    interrupted: bool,
    /// The newest operator mail delivered to the open turn and not yet
    /// consumed: progress alone does not show it was read.
    mail_waiting: Option<u64>,
    /// Archon's own notes sent and not yet delivered, whole: they reach the
    /// inbox as operator mail would, and only their exact text tells them apart.
    pending_notes: Vec<String>,
    nudges: u8,
    last_interrupt: Option<Timestamp>,
    probed: bool,
}

fn since(earlier: Timestamp, now: Timestamp) -> Duration {
    (now - earlier).to_std().unwrap_or(Duration::ZERO)
}

fn after(at: Timestamp, span: Duration) -> Timestamp {
    at + chrono::TimeDelta::from_std(span).expect("watchdog bounds are minutes long")
}

/// How long `call` may run while operator mail waits: its declared timeout,
/// capped as Aether caps it, plus [`PAST_TIMEOUT`].
pub fn call_bound(call: &OpenToolCall) -> Duration {
    let bound = match call.timeout {
        Some(timeout) => timeout.min(FOREGROUND_CAP) + PAST_TIMEOUT,
        None => UNDECLARED_CALL_BOUND,
    };
    assert!(bound >= PAST_TIMEOUT, "a call always gets its grace");
    assert!(
        bound <= FOREGROUND_CAP + PAST_TIMEOUT,
        "no call keeps mail waiting past Aether's foreground cap"
    );
    bound
}

/// When waiting operator mail interrupts a turn silent since `since`:
/// [`MAIL_SILENCE`] after it, or later while the open call is in its bound.
/// `Err` when the open call could not be read: [`UNKNOWN_CALL_BOUND`] holds.
pub fn mail_deadline(
    since: Timestamp,
    call: &Result<Option<OpenToolCall>, ArchonError>,
) -> Timestamp {
    let quiet = after(since, MAIL_SILENCE);
    let running = match call {
        Ok(None) => return quiet,
        Ok(Some(call)) => after(call.requested_at, call_bound(call)),
        Err(_) => after(since, UNKNOWN_CALL_BOUND),
    };
    let deadline = quiet.max(running);
    assert!(deadline >= quiet, "a running call never shortens the wait");
    assert!(deadline >= running, "a call keeps its whole bound");
    deadline
}

impl Liveness {
    /// Record an event of `agent`, the current orchestrator, seen at `now`.
    /// True when it is operator mail that makes a check due at once.
    pub fn saw(&mut self, agent: &AetherAgentId, kind: &EventKind, now: Timestamp) -> bool {
        if self.agent.as_ref() != Some(agent) {
            *self = Self {
                agent: Some(agent.clone()),
                ..Self::default()
            };
        }
        if let EventKind::InboxDelivered {
            seq,
            from_operator,
            message,
        } = kind
        {
            let own = message.as_ref().is_some_and(|m| self.take_note(&m.text));
            // Mail arriving proves nothing about a turn that is not reading it.
            return *from_operator && !own && self.mail_waits(*seq, now);
        }
        if let EventKind::InboxConsumed { up_to } = kind
            && self.mail_waiting.is_some_and(|seq| seq <= *up_to)
        {
            self.mail_waiting = None;
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
        false
    }

    /// Note operator mail `seq` delivered at `now`; true when the open turn
    /// has already been silent for [`MAIL_SILENCE`] and is not yet interrupted.
    fn mail_waits(&mut self, seq: u64, now: Timestamp) -> bool {
        let Some(last) = self.last_event.filter(|_| self.turn_open) else {
            return false;
        };
        let newest = self.mail_waiting.map_or(seq, |waiting| waiting.max(seq));
        self.mail_waiting = Some(newest);
        !self.interrupted && since(last, now) >= MAIL_SILENCE
    }

    /// Expect `note`, about to be sent to the orchestrator, back in its
    /// inbox, where it is not operator mail.
    pub fn sending_note(&mut self, note: &str) {
        assert!(!note.trim().is_empty(), "a note says something");
        if self.pending_notes.len() == PENDING_NOTES {
            self.pending_notes.remove(0);
        }
        self.pending_notes.push(note.to_owned());
        assert!(
            self.pending_notes.len() <= PENDING_NOTES,
            "pending notes stay bounded"
        );
    }

    /// Whether `text` is one of Archon's pending notes, now delivered.
    fn take_note(&mut self, text: &str) -> bool {
        let Some(at) = self.pending_notes.iter().position(|note| note == text) else {
            return false;
        };
        self.pending_notes.remove(at);
        true
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

    /// The open turn, once, when it has been silent past `STALL_AFTER` with
    /// no operator mail waiting (that mail interrupts it instead). Any
    /// progress event rearms it. Only the first `MAX_NUDGES` get a nudge.
    pub fn take_stall(&mut self, now: Timestamp) -> Option<Stall> {
        let silent = self.stall_silence(now)?;
        assert!(self.nudges <= MAX_NUDGES, "nudges stay bounded");
        assert!(
            !self.interrupted,
            "an unreported silence is never interrupted"
        );
        self.reported_at = Some(now);
        let nudge = self.nudges < MAX_NUDGES;
        self.nudges += u8::from(nudge);
        Some(Stall {
            agent: self.agent.clone()?,
            silent,
            nudge,
        })
    }

    /// How long the open turn has been silent, once that is a stall not yet
    /// reported and no operator mail waits on it.
    fn stall_silence(&self, now: Timestamp) -> Option<Duration> {
        let last = self.last_event?;
        if !self.turn_open || self.reported_at.is_some() || self.mail_waiting.is_some() {
            return None;
        }
        Some(since(last, now)).filter(|silent| *silent >= STALL_AFTER)
    }

    /// Whether `current` may be due a stall report or an interrupt at `now`,
    /// by Archon's own stream: only then is Aether asked what it saw.
    pub fn check_due(&self, current: &AetherAgentId, now: Timestamp) -> bool {
        if self.agent.as_ref() != Some(current) {
            return false;
        }
        self.stall_silence(now).is_some() || self.candidate(current, now).is_some()
    }

    /// Aether's newest turn event of `agent` was at `at`: a tool event among
    /// them, which Archon's stream lacks, is progress like any other.
    pub fn saw_activity(&mut self, agent: &AetherAgentId, at: Timestamp) {
        let Some(last) = self.last_event else {
            return;
        };
        if self.agent.as_ref() != Some(agent) || !self.turn_open || at <= last {
            return;
        }
        self.last_event = Some(at);
        self.reported_at = None;
        self.interrupted = false;
        assert!(
            self.last_event > Some(last),
            "activity only moves the silence later"
        );
    }

    /// `current`'s silence an interrupt may be due for: operator mail waiting
    /// for [`MAIL_SILENCE`] (its running call's bound is judged by the
    /// caller), or a reported stall still silent [`NUDGE_GRACE`] after it.
    pub fn candidate(&self, current: &AetherAgentId, now: Timestamp) -> Option<Silence> {
        let last = self.last_event?;
        if self.agent.as_ref() != Some(current) || !self.turn_open || self.interrupted {
            return None;
        }
        let mail = self.mail_waiting.is_some() && since(last, now) >= MAIL_SILENCE;
        let nudged = self
            .reported_at
            .is_some_and(|reported| since(reported, now) >= NUDGE_GRACE);
        if !mail && !nudged {
            return None;
        }
        Some(Silence {
            agent: current.clone(),
            since: last,
            mail,
        })
    }

    /// Claim the interrupt of `silence` at `now`, once, unless progress came
    /// since it was found or the last interrupt is under
    /// [`INTERRUPT_SPACING`] old; that one is retried at a later check.
    pub fn take_interrupt(&mut self, silence: &Silence, now: Timestamp) -> Option<Hung> {
        if self.candidate(&silence.agent, now).as_ref() != Some(silence) {
            return None;
        }
        assert!(self.turn_open, "only an open turn is a candidate");
        if self
            .last_interrupt
            .is_some_and(|last| since(last, now) < INTERRUPT_SPACING)
        {
            return None;
        }
        self.interrupted = true;
        self.last_interrupt = Some(now);
        assert!(
            silence.since <= now,
            "a silence begins before its interrupt"
        );
        Some(Hung {
            agent: silence.agent.clone(),
            silent: since(silence.since, now),
            mail: silence.mail,
        })
    }

    /// The interrupt of this silence failed: the next check after
    /// [`INTERRUPT_SPACING`] tries again.
    pub fn interrupt_failed(&mut self, agent: &AetherAgentId) {
        if self.agent.as_ref() != Some(agent) {
            return;
        }
        assert!(
            self.last_interrupt.is_some(),
            "only a taken interrupt fails"
        );
        self.interrupted = false;
    }
}

/// Checks every [`STALL_CHECK_INTERVAL`], and at once when operator mail
/// reaches a silent turn. Only a store failure ends it.
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
/// interrupt a silence that operator mail or a nudged stall made due.
/// Returns whether it did either.
pub async fn report_stall(ctx: &DaemonContext, now: Timestamp) -> Result<bool, ArchonError> {
    let Some(current) = probe_after_restart(ctx, now).await? else {
        return Ok(false);
    };
    if !lock(ctx).check_due(&current, now) {
        return Ok(false);
    }
    let (seen, call) = read_turn(ctx, &current).await?;
    if let Some(seen) = seen {
        lock(ctx).saw_activity(&current, seen);
    }
    let stall = lock(ctx).take_stall(now);
    if let Some(stall) = stall {
        report(ctx, &stall, now).await?;
        return Ok(true);
    }
    let Some(silence) = lock(ctx).candidate(&current, now) else {
        return Ok(false);
    };
    if silence.mail && now < mail_deadline(silence.since, &call) {
        return Ok(false);
    }
    let Some(hung) = lock(ctx).take_interrupt(&silence, now) else {
        return Ok(false);
    };
    interrupt(ctx, &hung, &call, now).await?;
    Ok(true)
}

/// What Aether saw of `agent`'s turn: its newest event, and the open call,
/// `Err` when that cannot be told. Only a store failure is returned as one.
async fn read_turn(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<(Option<Timestamp>, Result<Option<OpenToolCall>, ArchonError>), ArchonError> {
    match ctx.backend.turn_activity(agent).await {
        Ok(activity) if activity.call_known => Ok((activity.last_event_at, Ok(activity.open_call))),
        Ok(activity) => {
            let unknown = "its turn began before the newest events Aether returned";
            Ok((
                activity.last_event_at,
                Err(ArchonError::Unavailable(unknown.into())),
            ))
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "could not read the orchestrator's turn from Aether");
            Ok((None, Err(error)))
        }
    }
}

fn lock(ctx: &DaemonContext) -> std::sync::MutexGuard<'_, Liveness> {
    ctx.supervisor
        .liveness
        .lock()
        .expect("liveness is never held across a panic")
}

async fn report(ctx: &DaemonContext, stall: &Stall, now: Timestamp) -> Result<(), ArchonError> {
    let outcome = if stall.nudge {
        match nudge(ctx, stall, now).await {
            Ok(()) => Nudge::Sent,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::warn!(%error, "nudging the stalled orchestrator failed");
                Nudge::Failed(error)
            }
        }
    } else {
        Nudge::Spent
    };
    let text = stall_text(stall.silent, outcome);
    feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    Ok(())
}

async fn nudge(ctx: &DaemonContext, stall: &Stall, now: Timestamp) -> Result<(), ArchonError> {
    assert!(stall.nudge, "only a stall that asked for a nudge gets one");
    let key = format!("stall-nudge:{}:{}", stall.agent, now.timestamp());
    let note = nudge_text(stall.silent);
    lock(ctx).sending_note(&note);
    ctx.backend.send_message(&stall.agent, &note, &key).await
}

/// Interrupt the hung turn, as the operator's Esc would: Aether then delivers
/// the queued inbox in a fresh turn. Only a turn Aether reports stopped is
/// told which call was cut off, so the note is never false.
async fn interrupt(
    ctx: &DaemonContext,
    hung: &Hung,
    call: &Result<Option<OpenToolCall>, ArchonError>,
    now: Timestamp,
) -> Result<(), ArchonError> {
    let minutes = hung.silent.as_secs() / 60;
    let outcome = match ctx.backend.interrupt(&hung.agent).await {
        Ok(true) => Interrupted::Stopped {
            told: tell_cut_off(ctx, hung, call, now).await?,
        },
        Ok(false) => Interrupted::NoTurn,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => Interrupted::Failed(error),
    };
    let kind = match &outcome {
        Interrupted::Stopped { .. } => {
            tracing::warn!(agent = %hung.agent, minutes, mail = hung.mail, "orchestrator turn hung; interrupted it");
            NotificationKind::Decision
        }
        Interrupted::NoTurn => {
            tracing::warn!(agent = %hung.agent, minutes, "orchestrator hung, but no turn to interrupt");
            NotificationKind::Blocker
        }
        Interrupted::Failed(error) => {
            tracing::warn!(agent = %hung.agent, minutes, %error, "interrupting the hung orchestrator failed");
            lock(ctx).interrupt_failed(&hung.agent);
            NotificationKind::Blocker
        }
    };
    let text = interrupt_text(hung, call, &outcome);
    feed::notify(ctx, kind, text, None, None).await?;
    Ok(())
}

/// Tell the interrupted orchestrator which call was cut off. `Err` inside
/// when the note could not be sent; only a store failure is returned as one.
async fn tell_cut_off(
    ctx: &DaemonContext,
    hung: &Hung,
    call: &Result<Option<OpenToolCall>, ArchonError>,
    now: Timestamp,
) -> Result<Result<(), ArchonError>, ArchonError> {
    let note = notice::cut_off_note(hung, call);
    let key = format!("stall-interrupt:{}:{}", hung.agent, now.timestamp());
    lock(ctx).sending_note(&note);
    match ctx.backend.send_message(&hung.agent, &note, &key).await {
        Ok(()) => Ok(Ok(())),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(agent = %hung.agent, %error, "telling the orchestrator its cut-off call failed");
            Ok(Err(error))
        }
    }
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
    if !lock(ctx).needs_probe(&agent) {
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
    lock(ctx).probed(&agent, running, now);
    Ok(Some(agent))
}

#[cfg(test)]
mod tests;
