//! Where the archon arm reads the orchestrator's replies. From 1.5.1
//! archond keeps them only in the conversation that `archon chat`, the TUI
//! and the phone show; 1.5.0 and older write them to the feed instead.

use anyhow::{Context, bail, ensure};
use archon_client::ArchonClient;
use archon_core::conversation::{ConversationEntry, EntryKind};
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{Reply, Request};

/// Most events one conversation page reads, as archond caps it.
const PAGE_EVENTS: u32 = 1000;

/// Where this trial's archond keeps the orchestrator's replies, fixed
/// before the prompt is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySource {
    /// An archond without the `conversation` request: its feed's
    /// `Orchestrator` lines.
    Feed,
    /// The conversation's events after `after`, its end before the prompt.
    Conversation { after: u64 },
}

/// The orchestrator's replies since the prompt, read from their source.
#[derive(Debug, Clone, PartialEq)]
pub enum Replies {
    /// They are the feed's `Orchestrator` lines.
    Feed,
    /// The orchestrator's own text in the conversation, in order.
    Conversation(Vec<ConversationEntry>),
}

impl Replies {
    /// The orchestrator's own entries in a conversation read: its text,
    /// not the operator's lines, archond's posts or `ask_user` questions,
    /// which the feed already holds.
    pub fn from_conversation(entries: Vec<ConversationEntry>) -> Self {
        let replies: Vec<ConversationEntry> = entries
            .into_iter()
            .filter(|e| e.kind == EntryKind::Assistant)
            .collect();
        assert!(
            replies
                .windows(2)
                .all(|w| (w[0].seq, w[0].part) < (w[1].seq, w[1].part)),
            "replies keep the conversation's order"
        );
        Self::Conversation(replies)
    }

    /// Whether the orchestrator has answered the prompt yet.
    pub fn replied(&self, feed: &[FeedItem]) -> bool {
        match self {
            Self::Feed => feed.iter().any(|i| is_feed_reply(&i.entry)),
            Self::Conversation(replies) => !replies.is_empty(),
        }
    }

    /// Whether a feed entry is left out because the replies come from the
    /// conversation, so no reply is counted twice.
    pub fn supersedes(&self, entry: &FeedEntry) -> bool {
        matches!(self, Self::Conversation(_)) && is_feed_reply(entry)
    }
}

fn is_feed_reply(entry: &FeedEntry) -> bool {
    matches!(entry, FeedEntry::Orchestrator { .. })
}

/// Probe for the conversation and take its end as the trial's baseline.
pub async fn reply_source(client: &ArchonClient) -> anyhow::Result<ReplySource> {
    let request = Request::Conversation {
        after: None,
        before: None,
        limit: Some(1),
        wait: false,
    };
    match client.request(request).await {
        Ok(Reply::Conversation(page)) => {
            assert!(page.is_ordered(), "archond serves ordered pages");
            let after = page.last_seq.max(page.to_seq);
            Ok(ReplySource::Conversation { after })
        }
        Ok(other) => bail!("conversation returned another reply kind: {other:?}"),
        Err(e) if lacks_conversation(&e) => Ok(ReplySource::Feed),
        Err(e) => Err(e).context("reading the end of the orchestrator's conversation"),
    }
}

/// Whether `error` is an archond that predates the `conversation` request
/// refusing it as an unknown command.
pub fn lacks_conversation(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Invalid(text)
            if text.starts_with("not a request") && text.contains("`conversation`")
    )
}

/// Every reply since the baseline; the feed source needs no read.
pub async fn read_replies(client: &ArchonClient, source: ReplySource) -> anyhow::Result<Replies> {
    let ReplySource::Conversation { after } = source else {
        return Ok(Replies::Feed);
    };
    let entries = conversation_after(client, after).await?;
    Ok(Replies::from_conversation(entries))
}

/// Every important conversation entry after `after`, page by page.
async fn conversation_after(
    client: &ArchonClient,
    after: u64,
) -> anyhow::Result<Vec<ConversationEntry>> {
    let mut cursor = after;
    let mut entries = Vec::new();
    loop {
        let request = Request::Conversation {
            after: Some(cursor),
            before: None,
            limit: Some(PAGE_EVENTS),
            wait: false,
        };
        let Reply::Conversation(page) = client.request(request).await? else {
            bail!("conversation returned another reply kind");
        };
        ensure!(
            page.is_ordered() && page.entries.iter().all(|e| e.seq > cursor),
            "conversation page after {cursor} is out of order or behind its cursor"
        );
        entries.extend(page.entries);
        if !page.has_after {
            return Ok(entries);
        }
        ensure!(
            page.to_seq > cursor,
            "conversation page after {cursor} has more but did not advance"
        );
        cursor = page.to_seq;
    }
}
