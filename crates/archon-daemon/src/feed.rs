//! The operator feed and chat reply detection (ARCHITECTURE §12).
//! `append` is the one way anything reaches the operator.

use archon_core::aether_ids::AetherQuestionRef;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, PipelineRunId, QuestionId, QueueItemId};
use archon_core::notify::{FeedEntry, FeedItem, Notification, NotificationKind, Question};

use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;

/// Persist a feed entry, then broadcast it to live `SubscribeFeed`
/// connections. Persist first: a subscriber that reconnects replays from
/// the store and must never see an item the store does not have.
pub async fn append(ctx: &DaemonContext, entry: FeedEntry) -> Result<FeedItem, ArchonError> {
    let item = ctx
        .store
        .lock()
        .await
        .append_feed(entry, archon_core::now())?;
    // No receivers is normal (nobody has chat open); not an error.
    let _ = ctx.feed.send(item.clone());
    Ok(item)
}

/// Broadcast items the store already persisted as part of a larger write.
pub fn publish(ctx: &DaemonContext, items: Vec<FeedItem>) {
    for item in items {
        // No receivers is normal (nobody has chat open); not an error.
        let _ = ctx.feed.send(item);
    }
}

/// `kv` copy of the [`ReplyTracker`], so a reply still shows when `archond`
/// restarts while the orchestrator is answering.
pub const REPLY_STATE_KEY: &str = "chat_reply_state";

pub async fn notify(
    ctx: &DaemonContext,
    kind: NotificationKind,
    text: impl Into<String>,
    item: Option<QueueItemId>,
    run: Option<PipelineRunId>,
) -> Result<FeedItem, ArchonError> {
    let text = text.into();
    assert!(!text.trim().is_empty(), "a notification says something");
    let at = archon_core::now();
    let notification = Notification {
        id: NotificationId::new(),
        kind,
        text,
        item,
        run,
        at,
    };
    append(ctx, FeedEntry::Notification(notification)).await
}

/// Daemon lifecycle notes: reconnects, orchestrator restarts, degraded plugins.
pub async fn system(ctx: &DaemonContext, text: impl Into<String>) -> Result<FeedItem, ArchonError> {
    let text = text.into();
    assert!(!text.trim().is_empty(), "a system note says something");
    append(ctx, FeedEntry::System { text }).await
}

/// An Archon-side question (no Aether `ask_user` behind it): its answer
/// reaches the orchestrator as an `operator_answer` trigger.
pub async fn question(
    ctx: &DaemonContext,
    text: String,
    options: Vec<String>,
    item: Option<QueueItemId>,
) -> Result<Question, ArchonError> {
    assert!(!text.trim().is_empty(), "a question asks something");
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text,
        options,
        item,
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let (question, feed_item) = {
        let mut store = ctx.store.lock().await;
        let question = store.insert_question(&question)?;
        let feed_item =
            store.append_feed(FeedEntry::Question(question.clone()), archon_core::now())?;
        (question, feed_item)
    };
    assert!(question.number > 0, "a recorded question is numbered");
    // No receivers is normal (nobody has chat open); not an error.
    let _ = ctx.feed.send(feed_item);
    Ok(question)
}

/// Record an Aether `ask_user` as a `Question` plus its feed entry. A replayed
/// `question.asked` finds the first record and returns `None`.
pub async fn aether_question(
    ctx: &DaemonContext,
    aether: AetherQuestionRef,
    text: &str,
    item: Option<QueueItemId>,
) -> Result<Option<Question>, ArchonError> {
    assert!(aether.seq > 0, "event seqs start at 1");
    let feed_item = {
        let mut store = ctx.store.lock().await;
        if store.question_by_aether(&aether)?.is_some() {
            return Ok(None);
        }
        let question = Question {
            id: QuestionId::new(),
            number: 0,
            text: text.to_owned(),
            options: Vec::new(),
            item,
            aether: Some(aether),
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        };
        let question = store.insert_question(&question)?;
        store.append_feed(FeedEntry::Question(question), archon_core::now())?
    };
    let FeedEntry::Question(question) = &feed_item.entry else {
        unreachable!("the entry was just built as a question");
    };
    let question = question.clone();
    // No receivers is normal (nobody has chat open); not an error.
    let _ = ctx.feed.send(feed_item);
    Ok(Some(question))
}

