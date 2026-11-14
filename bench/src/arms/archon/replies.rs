//! Where the archon arm reads the orchestrator's replies. From 1.5.1
//! archond keeps them only in the conversation that `archon chat`, the TUI
//! and the phone show; 1.5.0 and older write them to the feed instead.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use archon_aether::AetherClient;
use archon_client::ArchonClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::conversation::{ConversationEntry, EntryKind};
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{Reply, Request};

use crate::arms::aether::endpoint;

/// Most events one conversation page reads, as Aether caps it.
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

/// The replies read so far, from every orchestrator the trial had. Each
/// transcript is read from aetherd itself: archond reads only the current
/// one, so a retired orchestrator's last replies would be lost to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyLog {
    /// The conversation's end before the prompt; a newly seen orchestrator
    /// is read from here, as Aether's event seq is global.
    baseline: u64,
    /// Per orchestrator, the last event read of its transcript.
    cursors: BTreeMap<AetherAgentId, u64>,
    /// By `(seq, part)`, which a successor that kept the transcript repeats.
    kept: BTreeMap<(u64, u32), ConversationEntry>,
    replies: Replies,
}

impl ReplyLog {
    /// Probe archond for the conversation before the prompt and take its
    /// end as the baseline, enrolling the orchestrator that will read the
    /// prompt; an archond without it keeps replies in the feed.
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
                let mut log = Self::after(page.last_seq.max(page.to_seq));
                log.enroll(super::orchestrator_agent(client).await?);
                Ok(log)
            }
            Ok(other) => bail!("conversation returned another reply kind: {other:?}"),
            Err(e) if lacks_conversation(&e) => Ok(Self::feed()),
            Err(e) => Err(e).context("reading the end of the orchestrator's conversation"),
        }
    }

    /// Replies kept in the feed: nothing to read.
    pub fn feed() -> Self {
        Self {
            baseline: 0,
            cursors: BTreeMap::new(),
            kept: BTreeMap::new(),
            replies: Replies::Feed,
        }
    }

    /// Replies read from the conversation's events after `baseline`.
    pub fn after(baseline: u64) -> Self {
        Self {
            replies: Replies::Conversation(Vec::new()),
            baseline,
            ..Self::feed()
        }
    }

    pub fn replies(&self) -> &Replies {
        &self.replies
    }

    /// Read `agent`'s transcript from the baseline on every refresh until
    /// the trial ends, even once another orchestrator replaces it.
    pub fn enroll(&mut self, agent: AetherAgentId) {
        assert!(self.replies != Replies::Feed, "only a conversation is read");
        self.cursors.entry(agent).or_insert(self.baseline);
        assert!(!self.cursors.is_empty(), "an enrolled agent is read");
    }

    /// The orchestrators whose transcripts are read, with their cursors.
    pub fn cursors(&self) -> &BTreeMap<AetherAgentId, u64> {
        &self.cursors
    }

    /// Enroll `current`, the orchestrator archond reports now, then read
    /// what every enrolled orchestrator added since the last read.
    pub async fn refresh(
        &mut self,
        current: AetherAgentId,
        aether_socket: &Path,
    ) -> anyhow::Result<&Replies> {
        if self.replies == Replies::Feed {
            return Ok(&self.replies);
        }
        self.enroll(current);
        let aether = AetherClient::connect(&endpoint(aether_socket)).await?;
        let agents: Vec<AetherAgentId> = self.cursors.keys().cloned().collect();
        for agent in agents {
            let after = self.cursors[&agent];
            let (entries, reached) = conversation_after(&aether, &agent, after).await?;
            self.record(&agent, entries, reached)?;
        }
        Ok(&self.replies)
    }

    /// Keep the orchestrator's own entries of one read of `agent`'s
    /// transcript that ended at event `reached`, and move its cursor there.
    pub fn record(
        &mut self,
        agent: &AetherAgentId,
        entries: Vec<ConversationEntry>,
        reached: u64,
    ) -> anyhow::Result<()> {
        ensure!(
            self.replies != Replies::Feed,
            "feed replies are not read from a conversation"
        );
        let cursor = self.cursors.entry(agent.clone()).or_insert(self.baseline);
        ensure!(
            entries.iter().all(|e| *cursor < e.seq && e.seq <= reached),
            "{agent}'s conversation entries fall outside events {cursor}..={reached}"
        );
        *cursor = (*cursor).max(reached);
        let Replies::Conversation(new) = Replies::from_conversation(entries) else {
            unreachable!("built from a conversation");
        };
        for entry in new {
            self.kept.insert((entry.seq, entry.part), entry);
        }
        self.replies = Replies::Conversation(self.kept.values().cloned().collect());
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

/// Every important entry of `agent`'s transcript after `after`, page by
/// page, and the last event read.
async fn conversation_after(
    aether: &AetherClient,
    agent: &AetherAgentId,
    after: u64,
) -> anyhow::Result<(Vec<ConversationEntry>, u64)> {
    let mut cursor = after;
    let mut entries = Vec::new();
    loop {
        let page = aether
            .conversation(agent, Some(cursor), None, Some(PAGE_EVENTS))
            .await
            .with_context(|| format!("reading {agent}'s conversation after {cursor}"))?;
        ensure!(
            page.entries.iter().all(|e| e.seq > cursor),
            "{agent}'s conversation page after {cursor} is behind its cursor"
        );
        entries.extend(page.entries);
        if !page.has_after {
            return Ok((entries, cursor.max(page.to_seq)));
        }
        ensure!(
            page.to_seq > cursor,
            "{agent}'s conversation page after {cursor} has more but did not advance"
        );
        cursor = page.to_seq;
    }
}
