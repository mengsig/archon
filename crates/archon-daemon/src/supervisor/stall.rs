//! Recovering the orchestrator from a hung turn, behind which mail queues unread (#223, #393).
//! Stateless: each check applies [`due`] to Aether's events and the stored [`Marks`].

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_aether::turn::{self, OpenCall, OpenTurn, Turn};
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::context::DaemonContext;
use crate::feed;

mod notice;

use notice::Interrupted;

/// Silence in a running turn, with no operator mail waiting, after which
/// the turn is nudged and the operator told.
pub const STALL_AFTER: Duration = Duration::from_secs(30 * 60);
/// How old unread operator mail, and how long the silence, must both be
/// before the mail gets the turn nudged.
pub const MAIL_SILENCE: Duration = Duration::from_secs(3 * 60);
/// How long past its declared timeout an open call may keep mail waiting.
pub const PAST_TIMEOUT: Duration = Duration::from_secs(60);
/// The bound of an open call that declares no timeout.
pub const UNDECLARED_CALL_BOUND: Duration = Duration::from_secs(4 * 60);
/// Aether's cap on a foreground command: no declared timeout runs longer.
pub const FOREGROUND_CAP: Duration = Duration::from_secs(10 * 60);
/// How long a nudged turn may stay silent before it is interrupted.
pub const NUDGE_GRACE: Duration = Duration::from_secs(2 * 60);
/// The least time between two interrupts of one orchestrator.
pub const INTERRUPT_SPACING: Duration = Duration::from_secs(5 * 60);
pub const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The `kv` key of the [`Marks`].
const MARKS_KEY: &str = "orchestrator_stall_marks";

/// What archond did last to a silent orchestrator, kept in the store so
/// that a restart forgets neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    /// The orchestrator they are about: another agent starts without any.
    pub agent: AetherAgentId,
    pub nudged_at: Option<Timestamp>,
    /// The last interrupt tried, whatever came of it.
    pub interrupted_at: Option<Timestamp>,
}

impl Marks {
    pub fn none(agent: &AetherAgentId) -> Self {
        Self {
            agent: agent.clone(),
            nudged_at: None,
            interrupted_at: None,
        }
    }
}

/// A silence that something is due for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silence {
    /// The turn's last progress.
    pub since: Timestamp,
    pub length: Duration,
    /// Operator mail waits on it; else it is a stall of [`STALL_AFTER`].
    pub mail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    Nothing,
    Nudge(Silence),
    Interrupt(Silence),
}

fn since(earlier: Timestamp, now: Timestamp) -> Duration {
    (now - earlier).to_std().unwrap_or(Duration::ZERO)
}

fn after(at: Timestamp, span: Duration) -> Timestamp {
    at + chrono::TimeDelta::from_std(span).expect("watchdog bounds are minutes long")
}

/// How long `call` may run while operator mail waits: its declared timeout,
/// capped as Aether caps it, plus [`PAST_TIMEOUT`].
pub fn call_bound(call: &OpenCall) -> Duration {
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

/// Whether unread operator mail makes `turn`'s silence due at `now`: both are
/// [`MAIL_SILENCE`] old, and every open call is past its own bound.
fn mail_is_due(turn: &OpenTurn, now: Timestamp) -> bool {
    let Some(delivered) = turn.oldest_unread_mail else {
        return false;
    };
    if since(delivered, now) < MAIL_SILENCE {
        return false;
    }
    let quiet = after(turn.last_progress, MAIL_SILENCE);
    let calls_over = turn
        .open_calls
        .iter()
        .map(|call| after(call.requested_at, call_bound(call)))
        .max();
    now >= calls_over.map_or(quiet, |over| over.max(quiet))
}

/// The whole rule: what is due for the running `turn` at `now`. A nudge counts
/// only for the silence it was sent in, so every interrupt follows its own.
pub fn due(turn: &OpenTurn, marks: &Marks, now: Timestamp) -> Due {
    let length = since(turn.last_progress, now);
    let mail = mail_is_due(turn, now);
    if !mail && length < STALL_AFTER {
        return Due::Nothing;
    }
    assert!(
        length >= MAIL_SILENCE,
        "only a silence of minutes is acted on"
    );
    let silence = Silence {
        since: turn.last_progress,
        length,
        mail,
    };
    let Some(nudged) = marks.nudged_at.filter(|at| *at >= turn.last_progress) else {
        return Due::Nudge(silence);
    };
    let spaced = marks
        .interrupted_at
        .is_none_or(|at| since(at, now) >= INTERRUPT_SPACING);
    if since(nudged, now) < NUDGE_GRACE || !spaced {
        return Due::Nothing;
    }
    Due::Interrupt(silence)
}

/// Checks every [`STALL_CHECK_INTERVAL`]. Only a store failure ends it.
pub async fn watch_stalls(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::time::sleep(STALL_CHECK_INTERVAL).await;
        if let Err(error) = report_stall(&ctx, archon_core::now()).await {
            if error.is_store() {
                return Err(error);
            }
            tracing::warn!(%error, "checking the orchestrator for a stalled turn failed");
        }
    }
}

