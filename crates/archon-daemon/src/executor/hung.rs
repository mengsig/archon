//! A stage agent stuck in one tool call (2026-11-01: a Bash call never
//! returned for 85 minutes). Its engine stays alive and Aether renews its
//! lease, so the silence watchdog sees a turn in flight. The first hang of a
//! stage run interrupts the turn and wakes the same agent with a note; a hang
//! after that stops the stage as an infrastructure failure, which
//! `limits.infra_retries` retries. Neither counts against the model.

use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{AgentMessage, MessageKind, MessageSender};
use archon_core::config::LimitsConfig;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::OpenToolCall;
use archon_core::run::{StageRun, StageState};

use super::{resume, stall};
use crate::context::DaemonContext;

/// The key of a stage run whose agent was interrupted for a hung call,
/// holding when.
pub(super) const KEY_PREFIX: &str = "hung_tool:";

/// Commands known to run long in the foreground, matched in lower case
/// with runs of whitespace folded.
const LONG_CHECKS: &[&str] = &[
    "make ci",
    "make check",
    "make test",
    "make e2e",
    "make bench",
    "make release",
    "build-release",
    "package-release",
    "cargo test",
    "cargo nextest",
    "cargo build",
    "cargo clippy",
    "npm test",
    "npm run test",
    "pnpm test",
    "yarn test",
    "pytest",
    "go test",
    "docker compose",
    "docker build",
    "gradle",
    "mvn ",
];

/// The longest command a note or message quotes.
const QUOTED_CHARS: usize = 120;

/// How long an open tool call may go with no event of its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub tool: Duration,
    /// For a known long check; never shorter than `tool`.
    pub check: Duration,
}

impl Limits {
    /// `None` when the check is off.
    pub fn of(limits: &LimitsConfig) -> Option<Self> {
        if limits.hung_tool_minutes == 0 {
            return None;
        }
        let minutes = |m: u32| Duration::from_secs(u64::from(m) * 60);
        let tool = minutes(limits.hung_tool_minutes);
        let check = minutes(limits.hung_check_minutes).max(tool);
        Some(Self { tool, check })
    }

    fn of_call(&self, call: &OpenToolCall) -> Duration {
        match call.command.as_deref().is_some_and(is_long_check) {
            true => self.check,
            false => self.tool,
        }
    }
}

/// Whether `command` runs a check known to take long in the foreground.
pub fn is_long_check(command: &str) -> bool {
    let folded = command
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    LONG_CHECKS.iter().any(|check| folded.contains(check))
}

/// Aether's own tools (`wait_for`, `delegate` with `wait`, `ask_user`) wait
/// on other agents or a person by design; the tree's events cover them.
fn waits_by_design(name: &str) -> bool {
    name.contains("aether")
}

/// An open tool call that went quiet past its limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hung {
    pub call: OpenToolCall,
    /// Since the agent's newest event.
    pub silent: Duration,
    pub limit: Duration,
}

/// `Some` when `call` has gone `limits`' time at `now` with no output,
/// outcome or other event of its agent. Aether's clock may step, so the
/// newer of its two times counts.
pub fn judge(limits: &Limits, call: &OpenToolCall, now: Timestamp) -> Option<Hung> {
    assert!(!call.name.is_empty(), "a call names its tool");
    assert!(limits.tool <= limits.check, "a long check never gets less");
    if waits_by_design(&call.name) {
        return None;
    }
    let quiet_since = call.last_event_at.max(call.requested_at);
    let silent = (now - quiet_since).to_std().unwrap_or(Duration::ZERO);
    let limit = limits.of_call(call);
    (silent >= limit).then(|| Hung {
        call: call.clone(),
        silent,
        limit,
    })
}

/// What the watchdog does about a hang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Interrupt,
    /// The agent was interrupted less than the hang's limit ago.
    Wait,
    Stop,
}

