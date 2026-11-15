//! The tool call an agent's current turn left open, read from its newest
//! events. Archon's stream carries no tool events; the stage watchdog reads
//! them from Aether only for a lead that has gone quiet.

use std::collections::BTreeSet;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{OpenToolCall, TurnActivity};
use serde_json::Value;

use crate::client::AetherClient;
use crate::wire::EventQuery;

/// The event types that show a turn moving: its tool calls, their outcomes,
/// its messages and usage, and where it starts and ends.
const TURN_EVENT_TYPES: &[&str] = &[
    "tool.requested",
    "tool.succeeded",
    "tool.failed",
    "tool.cancelled",
    "tool.denied",
    "tool.replayed",
    "agent.message",
    "agent.usage_updated",
    "agent.started",
    "agent.interrupted",
    "agent.turn_completed",
];

/// How many of the newest events are read. The query also matches the
/// agent's children; a page they fill means the tree is not quiet anyway.
const PAGE: usize = 200;

/// The tool call `agent`'s current turn left open; `None` when there is none
/// or its turn's start is not within the newest page.
pub(crate) async fn open(
    client: &AetherClient,
    agent: &AetherAgentId,
) -> Result<Option<OpenToolCall>, ArchonError> {
    Ok(activity(client, agent).await?.open_calls.into_iter().next())
}

/// `agent`'s newest turn event and the call its turn left open, from one
/// page of its events.
pub(crate) async fn activity(
    client: &AetherClient,
    agent: &AetherAgentId,
) -> Result<TurnActivity, ArchonError> {
    let query = EventQuery {
        agent: Some(agent.clone()),
        types: TURN_EVENT_TYPES.iter().map(|t| (*t).to_owned()).collect(),
        ..EventQuery::default()
    };
    let newest_first = client.latest_events_raw(query, PAGE).await?;
    activity_in(agent, &newest_first)
}

/// [`open`] over `agent`'s events, newest first.
#[cfg(test)]
fn open_in(
    agent: &AetherAgentId,
    newest_first: &[Value],
) -> Result<Option<OpenToolCall>, ArchonError> {
    Ok(activity_in(agent, newest_first)?
        .open_calls
        .into_iter()
        .next())
}

/// [`activity`] over `agent`'s events, newest first. Walking back from the
/// newest to its turn's start, every call with no outcome is open.
fn activity_in(agent: &AetherAgentId, newest_first: &[Value]) -> Result<TurnActivity, ArchonError> {
    let own: Vec<Event<'_>> = newest_first
        .iter()
        .map(Event::of)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|e| e.agent == Some(agent.as_str()))
        .collect();
    if own.windows(2).any(|pair| pair[0].seq <= pair[1].seq) {
        return Err(ArchonError::Protocol(
            "`events` before the latest did not come newest first".into(),
        ));
    }
    let Some(newest) = own.first() else {
        return Ok(TurnActivity::default());
    };
    let last_event_at = newest.at;
    let mut open_calls = Vec::new();
    let mut settled = BTreeSet::new();
    for event in &own {
        match event.type_name {
            "agent.started" | "agent.interrupted" | "agent.turn_completed" => {
                return Ok(TurnActivity {
                    last_event_at: Some(last_event_at),
                    open_calls,
                    call_known: true,
                });
            }
            "tool.requested" => {
                let call = event.field("call");
                let id = text(call.get("id"), "tool.requested call.id")?;
                if settled.contains(id) {
                    continue;
                }
                open_calls.push(OpenToolCall {
                    id: id.to_owned(),
                    name: text(call.get("name"), "tool.requested call.name")?.to_owned(),
                    command: command_of(call.get("input")),
                    timeout: foreground_timeout(call.get("input")),
                    requested_at: event.at,
                    // Aether's clock may step back; nothing is newer than the call.
                    last_event_at: last_event_at.max(event.at),
                });
            }
            "agent.message" | "agent.usage_updated" => {}
            // Every outcome names its call by id.
            _ => {
                settled.insert(text(event.kind.get("call"), "a tool outcome's call")?);
            }
        }
    }
    // The turn's start is beyond the page: more calls may be open before it.
    assert!(
        open_calls
            .windows(2)
            .all(|pair| pair[0].requested_at >= pair[1].requested_at),
        "open calls are listed newest first"
    );
    Ok(TurnActivity {
        last_event_at: Some(last_event_at),
        open_calls,
        call_known: false,
    })
}