/// Apply the rule to the orchestrator as of `now`: nudge its silent turn, or
/// interrupt one already nudged. Returns whether it did either.
pub async fn report_stall(ctx: &DaemonContext, now: Timestamp) -> Result<bool, ArchonError> {
    let Some(agent) = super::current_orchestrator(ctx).await? else {
        return Ok(false);
    };
    let client = ctx.aether.client()?;
    let still_since = now - chrono::TimeDelta::from_std(MAIL_SILENCE).expect("minutes");
    let Some(turn) = running_turn(&client, &agent, still_since).await? else {
        return Ok(false);
    };
    let marks = load_marks(ctx, &agent).await?;
    let due = due(&turn, &marks, now);
    // A turn Aether parked, as on a spent provider, is not one to recover.
    if due == Due::Nothing || !is_running(&client, &agent).await? {
        return Ok(false);
    }
    match due {
        Due::Nothing => Ok(false),
        Due::Nudge(silence) => nudge(ctx, &client, &silence, marks, now).await,
        Due::Interrupt(silence) => interrupt(ctx, &client, &silence, marks, now).await,
    }
}

async fn is_running(client: &AetherClient, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    Ok(client.get_agent(agent).await?.status["state"] == "running")
}

/// `agent`'s turn, when it is open and has not moved after `still_since`.
/// `None` also for a turn whose open calls are not all known: it is left alone.
async fn running_turn(
    client: &AetherClient,
    agent: &AetherAgentId,
    still_since: Timestamp,
) -> Result<Option<OpenTurn>, ArchonError> {
    match turn::read(client, agent, still_since).await? {
        Turn::Open(turn) => {
            assert!(
                turn.last_progress <= still_since,
                "a turn that moved is no open one"
            );
            Ok(Some(turn))
        }
        Turn::Closed | Turn::Moving => Ok(None),
        Turn::Unknown => {
            tracing::warn!(%agent, "the orchestrator's turn began beyond the events read; not judging it");
            Ok(None)
        }
    }
}

async fn load_marks(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<Marks, ArchonError> {
    let raw = ctx.read_store().await?.kv_get(MARKS_KEY)?;
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(Marks::none(agent));
    };
    let marks: Marks = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {MARKS_KEY}: {e}")))?;
    if marks.agent != *agent {
        return Ok(Marks::none(agent));
    }
    Ok(marks)
}

async fn save_marks(ctx: &DaemonContext, marks: &Marks) -> Result<(), ArchonError> {
    assert!(
        marks.nudged_at.is_some(),
        "marks are saved from the first nudge on"
    );
    let raw = serde_json::to_string(marks)?;
    ctx.store.lock().await.kv_set(MARKS_KEY, &raw)
}

/// Send the orchestrator `text` as a trigger, whose sender is Archon and so
/// never the operator. `key` makes a repeat the same delivery.
async fn tell(
    client: &AetherClient,
    agent: &AetherAgentId,
    text: &str,
    key: &str,
) -> Result<(), ArchonError> {
    assert!(!text.trim().is_empty(), "a note says something");
    assert!(key.contains(agent.as_str()), "a key is one agent's");
    let payload = json!({
        "archon": crate::triggers::PAYLOAD_VERSION,
        "events": [{"type": "watchdog", "text": text}],
        "summary": "Archon's stall watch",
    });
    client.trigger(agent, payload, key).await
}

/// Nudge the silent turn and mark it. A stall with no mail waiting is told
/// to the operator; mail's nudge is not, its interrupt will be.
async fn nudge(
    ctx: &DaemonContext,
    client: &AetherClient,
    silence: &Silence,
    mut marks: Marks,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    assert!(now >= silence.since, "a nudge follows its silence");
    let key = format!(
        "stall-nudge:{}:{}",
        marks.agent,
        silence.since.timestamp_millis()
    );
    tell(client, &marks.agent, &notice::nudge_text(silence), &key).await?;
    marks.nudged_at = Some(now);
    save_marks(ctx, &marks).await?;
    let minutes = silence.length.as_secs() / 60;
    tracing::warn!(agent = %marks.agent, minutes, mail = silence.mail, "orchestrator turn silent; nudged it");
    if !silence.mail {
        let text = notice::stall_text(silence);
        feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
    }
    Ok(true)
}

/// Interrupt the nudged turn, as Esc would, unless a fresh reading shows it moved.
/// The cut-off note goes first, so the fresh turn reads it with the mail.
async fn interrupt(
    ctx: &DaemonContext,
    client: &AetherClient,
    silence: &Silence,
    mut marks: Marks,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let nudged = marks.nudged_at.expect("an interrupt follows its nudge");
    assert!(nudged >= silence.since, "the nudge was of this silence");
    let Some(turn) = running_turn(client, &marks.agent, silence.since).await? else {
        return Ok(false);
    };
    let key = format!(
        "stall-cut-off:{}:{}",
        marks.agent,
        nudged.timestamp_millis()
    );
    let note = notice::cut_off_note(silence, &turn.open_calls);
    tell(client, &marks.agent, &note, &key).await?;
    let outcome = match ctx.backend.interrupt(&marks.agent).await {
        Ok(true) => Interrupted::Stopped,
        Ok(false) => Interrupted::NoTurn,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => Interrupted::Failed(error),
    };
    marks.interrupted_at = Some(now);
    save_marks(ctx, &marks).await?;
    let minutes = silence.length.as_secs() / 60;
    tracing::warn!(agent = %marks.agent, minutes, mail = silence.mail, outcome = outcome.name(), "orchestrator turn hung; interrupt tried");
    let kind = match outcome {
        Interrupted::Stopped => NotificationKind::Decision,
        Interrupted::NoTurn | Interrupted::Failed(_) => NotificationKind::Blocker,
    };
    let text = notice::interrupt_text(silence, &turn.open_calls, &outcome);
    feed::notify(ctx, kind, text, None, None).await?;
    Ok(true)
}

#[cfg(test)]
mod tests;