/// The step for `hung` given when this stage run's agent was interrupted
/// for an earlier hang: one interrupt per stage run, and a stop only once
/// the interrupted agent has had the hang's limit to recover. A mark from
/// a clock now behind counts as just made.
pub fn step(interrupted_at: Option<Timestamp>, hung: &Hung, now: Timestamp) -> Step {
    assert!(!hung.limit.is_zero(), "a check that is on has a limit");
    assert!(hung.silent >= hung.limit, "only a hang past its limit");
    let Some(at) = interrupted_at else {
        return Step::Interrupt;
    };
    let since = (now - at).to_std().unwrap_or(Duration::ZERO);
    match since >= hung.limit {
        true => Step::Stop,
        false => Step::Wait,
    }
}

/// Act on `hung`, the lead of `stage` stuck in a tool call; `Some(reason)` to
/// stop the stage with.
pub(super) async fn act(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    hung: &Hung,
    now: Timestamp,
) -> Result<Option<String>, ArchonError> {
    assert_eq!(stage.state, StageState::Running, "only a running stage");
    assert_eq!(stage.aether_agent.as_ref(), Some(lead), "the stage's lead");
    let marked = ctx.store.lock().await.kv_get(&key(&stage.id))?;
    let mark = marked.as_deref().map(Mark::parse).transpose()?;
    let minutes = hung.silent.as_secs() / 60;
    let call = describe(&hung.call);
    match step(mark.map(|m| m.interrupted_at), hung, now) {
        Step::Wait => Ok(None),
        Step::Interrupt => interrupt(ctx, stage, lead, hung, now).await,
        // A stop whose cancel went unanswered is retried without a new message.
        Step::Stop if mark.is_some_and(|m| m.stopped) => {
            Ok(Some(stall::hung_again(&call, minutes)))
        }
        Step::Stop => {
            tracing::warn!(stage = %stage.id, %lead, %call, minutes, "tool call hung again; stopping the stage");
            let text = format!(
                "Stage {} ({}): agent {lead}'s tool call {call} hung again for {minutes} min after \
                 Archon interrupted an earlier hang; the stage is stopped and retried as an \
                 infrastructure failure.",
                stage.stage_id, stage.id
            );
            let interrupted_at = mark.expect("a stop follows an interrupt").interrupted_at;
            let stopped = Mark {
                interrupted_at,
                stopped: true,
            };
            tell(ctx, stage, lead, text, Some(&stopped.render())).await?;
            Ok(Some(stall::hung_again(&call, minutes)))
        }
    }
}

/// What the stage run's kv mark holds: when its agent was interrupted, and
/// whether the stage was then stopped for a second hang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    interrupted_at: Timestamp,
    stopped: bool,
}

impl Mark {
    const STOPPED: &str = " stopped";

    fn parse(raw: &str) -> Result<Self, ArchonError> {
        let (at, stopped) = match raw.strip_suffix(Self::STOPPED) {
            Some(at) => (at, true),
            None => (raw, false),
        };
        let interrupted_at = at
            .parse::<Timestamp>()
            .map_err(|e| ArchonError::Corrupt(format!("hung-call mark {raw:?}: {e}")))?;
        Ok(Self {
            interrupted_at,
            stopped,
        })
    }

    fn render(&self) -> String {
        let at = self.interrupted_at.to_rfc3339();
        match self.stopped {
            true => format!("{at}{}", Self::STOPPED),
            false => at,
        }
    }
}