/// Apply `change` to the chat reply tracker and persist it when it changed.
/// Lock order: replies, then store.
pub async fn track<T>(
    ctx: &DaemonContext,
    change: impl FnOnce(&mut ReplyTracker) -> T,
) -> Result<T, ArchonError> {
    let mut replies = ctx.replies.lock().await;
    let before = replies.clone();
    let result = change(&mut replies);
    if *replies != before {
        let raw = serde_json::to_string(&*replies)?;
        ctx.store.lock().await.kv_set(REPLY_STATE_KEY, &raw)?;
    }
    Ok(result)
}

/// The persisted tracker; a fresh one when none was saved.
pub fn restore_replies(raw: Option<&str>) -> Result<ReplyTracker, ArchonError> {
    match raw.filter(|raw| !raw.is_empty()) {
        None => Ok(ReplyTracker::default()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| ArchonError::Store(format!("kv {REPLY_STATE_KEY} = {raw:?}: {e}"))),
    }
}

/// Decides which orchestrator assistant text becomes an `Orchestrator` feed
/// entry (ARCHITECTURE §12): the last non-empty text of a turn that consumed
/// an operator message, written after it consumed it. Trigger-woken turns,
/// tool calls, thinking and earlier text never show. Pure; driven only by
/// the orchestrator's ordered event stream, so it cannot race `Say` or a
/// lagging pump, and it is persisted so a restart resumes it exactly.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyTracker {
    /// Inbox seqs of operator messages no turn has consumed yet, ascending.
    pending: Vec<u64>,
    /// A consumed operator message awaits this turn's (or, after an
    /// interruption, the revived agent's next turn's) final text.
    answering: bool,
    /// The last non-empty text written since the message was consumed.
    text: Option<String>,
    /// Seq of the last Aether event applied, so a replay changes nothing.
    seen: u64,
}

impl ReplyTracker {
    /// `true` the first time event `seq` is seen; replays return `false`.
    pub fn first_sight(&mut self, seq: u64) -> bool {
        assert!(seq > 0, "event seqs start at 1");
        if seq <= self.seen {
            return false;
        }
        self.seen = seq;
        true
    }

    /// `inbox.delivered`: only a person's message is answered in the chat.
    pub fn delivered(&mut self, seq: u64, from_operator: bool) {
        assert!(seq > 0, "inbox seqs start at 1");
        if from_operator && self.pending.last().is_none_or(|last| *last < seq) {
            self.pending.push(seq);
        }
    }

    /// `inbox.consumed`: the running turn read the inbox up to `up_to`, at
    /// its start or mid-turn. Text written before it read a first operator
    /// message cannot answer it; an answer already begun stays until replaced.
    pub fn consumed(&mut self, up_to: u64) {
        let before = self.pending.len();
        self.pending.retain(|seq| *seq > up_to);
        if self.pending.len() == before {
            return;
        }
        if !self.answering {
            self.answering = true;
            self.text = None;
        }
        assert!(
            self.pending.iter().all(|seq| *seq > up_to),
            "consumed messages are no longer pending"
        );
    }

    /// `agent.message {role: assistant}`. Whitespace-only text (a message
    /// that was all tool calls or thinking) does not replace it.
    pub fn assistant_text(&mut self, text: &str) {
        if self.answering && !text.trim().is_empty() {
            self.text = Some(text.to_owned());
        }
    }

    /// `agent.turn_completed`: the reply to show, if this turn answered the
    /// operator and said something. A turn that ended `failed` or
    /// `cancelled` answered nothing: the revived agent answers instead.
    pub fn turn_completed(&mut self, next: Option<&str>) -> Option<String> {
        if !self.answering {
            return None;
        }
        if matches!(next, Some("failed" | "cancelled")) {
            self.interrupted();
            return None;
        }
        self.answering = false;
        self.text.take()
    }

