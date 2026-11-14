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

/// The orchestrator's replies since the prompt, read from their source.
#[derive(Debug, Clone, PartialEq)]
pub enum Replies {
    /// They are the feed's `Orchestrator` lines.
    Feed,
    /// The orchestrator's own text in the conversation, in order.
    Conversation(Vec<ConversationEntry>),
}

impl Replies {
    /// The orchestrator's own entries in a conversation read: all its text,
    /// as the operator reads it there, not the operator's lines, archond's
    /// posts or `ask_user` questions, which the feed already holds.
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

/// The replies read so far. They are kept across polls, so when the
/// orchestrator is replaced mid-trial, the retired transcript's replies stay.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyLog {
    /// The conversation's last event read; Aether's seq is global, so it
    /// carries over to a successor's transcript.
    cursor: u64,
    replies: Replies,
}

impl ReplyLog {
    /// Probe for the conversation before the prompt and take its end as
    /// the baseline; an archond without it keeps the replies in the feed.
    pub async fn start(client: &ArchonClient) -> anyhow::Result<Self> {
        let request = Request::Conversation {
            after: None,
            before: None,
            limit: Some(1),
            wait: false,
        };
        match client.request(request).await {
            Ok(Reply::Conversation(page)) => {
                assert!(page.is_ordered(), "archond serves ordered pages");
                Ok(Self::after(page.last_seq.max(page.to_seq)))
            }
            Ok(other) => bail!("conversation returned another reply kind: {other:?}"),
            Err(e) if lacks_conversation(&e) => Ok(Self::feed()),
            Err(e) => Err(e).context("reading the end of the orchestrator's conversation"),
        }
    }

    /// Replies kept in the feed: nothing to read.
    pub fn feed() -> Self {
        Self {
            cursor: 0,
            replies: Replies::Feed,
        }
    }

    /// Replies read from the conversation's events after `cursor`.
    pub fn after(cursor: u64) -> Self {
        Self {
            cursor,
            replies: Replies::Conversation(Vec::new()),
        }
    }

    pub fn replies(&self) -> &Replies {
        &self.replies
    }

    /// Read what the conversation gained since the last read.
    pub async fn refresh(&mut self, client: &ArchonClient) -> anyhow::Result<&Replies> {
        if self.replies == Replies::Feed {
            return Ok(&self.replies);
        }
        let (entries, reached) = conversation_after(client, self.cursor).await?;
        self.record(entries, reached)?;
        Ok(&self.replies)
    }

    /// Keep the orchestrator's own entries of one read that ended at event
    /// `reached`, and move the cursor there.
    pub fn record(&mut self, entries: Vec<ConversationEntry>, reached: u64) -> anyhow::Result<()> {
        let Replies::Conversation(kept) = &mut self.replies else {
            bail!("feed replies are not read from a conversation");
        };
        ensure!(
            entries
                .iter()
                .all(|e| self.cursor < e.seq && e.seq <= reached),
            "conversation entries fall outside events {}..={reached}",
            self.cursor
        );
        let Replies::Conversation(new) = Replies::from_conversation(entries) else {
            unreachable!("built from a conversation");
        };
        kept.extend(new);
        self.cursor = self.cursor.max(reached);
        Ok(())
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

/// Every important conversation entry after `after`, page by page, and the
/// last event read.
async fn conversation_after(
    client: &ArchonClient,
    after: u64,
) -> anyhow::Result<(Vec<ConversationEntry>, u64)> {
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
            return Ok((entries, cursor.max(page.to_seq)));
        }
        ensure!(
            page.to_seq > cursor,
            "conversation page after {cursor} has more but did not advance"
        );
        cursor = page.to_seq;
    }
}