/// Interrupt the turn stuck in `hung` and wake its agent with a note, then
/// tell the orchestrator. An unanswered interrupt is tried by the next sweep;
/// a note that cannot be sent leaves an idle agent, so the stage stops.
async fn interrupt(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    hung: &Hung,
    now: Timestamp,
) -> Result<Option<String>, ArchonError> {
    let minutes = hung.silent.as_secs() / 60;
    let call = describe(&hung.call);
    match ctx.backend.interrupt(lead).await {
        Ok(was_running) => {
            tracing::warn!(stage = %stage.id, %lead, %call, minutes, was_running, "tool call hung; turn interrupted");
        }
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(stage = %stage.id, %lead, %error, "hung-call interrupt unanswered; tried again next sweep");
            return Ok(None);
        }
        Err(error) if error.is_store() => return Err(error),
        // A second hang still stops the stage once the limit has passed.
        Err(refusal) => {
            tracing::warn!(stage = %stage.id, %lead, %refusal, "Aether refused to interrupt a hung call");
        }
    }
    let note = format!(
        "Your tool call {call} hung for {minutes} min and was interrupted; continue your task. \
         Check the state it would have changed before you repeat it, and give a command that \
         may hang a timeout or run it in the background."
    );
    let note_key = format!("archon:hung:{}:{}", stage.id, hung.call.id);
    if let Err(error) = ctx.backend.send_message(lead, &note, &note_key).await {
        if error.is_store() {
            return Err(error);
        }
        tracing::warn!(stage = %stage.id, %lead, %error, "could not wake an interrupted agent");
        return Ok(Some(stall::hung_unwoken(&call, &error.to_string())));
    }
    let text = format!(
        "Stage {} ({}): agent {lead}'s tool call {call} hung for {minutes} min with no output; \
         Archon interrupted the turn and woke the agent to continue. Another hang stops the \
         stage as an infrastructure failure.",
        stage.stage_id, stage.id
    );
    let mark = Mark {
        interrupted_at: now,
        stopped: false,
    };
    tell(ctx, stage, lead, text, Some(&mark.render())).await?;
    Ok(None)
}

/// Post `text` as a stage `info` message to the orchestrator, writing the
/// stage run's mark with it when given.
async fn tell(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    text: String,
    mark: Option<&str>,
) -> Result<(), ArchonError> {
    assert!(!text.trim().is_empty(), "a message says something");
    let mark_key = key(&stage.id);
    let updates: Vec<(&str, &str)> = mark
        .map(|value| (mark_key.as_str(), value))
        .into_iter()
        .collect();
    let mut store = ctx.store.lock().await;
    let item = store.run(&stage.run)?.item;
    let message = AgentMessage {
        item,
        run: stage.run.clone(),
        stage: Some(stage.stage_id.clone()),
        stage_run: Some(stage.id.clone()),
        attempt: Some(stage.attempt),
        agent: Some(lead.clone()),
        kind: MessageKind::Info,
        text,
        original_sender: MessageSender::System,
        addressed_to_run_node: None,
    };
    let saved = crate::triggers::record_message(&mut store, message, &updates)?;
    drop(store);
    crate::feed::publish(ctx, vec![saved]);
    crate::triggers::wake(ctx);
    Ok(())
}

pub(super) fn key(stage: &StageRunId) -> String {
    format!("{KEY_PREFIX}{stage}")
}