    /// The orchestrator failed or was cancelled mid-turn and is revived in
    /// place: half an answer is dropped, the message stays awaited.
    pub fn interrupted(&mut self) {
        self.text = None;
    }

    /// Another agent now orchestrates. Inbox seqs are per agent, so pending
    /// ones are forgotten; a consumed message is still answered only by a
    /// successor that kept the conversation, not by a fresh generation.
    pub fn agent_replaced(&mut self, kept_conversation: bool) {
        self.pending.clear();
        self.text = None;
        self.answering &= kept_conversation;
    }

    /// An operator message is waiting for its reply.
    pub fn awaiting_reply(&self) -> bool {
        !self.pending.is_empty() || self.answering
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATOR: bool = true;

    /// A turn that consumes the inbox up to `up_to` at its start.
    fn turn(tracker: &mut ReplyTracker, up_to: u64, texts: &[&str]) -> Option<String> {
        tracker.consumed(up_to);
        for text in texts {
            tracker.assistant_text(text);
        }
        tracker.turn_completed(Some("ready"))
    }

    #[test]
    fn the_turn_that_consumes_an_operator_message_is_shown() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        assert!(tracker.awaiting_reply());
        assert_eq!(
            turn(&mut tracker, 1, &["Queued it."]).as_deref(),
            Some("Queued it.")
        );
        assert!(!tracker.awaiting_reply());
    }

