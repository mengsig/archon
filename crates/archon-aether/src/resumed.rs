//! What a kept stage agent answered after it was resumed by message
//! (ARCHITECTURE §6.7). Its task settled with its first attempt, so the
//! answer is read from its own events: the delivery of the message, the turn
//! that took it, and the last thing the agent said since.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::resumed_reply_outcome;
use archon_core::run::StageOutcome;

use crate::client::AetherClient;
use crate::events::EventKind;
use crate::wire::{Agent, EventQuery};

/// Aether returns at most this many events for one `get_events`.
const PAGE: usize = 5000;
/// Pages read of one kind of event before the answer counts as unreadable.
const MAX_PAGES: usize = 8;

/// Whether the agent waits for its inbox alone, with no turn running.
pub(crate) fn is_idle(agent: &Agent) -> bool {
    let state = agent.status.get("state").and_then(|s| s.as_str());
    let on = agent
        .status
        .pointer("/condition/on")
        .and_then(|o| o.as_str());
    state == Some("waiting") && on == Some("inbox")
}

/// An agent that can no longer answer fails its attempt as infrastructure,
/// which replaces it with a fresh one.
pub(crate) fn lost(why: &str) -> StageOutcome {
    assert!(!why.trim().is_empty(), "a lost agent says how");
    StageOutcome::Failed {
        error: format!("the agent kept for this stage could not answer: {why}"),
        infra: true,
    }
}

/// The turn that took the message, read forward from its delivery.
#[derive(Debug, Default)]
struct Turn {
    /// A turn rendered the message into its prompt.
    taken: bool,
    /// The last thing the agent said since.
    answer: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    /// The message waits in the inbox for a turn.
    Pending,
    /// A turn took the message and ended without a word.
    Silent,
    Answer(String),
}

impl Turn {
    /// Take one of the agent's events after the delivery of inbox item
    /// `delivered`, oldest first.
    fn step(&mut self, kind: &EventKind, delivered: u64) {
        match kind {
            EventKind::InboxConsumed { up_to } if *up_to >= delivered => self.taken = true,
            EventKind::AgentMessage {
                assistant: true,
                text,
            } if self.taken && !text.trim().is_empty() => {
                self.answer = Some(text.trim().to_owned());
            }
            _ => {}
        }
    }

    fn verdict(self) -> Verdict {
        assert!(self.taken || self.answer.is_none(), "an answer follows");
        match self.answer {
            _ if !self.taken => Verdict::Pending,
            Some(answer) => Verdict::Answer(answer),
            None => Verdict::Silent,
        }
    }
}

/// The agent's own events of `types` after event `after`, oldest first;
/// `None` when they do not end within [`MAX_PAGES`].
async fn own_events(
    client: &AetherClient,
    agent: &AetherAgentId,
    types: &[&str],
    mut after: u64,
) -> Result<Option<Vec<EventKind>>, ArchonError> {
    let mut own = Vec::new();
    for _ in 0..MAX_PAGES {
        let query = EventQuery {
            agent: Some(agent.clone()),
            types: types.iter().map(|t| (*t).to_owned()).collect(),
            ..EventQuery::default()
        };
        let page = client.events_after(after, query, PAGE).await?;
        let full = page.len() == PAGE;
        after = page.last().map_or(after, |last| last.seq);
        // The query follows what the agent delegated; only its own count.
        let mine = page.into_iter().filter(|e| e.agent.as_ref() == Some(agent));
        own.extend(mine.map(|e| (e.seq, e.kind)));
        if !full {
            debug_assert!(own.windows(2).all(|pair| pair[0].0 < pair[1].0));
            return Ok(Some(own.into_iter().map(|(_, kind)| kind).collect()));
        }
    }
    Ok(None)
}