/// `call` as a note quotes it: its tool and, when it runs one, its command.
fn describe(call: &OpenToolCall) -> String {
    let Some(command) = &call.command else {
        return call.name.clone();
    };
    let mut quoted: String = command.chars().take(QUOTED_CHARS).collect();
    if quoted.len() < command.len() {
        quoted.push('…');
    }
    format!("{} (`{}`)", call.name, quoted.replace('`', "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        "2026-11-01T12:30:00Z".parse::<Timestamp>().unwrap() + chrono::Duration::minutes(minutes)
    }

    fn limits() -> Limits {
        Limits::of(&LimitsConfig::default()).expect("on by default")
    }

    fn call(command: &str) -> OpenToolCall {
        OpenToolCall {
            id: "call_01".into(),
            name: "Bash".into(),
            command: Some(command.into()),
            requested_at: at(0),
            last_event_at: at(0),
        }
    }

    const STUCK: &str = "git status --short; git rev-parse HEAD; kill 1227080; ps -p 1227080";

    #[test]
    fn a_hung_call_is_seen_after_the_tool_limit_and_not_before() {
        let stuck = call(STUCK);
        assert_eq!(judge(&limits(), &stuck, at(19)), None);
        let hung = judge(&limits(), &stuck, at(20)).expect("hung at 20 min");
        assert_eq!(hung.limit, Duration::from_secs(20 * 60));
        assert_eq!(hung.silent, Duration::from_secs(20 * 60));
        let later_output = OpenToolCall {
            last_event_at: at(10),
            ..stuck
        };
        assert_eq!(
            judge(&limits(), &later_output, at(29)),
            None,
            "an event of the agent since restarts the clock"
        );
    }

    #[test]
    fn a_long_foreground_check_gets_the_longer_limit() {
        for command in [
            "make ci",
            "CARGO_TARGET_DIR=x cargo  test -p archon",
            "uv run pytest -q",
        ] {
            assert!(is_long_check(command), "{command}");
            let check = call(command);
            assert_eq!(judge(&limits(), &check, at(59)), None, "{command}");
            assert!(judge(&limits(), &check, at(60)).is_some(), "{command}");
        }
        assert!(!is_long_check(STUCK));
    }

    #[test]
    fn a_wait_on_other_agents_is_never_hung() {
        let wait = OpenToolCall {
            name: "mcp__aether__wait_for".into(),
            command: None,
            ..call("")
        };
        assert_eq!(judge(&limits(), &wait, at(600)), None);
    }

    #[test]
    fn one_interrupt_then_a_stop_and_never_a_loop() {
        let hung = judge(&limits(), &call(STUCK), at(20)).unwrap();
        assert_eq!(step(None, &hung, at(20)), Step::Interrupt);
        let again = judge(&limits(), &call(STUCK), at(25)).unwrap();
        assert_eq!(
            step(Some(at(20)), &again, at(25)),
            Step::Wait,
            "the interrupted agent gets the limit to recover"
        );
        assert_eq!(step(Some(at(20)), &again, at(40)), Step::Stop);
        assert_eq!(
            step(Some(at(20)), &again, at(400)),
            Step::Stop,
            "an interrupted stage run is never interrupted again"
        );
    }

    #[test]
    fn a_mark_round_trips_and_a_clock_step_neither_panics_nor_stops_early() {
        for stopped in [false, true] {
            let mark = Mark {
                interrupted_at: at(20),
                stopped,
            };
            assert_eq!(Mark::parse(&mark.render()).unwrap(), mark);
        }
        assert!(Mark::parse("not a time").is_err());
        let hung = judge(&limits(), &call(STUCK), at(20)).unwrap();
        assert_eq!(
            step(Some(at(90)), &hung, at(20)),
            Step::Wait,
            "a mark from a clock now behind counts as just made"
        );
        let stepped = OpenToolCall {
            requested_at: at(10),
            ..call(STUCK)
        };
        assert_eq!(
            judge(&limits(), &stepped, at(29)),
            None,
            "the newer time counts"
        );
    }

    #[test]
    fn zero_turns_the_check_off_and_a_check_never_gets_less_than_a_tool() {
        let off = LimitsConfig {
            hung_tool_minutes: 0,
            ..LimitsConfig::default()
        };
        assert_eq!(Limits::of(&off), None);
        let inverted = LimitsConfig {
            hung_tool_minutes: 30,
            hung_check_minutes: 10,
            ..LimitsConfig::default()
        };
        let limits = Limits::of(&inverted).unwrap();
        assert_eq!(limits.check, limits.tool);
    }

    #[test]
    fn a_long_command_is_quoted_short() {
        let long = call(&"x".repeat(500));
        let quoted = describe(&long);
        assert!(quoted.chars().count() < 140, "{quoted}");
        assert_eq!(describe(&call("ls `pwd`")), "Bash (`ls 'pwd'`)");
    }
}
