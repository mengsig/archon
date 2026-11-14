//! The operator feed (ARCHITECTURE §12): archond's record of what it told
//! the operator, and the outbox `posts` copies into the orchestrator's
//! transcript. `append` is the one way anything reaches the operator.

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
    crate::push::question(ctx, &question).await;
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
    crate::push::question(ctx, &question).await;
    Ok(Some(question))
}

/// `kv`: the last orchestrator event applied, so a replay (after a restart
/// or reconnect) applies nothing twice. The key is the one the old chat reply
/// tracker used, whose saved state carries the same `seen`.
pub const SEEN_KEY: &str = "chat_reply_state";

/// The replay guard over the orchestrator's ordered event stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenEvents {
    /// Seq of the last Aether event applied.
    seen: u64,
}

impl SeenEvents {
    /// `true` the first time event `seq` is seen; replays return `false`.
    pub fn first_sight(&mut self, seq: u64) -> bool {
        assert!(seq > 0, "event seqs start at 1");
        if seq <= self.seen {
            return false;
        }
        self.seen = seq;
        true
    }
}

/// The saved guard; a fresh one when none was saved.
pub fn restore_seen(raw: Option<&str>) -> Result<SeenEvents, ArchonError> {
    match raw.filter(|raw| !raw.is_empty()) {
        None => Ok(SeenEvents::default()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {SEEN_KEY} = {raw:?}: {e}"))),
    }
}

/// Whether event `seq` is new, persisting the guard when it is.
pub async fn first_sight(ctx: &DaemonContext, seq: u64) -> Result<bool, ArchonError> {
    let mut seen = ctx.seen.lock().await;
    if !seen.first_sight(seq) {
        return Ok(false);
    }
    let raw = serde_json::to_string(&*seen)?;
    ctx.store.lock().await.kv_set(SEEN_KEY, &raw)?;
    assert_eq!(seen.seen, seq, "the guard moved to this event");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replayed_event_is_seen_once() {
        let mut seen = SeenEvents::default();
        assert!(seen.first_sight(10));
        assert!(!seen.first_sight(10));
        assert!(!seen.first_sight(4));
        assert!(seen.first_sight(11));
    }

    #[test]
    fn the_old_reply_tracker_state_restores_its_seen_seq() {
        let saved = r#"{"pending":[4],"answering":true,"text":"Checking.","seen":9}"#;
        let mut seen = restore_seen(Some(saved)).unwrap();
        assert!(
            !seen.first_sight(9),
            "event 9 was applied before the upgrade"
        );
        assert!(seen.first_sight(10));
        assert_eq!(restore_seen(None).unwrap(), SeenEvents::default());
        assert!(matches!(
            restore_seen(Some("{")),
            Err(ArchonError::Corrupt(_))
        ));
    }
}
