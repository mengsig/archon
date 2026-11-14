//! Where the archon arm reads the orchestrator's answers. From 1.5.1
//! archond keeps them only in the conversation that `archon chat`, the TUI
//! and the phone show, which is the orchestrator's Aether transcript; 1.5.0
//! and older write them to the feed. Both are measured by one rule, the
//! tracker's, and a trial has one orchestrator from before the prompt on.

use std::path::Path;

use anyhow::{Context, bail, ensure};
use archon_aether::AetherClient;
use archon_aether::wire::EventQuery;
use archon_client::ArchonClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{Reply, Request};

use super::tracker::{Answer, ReplyTracker};
use crate::arms::aether::endpoint;

/// Most events one read of the transcript asks Aether for.
const PAGE_EVENTS: usize = 1000;

/// The orchestrator's answers since the prompt, read from their source.
#[derive(Debug, Clone, PartialEq)]
pub enum Replies {
    /// They are the feed's `Orchestrator` lines.
    Feed,
    /// Derived from the orchestrator's transcript, in order.
    Transcript(Vec<Answer>),
}

impl Replies {
    /// Whether the orchestrator has answered the prompt yet.
    pub fn replied(&self, feed: &[FeedItem]) -> bool {
        match self {
            Self::Feed => feed
                .iter()
                .any(|i| matches!(i.entry, FeedEntry::Orchestrator { .. })),
            Self::Transcript(answers) => !answers.is_empty(),
        }
    }
}

/// The trial's one orchestrator and the answers read from it so far.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyLog {
    orchestrator: AetherAgentId,
    source: Source,
}

#[derive(Debug, Clone, PartialEq)]
enum Source {
    Feed,
    Transcript {
        /// The last Aether event applied to the tracker.
        cursor: u64,
        tracker: ReplyTracker,
        answers: Vec<Answer>,
    },
}

impl ReplyLog {
    /// Before the prompt: record the orchestrator, and probe archond for
    /// the conversation; an archond without it keeps answers in the feed.
    pub async fn start(client: &ArchonClient, aether_socket: &Path) -> anyhow::Result<Self> {
        let orchestrator = super::orchestrator_agent(client).await?;
        let request = Request::Conversation {
            after: None,
            before: None,
            limit: Some(1),
            wait: false,
        };
        match client.request(request).await {
            Ok(Reply::Conversation(_)) => {
                let aether = AetherClient::connect(&endpoint(aether_socket)).await?;
                let cursor = newest_event(&aether, &orchestrator).await?;
                Ok(Self::transcript(orchestrator, cursor))
            }
            Ok(other) => bail!("conversation returned another reply kind: {other:?}"),
            Err(e) if lacks_conversation(&e) => Ok(Self::feed(orchestrator)),
            Err(e) => Err(e).context("probing archond for the conversation"),
        }
    }

    pub fn feed(orchestrator: AetherAgentId) -> Self {
        Self {
            orchestrator,
            source: Source::Feed,
        }
    }

    /// Answers read from `orchestrator`'s events after `cursor`.
    pub fn transcript(orchestrator: AetherAgentId, cursor: u64) -> Self {
        let source = Source::Transcript {
            cursor,
            tracker: ReplyTracker::default(),
            answers: Vec::new(),
        };
        Self {
            orchestrator,
            source,
        }
    }

    pub fn replies(&self) -> Replies {
        match &self.source {
            Source::Feed => Replies::Feed,
            Source::Transcript { answers, .. } => Replies::Transcript(answers.clone()),
        }
    }

    /// Fail the trial when archond now reports another orchestrator, then
    /// read what the transcript added since the last refresh.
    pub async fn refresh(
        &mut self,
        current: &AetherAgentId,
        aether_socket: &Path,
    ) -> anyhow::Result<Replies> {
        self.check_orchestrator(current)?;
        if self.source == Source::Feed {
            return Ok(Replies::Feed);
        }
        let aether = AetherClient::connect(&endpoint(aether_socket)).await?;
        loop {
            let query = EventQuery {
                agent: Some(self.orchestrator.clone()),
                ..EventQuery::default()
            };
            let page = aether
                .events_after(self.cursor(), query, PAGE_EVENTS)
                .await
                .with_context(|| format!("reading {}'s events", self.orchestrator))?;
            let full = page.len() == PAGE_EVENTS;
            self.apply(&page)?;
            if !full {
                return Ok(self.replies());
            }
        }
    }

    /// The trial is void once another orchestrator answers it.
    pub fn check_orchestrator(&self, current: &AetherAgentId) -> anyhow::Result<()> {
        ensure!(
            *current == self.orchestrator,
            "orchestrator replaced: the trial began with {} and archond now reports {current}",
            self.orchestrator
        );
        Ok(())
    }

    /// Apply a page of the orchestrator's events, oldest first.
    pub fn apply(&mut self, page: &[archon_aether::AetherEvent]) -> anyhow::Result<()> {
        let Source::Transcript {
            cursor,
            tracker,
            answers,
        } = &mut self.source
        else {
            bail!("feed answers are not read from a transcript");
        };
        for event in page {
            ensure!(
                event.seq > *cursor,
                "event {} is not after the cursor {cursor}",
                event.seq
            );
            ensure!(
                event.agent.as_ref() == Some(&self.orchestrator),
                "event {} is not the orchestrator's",
                event.seq
            );
            *cursor = event.seq;
            answers.extend(tracker.apply(event));
        }
        Ok(())
    }

    fn cursor(&self) -> u64 {
        match &self.source {
            Source::Feed => unreachable!("a feed log reads no events"),
            Source::Transcript { cursor, .. } => *cursor,
        }
    }
}

/// The seq of `agent`'s newest event, 0 when it has none.
async fn newest_event(aether: &AetherClient, agent: &AetherAgentId) -> anyhow::Result<u64> {
    let query = EventQuery {
        agent: Some(agent.clone()),
        ..EventQuery::default()
    };
    let newest = aether
        .latest_events_raw(query, 1)
        .await
        .with_context(|| format!("reading {agent}'s newest event"))?;
    match newest.first() {
        None => Ok(0),
        Some(event) => event["seq"].as_u64().context("an event without seq"),
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
