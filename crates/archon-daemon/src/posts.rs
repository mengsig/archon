//! The one operator conversation: every post archond makes (notices,
//! outcomes, gates, merges, stage messages, questions, answers, reports)
//! goes, in feed order, into the orchestrator's Aether transcript as an
//! important post, which the TUI's `/focus`, `archon chat` and the phone show.
//! The feed is its durable outbox: a post Aether cannot take yet waits there.

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, FeedItem, Notification, NotificationKind, Question};
use archon_queue::Store;
use tokio::sync::broadcast::error::RecvError;

use crate::context::DaemonContext;
use crate::executor::clip;
use crate::operator_commands::OUTCOME_FOLLOWS;
use crate::supervisor;

/// `kv`: the seq of the last feed item posted to the transcript.
pub const POSTED_KEY: &str = "transcript_posted_seq";
/// Characters of one post: 4 bytes each at most stays under Aether's 16 KiB.
const MAX_POST_CHARS: usize = 4096;
/// Feed items read from the store per catch-up query.
const BATCH: usize = 100;
/// How long a post waits when Aether or the orchestrator is not there yet.
const RETRY: Duration = Duration::from_secs(5);
/// Orchestrators one post follows through replacements before it waits.
const MAX_SWITCHES_PER_POST: usize = 3;
/// Characters of one question option; ten of them leave the question room.
const MAX_OPTION_CHARS: usize = 200;

/// One post: its markdown, its links and whether the focused views show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub text: String,
    pub links: Vec<String>,
    pub important: bool,
}