    #[test]
    fn trigger_woken_turns_are_hidden() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, !OPERATOR);
        assert_eq!(turn(&mut tracker, 1, &["item finished; noted"]), None);
        tracker.delivered(2, OPERATOR);
        assert_eq!(
            turn(&mut tracker, 2, &["Answer"]).as_deref(),
            Some("Answer")
        );
        tracker.delivered(3, !OPERATOR);
        assert_eq!(
            turn(&mut tracker, 3, &["another trigger"]),
            None,
            "one reply per message"
        );
    }

    #[test]
    fn only_the_last_text_of_the_turn_is_shown() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        let shown = turn(
            &mut tracker,
            1,
            &["Let me check the queue.", "", "Two items are running."],
        );
        assert_eq!(shown.as_deref(), Some("Two items are running."));
    }

    #[test]
    fn tool_only_messages_do_not_replace_the_last_text() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        let shown = turn(&mut tracker, 1, &["Done: PR #4 is open.", "  \n", ""]);
        assert_eq!(shown.as_deref(), Some("Done: PR #4 is open."));
    }

    #[test]
    fn an_empty_turn_shows_nothing_and_consumes_the_message() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        assert_eq!(turn(&mut tracker, 1, &[]), None);
        assert!(!tracker.awaiting_reply());
        tracker.delivered(2, !OPERATOR);
        assert_eq!(turn(&mut tracker, 2, &["later trigger turn"]), None);
    }

    #[test]
    fn a_message_delivered_mid_turn_but_not_consumed_waits_for_the_next_turn() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, !OPERATOR);
        tracker.consumed(1);
        tracker.assistant_text("working on a trigger");
        tracker.delivered(2, OPERATOR);
        tracker.assistant_text("trigger handled");
        assert_eq!(tracker.turn_completed(Some("ready")), None);
        assert!(tracker.awaiting_reply());
        assert_eq!(
            turn(&mut tracker, 2, &["Here is your answer."]).as_deref(),
            Some("Here is your answer.")
        );
    }

    #[test]
    fn a_trigger_consumed_after_the_message_arrived_does_not_answer_it() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, !OPERATOR);
        tracker.delivered(2, OPERATOR);
        tracker.consumed(1);
        tracker.assistant_text("trigger handled");
        assert_eq!(tracker.turn_completed(Some("ready")), None);
        assert_eq!(
            turn(&mut tracker, 2, &["Answer"]).as_deref(),
            Some("Answer")
        );
    }

    #[test]
    fn messages_consumed_by_different_turns_each_get_their_reply() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        tracker.delivered(2, !OPERATOR);
        tracker.delivered(3, OPERATOR);
        assert_eq!(
            turn(&mut tracker, 2, &["First."]).as_deref(),
            Some("First.")
        );
        assert!(tracker.awaiting_reply(), "the second is still unread");
        assert_eq!(
            turn(&mut tracker, 3, &["Second."]).as_deref(),
            Some("Second.")
        );
        assert!(!tracker.awaiting_reply());
    }

    #[test]
    fn a_message_steered_in_after_the_answer_began_does_not_erase_it() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        tracker.consumed(1);
        tracker.assistant_text("Answer to the first.");
        tracker.delivered(2, OPERATOR);
        tracker.consumed(2);
        assert_eq!(
            tracker.turn_completed(Some("ready")).as_deref(),
            Some("Answer to the first.")
        );
        assert!(!tracker.awaiting_reply());
    }

    #[test]
    fn a_message_steered_into_the_running_turn_is_answered_by_it() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, !OPERATOR);
        tracker.consumed(1);
        tracker.assistant_text("before the message arrived");
        tracker.delivered(2, OPERATOR);
        tracker.consumed(2);
        assert_eq!(
            tracker.turn_completed(Some("ready")),
            None,
            "text before consumption is not an answer"
        );
        assert!(!tracker.awaiting_reply());

        tracker.delivered(3, OPERATOR);
        tracker.consumed(3);
        tracker.assistant_text("Switching to the other repo.");
        assert_eq!(
            tracker.turn_completed(Some("waiting")).as_deref(),
            Some("Switching to the other repo.")
        );
    }

    #[test]
    fn a_second_message_arriving_mid_answer_keeps_the_first_reply() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        tracker.consumed(1);
        tracker.delivered(2, OPERATOR);
        tracker.assistant_text("Answer to the first.");
        assert_eq!(
            tracker.turn_completed(Some("ready")).as_deref(),
            Some("Answer to the first.")
        );
        assert!(tracker.awaiting_reply());
        assert_eq!(
            turn(&mut tracker, 2, &["Answer to the second."]).as_deref(),
            Some("Answer to the second.")
        );
    }

    #[test]
    fn an_interrupted_turn_keeps_the_message_awaited() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        tracker.consumed(1);
        tracker.assistant_text("half an answer");
        assert_eq!(tracker.turn_completed(Some("failed")), None);
        assert!(tracker.awaiting_reply());
        tracker.assistant_text("Full answer");
        assert_eq!(
            tracker.turn_completed(Some("ready")).as_deref(),
            Some("Full answer")
        );
    }

    #[test]
    fn a_fresh_orchestrator_does_not_answer_its_predecessors_message() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(1, OPERATOR);
        tracker.consumed(1);
        tracker.delivered(2, OPERATOR);
        tracker.agent_replaced(false);
        assert!(!tracker.awaiting_reply());
        tracker.delivered(1, !OPERATOR);
        assert_eq!(turn(&mut tracker, 1, &["first trigger"]), None);

        let mut successor = ReplyTracker::default();
        successor.delivered(4, OPERATOR);
        successor.consumed(4);
        successor.agent_replaced(true);
        successor.assistant_text("Answer from the restarted agent");
        assert_eq!(
            successor.turn_completed(Some("ready")).as_deref(),
            Some("Answer from the restarted agent")
        );
    }

    #[test]
    fn a_replayed_event_is_seen_once() {
        let mut tracker = ReplyTracker::default();
        assert!(tracker.first_sight(10));
        assert!(!tracker.first_sight(10));
        assert!(!tracker.first_sight(4));
        assert!(tracker.first_sight(11));
    }

    #[test]
    fn the_tracker_round_trips_through_kv_json() {
        let mut tracker = ReplyTracker::default();
        tracker.delivered(5, OPERATOR);
        tracker.consumed(5);
        tracker.assistant_text("Answer so far");
        let raw = serde_json::to_string(&tracker).unwrap();
        let mut restored = restore_replies(Some(&raw)).unwrap();
        assert_eq!(restored, tracker);
        assert_eq!(
            restored.turn_completed(None).as_deref(),
            Some("Answer so far")
        );
        assert_eq!(restore_replies(None).unwrap(), ReplyTracker::default());
        assert!(matches!(
            restore_replies(Some("{")),
            Err(ArchonError::Store(_))
        ));
    }
}