/// The shell command a call's input names: Claude's `command`, or Codex's
/// `command` list or `cmd`.
fn command_of(input: Option<&Value>) -> Option<String> {
    let input = input?;
    let raw = input.get("command").or_else(|| input.get("cmd"))?;
    let command = match raw {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    (!command.trim().is_empty()).then_some(command)
}

/// The timeout a foreground call declares: Claude's Bash `timeout`, in
/// milliseconds. A background call's `timeout` bounds its job, not the call.
fn foreground_timeout(input: Option<&Value>) -> Option<Duration> {
    let input = input?;
    if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let millis = input.get("timeout").and_then(Value::as_u64)?;
    (millis > 0).then(|| Duration::from_millis(millis))
}

fn text<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a str, ArchonError> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ArchonError::Protocol(format!("{what} is not a non-empty string")))
}

/// The parts of a raw event the walk reads.
struct Event<'a> {
    seq: u64,
    at: Timestamp,
    agent: Option<&'a str>,
    type_name: &'a str,
    kind: &'a Value,
}

impl<'a> Event<'a> {
    fn of(raw: &'a Value) -> Result<Self, ArchonError> {
        let bad = |what: &str| ArchonError::Protocol(format!("an event's {what} did not decode"));
        let seq = raw
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("seq"))?;
        let at = raw
            .get("at")
            .and_then(Value::as_str)
            .and_then(|at| at.parse::<Timestamp>().ok())
            .ok_or_else(|| bad("at"))?;
        let kind = raw.get("kind").ok_or_else(|| bad("kind"))?;
        let type_name = kind
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("type"))?;
        Ok(Self {
            seq,
            at,
            agent: raw.get("agent_id").and_then(Value::as_str),
            type_name,
            kind,
        })
    }

    fn field(&self, name: &str) -> &'a Value {
        self.kind.get(name).unwrap_or(&Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LEAD: &str = "agt_01M6BFQZRXYJK8CKVK10H85F7C";
    const CHILD: &str = "agt_01M6BFQZRXYJK8CKVK10H85F7D";

    fn lead() -> AetherAgentId {
        AetherAgentId::parse(LEAD).unwrap()
    }

    fn event(seq: u64, minute: u32, agent: &str, kind: Value) -> Value {
        json!({"seq": seq, "at": format!("2026-11-01T12:{minute:02}:00Z"),
               "agent_id": agent, "kind": kind})
    }

    fn requested(seq: u64, minute: u32, id: &str, command: &str) -> Value {
        let call = json!({"id": id, "name": "Bash", "input": {"command": command}});
        event(
            seq,
            minute,
            LEAD,
            json!({"type": "tool.requested", "call": call}),
        )
    }

    fn succeeded(seq: u64, minute: u32, id: &str) -> Value {
        event(
            seq,
            minute,
            LEAD,
            json!({"type": "tool.succeeded", "call": id}),
        )
    }

    fn started(seq: u64, minute: u32) -> Value {
        event(
            seq,
            minute,
            LEAD,
            json!({"type": "agent.started", "turn": 3}),
        )
    }

    #[test]
    fn the_newest_call_without_an_outcome_in_this_turn_is_open() {
        let events = [
            requested(6, 30, "call_b", "git status; kill 1227080"),
            succeeded(5, 29, "call_a"),
            requested(4, 28, "call_a", "ls"),
            started(3, 27),
        ];
        let open = open_in(&lead(), &events).unwrap().expect("call_b is open");
        assert_eq!(open.id, "call_b");
        assert_eq!(open.name, "Bash");
        assert_eq!(open.command.as_deref(), Some("git status; kill 1227080"));
        assert_eq!(open.requested_at, open.last_event_at);
    }

    #[test]
    fn a_call_settled_after_a_parallel_one_leaves_that_one_open() {
        let events = [
            succeeded(7, 35, "call_b"),
            requested(6, 31, "call_b", "ls"),
            requested(5, 30, "call_a", "make ci"),
            started(4, 29),
        ];
        let open = open_in(&lead(), &events).unwrap().expect("call_a is open");
        assert_eq!(open.id, "call_a");
        assert_eq!(open.command.as_deref(), Some("make ci"));
        assert!(open.last_event_at > open.requested_at, "call_b came since");
    }

    #[test]
    fn no_call_is_open_once_it_settled_or_its_turn_ended() {
        let settled = [succeeded(5, 31, "call_a"), requested(4, 30, "call_a", "ls")];
        assert_eq!(open_in(&lead(), &settled).unwrap(), None);
        let interrupted = event(6, 50, LEAD, json!({"type": "agent.interrupted", "turn": 3}));
        let ended = [
            interrupted,
            requested(5, 30, "call_a", "ls"),
            started(4, 29),
        ];
        assert_eq!(
            open_in(&lead(), &ended).unwrap(),
            None,
            "an interrupted turn left nothing open"
        );
    }

    #[test]
    fn a_foreground_bash_timeout_is_read_and_a_background_one_is_not() {
        let open_with = |input: Value| {
            let call = json!({"id": "call_t", "name": "Bash", "input": input});
            let events = [event(
                2,
                30,
                LEAD,
                json!({"type": "tool.requested", "call": call}),
            )];
            open_in(&lead(), &events).unwrap().unwrap().timeout
        };
        assert_eq!(
            open_with(json!({"command": "make ci", "timeout": 600_000})),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            open_with(json!({"command": "ls"})),
            None,
            "Claude's default is not declared"
        );
        assert_eq!(
            open_with(json!({"command": "make ci", "timeout": 600_000, "run_in_background": true})),
            None,
            "a background job's timeout is not the call's"
        );
        assert_eq!(open_with(json!({"command": "ls", "timeout": "soon"})), None);
    }

    #[test]
    fn the_newest_event_is_reported_and_a_call_beyond_the_page_is_unknown() {
        let started = event(3, 29, LEAD, json!({"type": "agent.started", "turn": 2}));
        let events = [
            succeeded(5, 31, "call_a"),
            requested(4, 30, "call_a", "ls"),
            started,
        ];
        let activity = activity_in(&lead(), &events).unwrap();
        assert!(activity.open_calls.is_empty());
        assert!(activity.call_known, "the turn's start was read");
        let newest = activity.last_event_at.expect("a turn event was read");
        assert_eq!(newest.to_rfc3339(), "2026-11-01T12:31:00+00:00");
        let cut = activity_in(&lead(), &events[..2]).unwrap();
        assert!(!cut.call_known, "the open call may precede the page");
        assert_eq!(cut.last_event_at, activity.last_event_at);
        assert!(!activity_in(&lead(), &[]).unwrap().call_known);
    }

    #[test]
    fn every_call_open_in_the_turn_is_listed() {
        let build = json!({"id": "call_b", "name": "Bash",
                           "input": {"command": "make", "timeout": 600_000}});
        let events = [
            requested(5, 25, "call_c", "ls"),
            succeeded(4, 24, "call_a"),
            event(
                3,
                22,
                LEAD,
                json!({"type": "tool.requested", "call": build}),
            ),
            requested(2, 21, "call_a", "pwd"),
            event(1, 20, LEAD, json!({"type": "agent.started", "turn": 2})),
        ];
        let activity = activity_in(&lead(), &events).unwrap();
        let ids: Vec<&str> = activity.open_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["call_c", "call_b"], "the older build still runs");
        assert!(activity.call_known);
        assert_eq!(
            activity.open_calls[1].timeout,
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn a_childs_call_is_never_the_leads() {
        let child_call = json!({"id": "call_c", "name": "Bash", "input": {"command": "sleep 9"}});
        let events = [
            event(
                6,
                40,
                CHILD,
                json!({"type": "tool.requested", "call": child_call}),
            ),
            succeeded(5, 31, "call_a"),
            requested(4, 30, "call_a", "ls"),
        ];
        assert_eq!(open_in(&lead(), &events).unwrap(), None);
    }

    #[test]
    fn codex_commands_are_read_and_an_ascending_page_is_refused() {
        let call = json!({"id": "call_x", "name": "shell",
                          "input": {"command": ["bash", "-lc", "cargo test"]}});
        let events = [event(
            2,
            30,
            LEAD,
            json!({"type": "tool.requested", "call": call}),
        )];
        let open = open_in(&lead(), &events).unwrap().unwrap();
        assert_eq!(open.command.as_deref(), Some("bash -lc cargo test"));
        let ascending = [started(3, 27), requested(4, 28, "call_a", "ls")];
        assert!(open_in(&lead(), &ascending).is_err());
    }
}