/// Post every feed item after the saved cursor, then each one appended.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut live = ctx.feed.subscribe();
    let mut cursor = {
        let mut store = ctx.store.lock().await;
        mark_start(&mut store)?;
        saved_cursor(&store)?
    };
    loop {
        let stalled = post_pending(&ctx, &mut cursor).await?;
        if stalled {
            tokio::time::sleep(RETRY).await;
            continue;
        }
        match live.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

/// On the first start, begin posting at the feed's end: older lines stay
/// readable in `archon log`. archond calls it before anything writes the feed.
pub fn mark_start(store: &mut Store) -> Result<(), ArchonError> {
    if store.kv_get(POSTED_KEY)?.is_some() {
        return Ok(());
    }
    let head = store.last_feed_seq()?;
    store.kv_set(POSTED_KEY, &head.to_string())?;
    assert_eq!(saved_cursor(store)?, head, "the cursor reads back");
    Ok(())
}

fn saved_cursor(store: &Store) -> Result<u64, ArchonError> {
    let raw = store
        .kv_get(POSTED_KEY)?
        .ok_or_else(|| ArchonError::Corrupt(format!("kv {POSTED_KEY} is not set")))?;
    raw.parse::<u64>()
        .map_err(|e| ArchonError::Corrupt(format!("kv {POSTED_KEY} = {raw:?}: {e}")))
}

/// Post the items after `cursor` in order. `true` when one must wait for
/// Aether or an orchestrator; the cursor then stays before it.
async fn post_pending(ctx: &DaemonContext, cursor: &mut u64) -> Result<bool, ArchonError> {
    loop {
        let batch = ctx.store.lock().await.feed_after(*cursor, BATCH)?;
        let done = batch.len() < BATCH;
        for item in batch {
            assert!(item.seq > *cursor, "feed_after returns later items");
            if !post_item(ctx, &item).await? {
                return Ok(true);
            }
            *cursor = item.seq;
            ctx.store
                .lock()
                .await
                .kv_set(POSTED_KEY, &cursor.to_string())?;
        }
        if done {
            return Ok(false);
        }
    }
}

/// `false` when the item must wait: Aether away or without posts, no
/// orchestrator, or a failure that may pass. Only a post Aether refuses as
/// invalid is logged and skipped, so it cannot hold the rest back. Delivery
/// is at least once: Aether takes no key that would drop a repeated post.
async fn post_item(ctx: &DaemonContext, item: &FeedItem) -> Result<bool, ArchonError> {
    let Some(post) = post_of(ctx, item).await? else {
        return Ok(true);
    };
    let Ok(client) = ctx.aether.client() else {
        return Ok(false);
    };
    if !client.serves_conversation() {
        return Ok(false);
    }
    // A replacement during an append may leave the post on the retired
    // transcript only, so it goes to the successor too: a repeat, not a loss.
    let mut posted_to = None;
    for _ in 0..MAX_SWITCHES_PER_POST {
        let Some(orchestrator) = supervisor::current_orchestrator(ctx).await? else {
            return Ok(false);
        };
        if posted_to.as_ref() == Some(&orchestrator) {
            return Ok(true);
        }
        if !send(ctx, &client, &orchestrator, &post, item.seq).await {
            return Ok(false);
        }
        posted_to = Some(orchestrator);
    }
    // Still switching: the retry posts it to whichever orchestrator is then current.
    Ok(false)
}

/// Append `post` to `orchestrator`'s transcript; `false` when it must be
/// retried, `true` once posted or refused as invalid.
async fn send(
    ctx: &DaemonContext,
    client: &AetherClient,
    orchestrator: &AetherAgentId,
    post: &Post,
    seq: u64,
) -> bool {
    let sent = match post.important {
        true => client.post(orchestrator, &post.text, &post.links).await,
        // 500 characters stay under a plain note's 2048 bytes.
        false => {
            client
                .append_note(orchestrator, &clip(&post.text, 500), None)
                .await
        }
    };
    match sent {
        Ok(()) => {
            ctx.conversation.send_modify(|posts| *posts += 1);
            true
        }
        Err(error @ ArchonError::Invalid(_)) => {
            tracing::warn!(seq, %error, "aetherd refused a post; it stays in the feed");
            true
        }
        Err(error @ ArchonError::Unavailable(_)) => {
            tracing::debug!(seq, %error, "a post waits for aetherd");
            false
        }
        Err(error) => {
            tracing::warn!(seq, %error, "a post failed; it and later ones are retried");
            false
        }
    }
}

/// The post a feed item makes, `None` for what the transcript holds already:
/// operator lines, orchestrator replies and the orchestrator's own `ask_user`
/// questions; and the audit line of a command whose result line follows it.
async fn post_of(ctx: &DaemonContext, item: &FeedItem) -> Result<Option<Post>, ArchonError> {
    let post = match &item.entry {
        FeedEntry::Operator { .. } | FeedEntry::Orchestrator { .. } => None,
        // Its own result line follows.
        FeedEntry::Authority { result, .. } if result == OUTCOME_FOLLOWS => None,
        FeedEntry::Authority { action, result, .. } => {
            Some(plain(format!("**Authority:** {action}: {result}")))
        }
        FeedEntry::Question(question) if asked_by_orchestrator(ctx, question).await? => None,
        FeedEntry::Question(question) => Some(question_post(question)),
        FeedEntry::Notification(notification) => Some(notification_post(notification)),
        FeedEntry::Answer { question, text, .. } => {
            let number = match ctx.store.lock().await.question(question) {
                Ok(asked) => format!("Q{}", asked.number),
                Err(ArchonError::NotFound(_)) => question.to_string(),
                Err(error) => return Err(error),
            };
            Some(plain(format!("**Answered {number}:** {text}")))
        }
        FeedEntry::Report {
            title, location, ..
        } => Some(report_post(title, location)),
        FeedEntry::System { text } => Some(plain(format!("**Archon:** {text}"))),
    };
    Ok(post.map(|post| Post {
        text: clip(&post.text, MAX_POST_CHARS),
        ..post
    }))
}

/// A stage agent's `ask_user` is not in the orchestrator's transcript, so
/// only the current orchestrator's own questions are left out.
async fn asked_by_orchestrator(
    ctx: &DaemonContext,
    question: &Question,
) -> Result<bool, ArchonError> {
    let Some(aether) = &question.aether else {
        return Ok(false);
    };
    let orchestrator = supervisor::current_orchestrator(ctx).await?;
    Ok(orchestrator.as_ref() == Some(&aether.agent))
}

fn plain(text: String) -> Post {
    assert!(!text.trim().is_empty(), "a post says something");
    Post {
        text,
        links: Vec::new(),
        important: true,
    }
}

/// `**Outcome** (qi_…): text`; progress is a plain note the focused views skip.
pub fn notification_post(notification: &Notification) -> Post {
    let label = match notification.kind {
        NotificationKind::Question => "Question",
        NotificationKind::Decision => "Decision",
        NotificationKind::Blocker => "Blocker",
        NotificationKind::Outcome => "Outcome",
        NotificationKind::Progress => "Progress",
    };
    let about = notification
        .item
        .as_ref()
        .map(|item| format!(" ({item})"))
        .unwrap_or_default();
    Post {
        important: notification.kind.shown_by_default(),
        ..plain(format!("**{label}**{about}: {}", notification.text))
    }
}

/// The question, its numbered options and how to answer it. Only the
/// question is clipped, so the options and the answer hint always fit.
pub fn question_post(question: &Question) -> Post {
    assert!(question.number > 0, "a recorded question is numbered");
    let n = question.number;
    let head = format!("**Q{n}:** ");
    let mut tail = String::new();
    for (i, option) in question.options.iter().enumerate() {
        let option = clip(option, MAX_OPTION_CHARS);
        tail.push_str(&format!("\n{}. {option}", i + 1));
    }
    tail.push_str(&format!(
        "\n\nAnswer with `/answer {n} …` or `archon answer {n} …`."
    ));
    let room = MAX_POST_CHARS.saturating_sub(head.chars().count() + tail.chars().count());
    // Ten options always leave room; more, never asked, clip the whole post.
    let text = match room > 1 {
        true => format!("{head}{}{tail}", clip(&question.text, room)),
        false => clip(&format!("{head}{}{tail}", question.text), MAX_POST_CHARS),
    };
    assert!(
        text.chars().count() <= MAX_POST_CHARS,
        "a question post fits"
    );
    plain(text)
}

/// The report's title, and its location as a link when it is a web page.
pub fn report_post(title: &str, location: &str) -> Post {
    let web = location.starts_with("https://") || location.starts_with("http://");
    let link_fits = web && location.len() <= 512;
    Post {
        links: link_fits.then(|| location.to_owned()).into_iter().collect(),
        ..plain(format!("**Report:** {title}\n\n{location}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::id::{NotificationId, QuestionId, QueueItemId};

    fn notification(kind: NotificationKind, item: Option<QueueItemId>) -> Notification {
        Notification {
            id: NotificationId::new(),
            kind,
            text: "PR #7 merged".into(),
            item,
            run: None,
            at: archon_core::now(),
        }
    }

    #[test]
    fn a_notification_names_its_kind_and_item_and_progress_is_not_important() {
        let item = QueueItemId::new();
        let post = notification_post(&notification(NotificationKind::Outcome, Some(item.clone())));
        assert_eq!(post.text, format!("**Outcome** ({item}): PR #7 merged"));
        assert!(post.important);
        let progress = notification_post(&notification(NotificationKind::Progress, None));
        assert_eq!(progress.text, "**Progress**: PR #7 merged");
        assert!(!progress.important, "the focused views skip progress");
    }

    #[test]
    fn a_question_lists_its_options_and_how_to_answer() {
        let question = Question {
            id: QuestionId::new(),
            number: 4,
            text: "Ship it?".into(),
            options: vec!["yes".into(), "no".into()],
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        };
        let post = question_post(&question);
        assert_eq!(
            post.text,
            "**Q4:** Ship it?\n1. yes\n2. no\n\nAnswer with `/answer 4 …` or `archon answer 4 …`."
        );
        assert!(post.important && post.links.is_empty());
    }

    /// A question as long as `ask` takes keeps its options and the answer
    /// hint: only the question itself is clipped to fit one post.
    #[test]
    fn a_long_question_keeps_its_options_and_how_to_answer() {
        let question = Question {
            id: QuestionId::new(),
            number: 9,
            text: "why? ".repeat(4_000),
            options: vec!["yes".into(), "no".into(), "x".repeat(1_000)],
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        };
        let post = question_post(&question);
        assert_eq!(post.text.chars().count(), MAX_POST_CHARS);
        assert!(post.text.starts_with("**Q9:** why? why?"), "{}", post.text);
        let third = format!("{}…", "x".repeat(MAX_OPTION_CHARS - 1));
        let tail = format!(
            "…\n1. yes\n2. no\n3. {third}\n\nAnswer with `/answer 9 …` or `archon answer 9 …`."
        );
        assert!(post.text.ends_with(&tail), "{}", post.text);
    }

    #[test]
    fn only_a_web_report_carries_a_link() {
        let web = report_post("Bench", "https://lavish.example/r/1");
        assert_eq!(web.links, vec!["https://lavish.example/r/1".to_owned()]);
        assert_eq!(web.text, "**Report:** Bench\n\nhttps://lavish.example/r/1");
        let file = report_post("Bench", "/tmp/report.html");
        assert!(file.links.is_empty());
    }
}
