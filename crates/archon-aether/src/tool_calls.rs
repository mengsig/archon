//! The tool call an agent's current turn left open, read from its newest
//! events. Archon's stream carries no tool events; the stage watchdog reads
//! them from Aether only for a lead that has gone quiet.

use std::collections::BTreeSet;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::OpenToolCall;
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
    let query = EventQuery {
        agent: Some(agent.clone()),
        types: TURN_EVENT_TYPES.iter().map(|t| (*t).to_owned()).collect(),
        ..EventQuery::default()
    };
    let newest_first = client.latest_events_raw(query, PAGE).await?;
    open_in(agent, &newest_first)
}

/// [`open`] over `agent`'s events, newest first. Walking back from the
/// newest, the first call with no outcome before its turn's start is open.
pub(crate) fn open_in(
    agent: &AetherAgentId,
    newest_first: &[Value],
) -> Result<Option<OpenToolCall>, ArchonError> {
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
        return Ok(None);
    };
    let last_event_at = newest.at;
    let mut settled = BTreeSet::new();
    for event in &own {
        match event.type_name {
            "agent.started" | "agent.interrupted" | "agent.turn_completed" => return Ok(None),
            "tool.requested" => {
                let call = event.field("call");
                let id = text(call.get("id"), "tool.requested call.id")?;
                if settled.contains(id) {
                    continue;
                }
                let found = OpenToolCall {
                    id: id.to_owned(),
                    name: text(call.get("name"), "tool.requested call.name")?.to_owned(),
                    command: command_of(call.get("input")),
                    requested_at: event.at,
                    // Aether's clock may step back; nothing is newer than the call.
                    last_event_at: last_event_at.max(event.at),
                };
                return Ok(Some(found));
            }
            "agent.message" | "agent.usage_updated" => {}
            // Every outcome names its call by id.
            _ => {
                settled.insert(text(event.kind.get("call"), "a tool outcome's call")?);
            }
        }
    }
    Ok(None)
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
