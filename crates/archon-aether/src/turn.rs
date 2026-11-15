//! One agent's turn as Aether's events show it: whether it is open, when it
//! last moved, every tool call it has open and the operator mail it has not read.

use std::collections::BTreeSet;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use serde_json::Value;

use crate::client::AetherClient;
use crate::events::{EventKind, parse_event};
use crate::tool_calls::{Event, command_of, text};
use crate::wire::EventQuery;

/// The event types the walk reads. Each but `inbox.delivered` shows the turn
/// moving: mail arriving proves nothing about a turn that does not read it.
const TYPES: &[&str] = &[
    "tool.requested",
    "tool.started",
    "tool.retried",
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
    "agent.failed",
    "agent.cancelled",
    "agent.completed",
    "inbox.consumed",
    "inbox.delivered",
];

/// The first page: enough to see that a turn moved lately.
const PEEK: usize = 20;
/// Every later page. The query also matches the agent's children.
const PAGE: usize = 200;
/// Pages read before a turn whose start is not found is left unknown.
const MAX_PAGES: usize = 50;

/// A tool call with no outcome yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCall {
    pub name: String,
    /// The shell command it runs, when its input names one.
    pub command: Option<String>,
    /// The timeout a foreground call declares (Claude's Bash `timeout`);
    /// `None` when it declares none or runs in the background.
    pub timeout: Option<Duration>,
    pub requested_at: Timestamp,
}

/// A running turn, read back to its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTurn {
    /// Its newest tool, message, usage, inbox-read or turn event.
    pub last_progress: Timestamp,
    /// Every call it has open, newest first: calls may run in parallel.
    pub open_calls: Vec<OpenCall>,
    /// When the oldest operator mail it has not read was delivered.
    pub oldest_unread_mail: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// No turn is running.
    Closed,
    /// The agent moved after the time asked about; no more was read.
    Moving,
    Open(OpenTurn),
    /// Its start lies beyond the pages read, so more calls may be open and
    /// more mail unread than was seen: nothing may be concluded.
    Unknown,
}

/// `agent`'s turn, walking its events back from the newest. A turn that
/// moved after `still_since` is [`Turn::Moving`], read no further.
pub async fn read(
    client: &AetherClient,
    agent: &AetherAgentId,
    still_since: Timestamp,
) -> Result<Turn, ArchonError> {
    let mut walk = Walk::default();
    let mut before = u64::MAX;
    for page in 0..MAX_PAGES {
        let limit = if page == 0 { PEEK } else { PAGE };
        let query = EventQuery {
            agent: Some(agent.clone()),
            types: TYPES.iter().map(|t| (*t).to_owned()).collect(),
            ..EventQuery::default()
        };
        let events = client.events_before_raw(before, query, limit).await?;
        before = walk.take_page(agent, &events, before)?;
        if walk.last_progress.is_some_and(|at| at > still_since) {
            return Ok(Turn::Moving);
        }
        // A short page is the end of history: nothing older is hidden.
        if walk.is_complete() || events.len() < limit {
            return Ok(walk.finish());
        }
    }
    Ok(Turn::Unknown)
}

/// What the walk has seen so far, newest first.
#[derive(Debug, Default)]
struct Walk {
    last_progress: Option<Timestamp>,
    /// Whether the turn is open, once its newest boundary is reached.
    open: Option<bool>,
    calls: Vec<OpenCall>,
    settled: BTreeSet<String>,
    /// The oldest operator mail delivered after the newest `inbox.consumed`.
    unread_mail: Option<Timestamp>,
    /// The newest `inbox.consumed` is reached: older mail was read.
    inbox_read: bool,
}

impl Walk {
    /// Take one page, newest first, all below `before`; returns the seq the
    /// next page ends below.
    fn take_page(
        &mut self,
        agent: &AetherAgentId,
        events: &[Value],
        before: u64,
    ) -> Result<u64, ArchonError> {
        let mut lowest = before;
        for raw in events {
            let event = Event::of(raw)?;
            if event.seq >= lowest {
                return Err(ArchonError::Protocol(
                    "`events` before a seq did not come newest first below it".into(),
                ));
            }
            lowest = event.seq;
            if event.agent == Some(agent.as_str()) {
                self.take(&event, raw)?;
            }
        }
        assert!(lowest <= before, "a walk only moves back");
        Ok(lowest)
    }

    fn take(&mut self, event: &Event<'_>, raw: &Value) -> Result<(), ArchonError> {
        if event.type_name == "inbox.delivered" {
            if !self.inbox_read && is_operator_mail(raw)? {
                self.unread_mail = Some(event.at);
            }
            return Ok(());
        }
        self.last_progress.get_or_insert(event.at);
        if event.type_name == "inbox.consumed" {
            self.inbox_read = true;
            return Ok(());
        }
        if self.open.is_some() {
            return Ok(());
        }
        match event.type_name {
            "agent.started" => self.open = Some(true),
            "agent.interrupted"
            | "agent.turn_completed"
            | "agent.failed"
            | "agent.cancelled"
            | "agent.completed" => self.open = Some(false),
            "tool.requested" => self.requested(event)?,
            "tool.succeeded" | "tool.failed" | "tool.cancelled" | "tool.denied"
            | "tool.replayed" => {
                let call = text(event.kind.get("call"), "a tool outcome's call")?;
                self.settled.insert(call.to_owned());
            }
            // A start, a retry, a message or usage: progress that settles no call.
            _ => {}
        }
        Ok(())
    }

    fn requested(&mut self, event: &Event<'_>) -> Result<(), ArchonError> {
        let call = event.field("call");
        let id = text(call.get("id"), "tool.requested call.id")?;
        if self.settled.contains(id) {
            return Ok(());
        }
        self.calls.push(OpenCall {
            name: text(call.get("name"), "tool.requested call.name")?.to_owned(),
            command: command_of(call.get("input")),
            timeout: foreground_timeout(call.get("input")),
            requested_at: event.at,
        });
        Ok(())
    }

    /// Whether the turn's newest boundary and inbox read are both reached.
    fn is_complete(&self) -> bool {
        self.open.is_some() && self.inbox_read
    }

    /// The turn, once the walk is complete or history has ended.
    fn finish(self) -> Turn {
        let (Some(true), Some(last_progress)) = (self.open, self.last_progress) else {
            return Turn::Closed;
        };
        Turn::Open(OpenTurn {
            last_progress,
            open_calls: self.calls,
            oldest_unread_mail: self.unread_mail,
        })
    }
}

/// Whether the `inbox.delivered` event `raw` is the operator's, by who sent
/// it as Aether recorded them: never by what its text says.
fn is_operator_mail(raw: &Value) -> Result<bool, ArchonError> {
    let event = parse_event(raw.clone())
        .map_err(|error| ArchonError::Protocol(format!("an inbox event: {error}")))?;
    Ok(matches!(
        event.kind,
        EventKind::InboxDelivered {
            from_operator: true,
            ..
        }
    ))
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

#[cfg(test)]
mod tests;