/// Where `message` was put in the agent's inbox: the event that says so and
/// the inbox item it became. The latest delivery counts.
async fn delivery(
    client: &AetherClient,
    agent: &AetherAgentId,
    message: &str,
) -> Result<Option<(u64, u64)>, ArchonError> {
    let query = EventQuery {
        agent: Some(agent.clone()),
        types: vec!["inbox.delivered".to_owned()],
        ..EventQuery::default()
    };
    let mut found = None;
    let mut after = 0;
    for _ in 0..MAX_PAGES {
        let page = client.events_after(after, query.clone(), PAGE).await?;
        for event in page.iter().filter(|e| e.agent.as_ref() == Some(agent)) {
            if let EventKind::InboxDelivered {
                seq,
                message: Some(delivered),
                ..
            } = &event.kind
                && delivered.text.trim() == message.trim()
            {
                found = Some((event.seq, *seq));
            }
        }
        match page.last() {
            Some(last) if page.len() == PAGE => after = last.seq,
            _ => break,
        }
    }
    Ok(found)
}

/// How the turn ended in which the idle `agent` took `message`.
pub(crate) async fn read(
    client: &AetherClient,
    agent: &AetherAgentId,
    message: &str,
) -> Result<Option<StageOutcome>, ArchonError> {
    assert!(!message.trim().is_empty(), "an agent is resumed with text");
    let Some((event, item)) = delivery(client, agent, message).await? else {
        let why = "Aether has no record of the message it was resumed with";
        return Ok(Some(lost(why)));
    };
    let types = ["agent.message", "inbox.consumed"];
    let Some(since) = own_events(client, agent, &types, event).await? else {
        let events = PAGE * MAX_PAGES;
        let why = format!("its answer lies more than {events} events after its resume");
        return Ok(Some(lost(&why)));
    };
    let mut turn = Turn::default();
    for kind in &since {
        turn.step(kind, item);
    }
    Ok(match turn.verdict() {
        Verdict::Pending => None,
        Verdict::Silent => Some(lost("it ended its turn without a final message")),
        Verdict::Answer(answer) => Some(resumed_reply_outcome(&answer)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(assistant: bool, text: &str) -> EventKind {
        EventKind::AgentMessage {
            assistant,
            text: text.into(),
        }
    }

    /// The verdict on the events after the delivery of inbox item 4.
    fn verdict(since: &[EventKind]) -> Verdict {
        let mut turn = Turn::default();
        since.iter().for_each(|kind| turn.step(kind, 4));
        turn.verdict()
    }

    #[test]
    fn the_answer_is_the_last_word_since_a_turn_took_the_message() {
        let since = [
            EventKind::InboxConsumed { up_to: 4 },
            said(false, "Archon stage `implement`, attempt 2: fix it."),
            said(true, "Looking at the finding."),
            said(true, "  Fixed in src/a.rs.  "),
        ];
        assert_eq!(
            verdict(&since),
            Verdict::Answer("Fixed in src/a.rs.".into())
        );
    }

    #[test]
    fn a_message_no_turn_took_yet_is_pending() {
        let since = [
            EventKind::InboxConsumed { up_to: 3 },
            said(true, "Still wrapping up the first attempt."),
        ];
        assert_eq!(verdict(&since), Verdict::Pending);
        assert_eq!(verdict(&[]), Verdict::Pending);
    }

    #[test]
    fn a_turn_that_said_nothing_is_silent() {
        let since = [EventKind::InboxConsumed { up_to: 4 }, said(true, "   ")];
        assert_eq!(verdict(&since), Verdict::Silent);
    }

    #[test]
    fn a_later_note_in_the_same_turn_does_not_hide_the_answer() {
        let since = [
            EventKind::InboxConsumed { up_to: 4 },
            said(true, "On it."),
            EventKind::InboxConsumed { up_to: 5 },
            said(true, "Fixed, and the flag stays."),
        ];
        let answer = Verdict::Answer("Fixed, and the flag stays.".into());
        assert_eq!(verdict(&since), answer);
    }

    #[test]
    fn only_a_wait_for_the_inbox_alone_is_idle() {
        let mut agent: Agent = serde_json::from_value(serde_json::json!({
            "id": "agt_1", "role": "r", "name": "n",
            "model": {"provider": "anthropic", "model": "m"},
            "status": {"state": "waiting", "condition": {"on": "inbox"}},
        }))
        .unwrap();
        assert!(is_idle(&agent));
        for status in [
            serde_json::json!({"state": "running"}),
            serde_json::json!({"state": "waiting", "condition": {"on": "task", "id": "tsk_1"}}),
            serde_json::json!({"state": "completed"}),
        ] {
            agent.status = status;
            assert!(!is_idle(&agent), "{}", agent.status);
        }
    }
}
