//! The chat's connection to `archond`: one client plus one feed
//! subscription, re-established with backoff after the daemon restarts or
//! the connection drops, resuming after the last seq the chat has seen.
//! Reconnecting never starts `archond`: an operator who stopped it meant it.

use std::path::PathBuf;
use std::time::Duration;

use archon_client::{ArchonClient, FeedSubscription};
use archon_core::error::ArchonError;
use archon_core::notify::FeedItem;
use archon_core::protocol::ClientKind;
use tokio::time::Instant;

const FIRST_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(15);
const NOT_RUNNING: &str = "archond is not running; `archon daemon start` starts it";

#[derive(Debug)]
pub enum LinkEvent {
    Item(Box<FeedItem>),
    /// The connection just dropped, with why.
    Lost(String),
    /// Connected and subscribed again. `reset_to` is set when the daemon's
    /// feed is behind the chat's cursor (a different store): the chat
    /// continues from that seq instead.
    Restored {
        reset_to: Option<u64>,
    },
    /// A reconnect attempt failed for a reason not reported yet.
    StillDown(String),
}

#[derive(Debug)]
enum State {
    Up {
        client: ArchonClient,
        feed: FeedSubscription,
    },
    Down {
        retry_at: Instant,
        backoff: Duration,
        reported: Option<String>,
    },
}

#[derive(Debug)]
pub struct Link {
    socket: PathBuf,
    state: State,
}

impl Link {
    pub fn up(socket: PathBuf, client: ArchonClient, feed: FeedSubscription) -> Self {
        assert!(!socket.as_os_str().is_empty(), "the chat knows its socket");
        Self {
            socket,
            state: State::Up { client, feed },
        }
    }

    /// The live client, or `None` while reconnecting.
    pub fn client(&self) -> Option<&ArchonClient> {
        match &self.state {
            State::Up { client, .. } if !client.is_closed() => Some(client),
            _ => None,
        }
    }

    /// The next thing that happened. Cancel-safe: dropping the future loses
    /// no feed item, and an interrupted reconnect is simply retried.
    pub async fn next(&mut self, after: u64) -> LinkEvent {
        loop {
            match &mut self.state {
                State::Up { feed, .. } => match feed.items.recv().await {
                    Some(item) => return LinkEvent::Item(Box::new(item)),
                    None => {
                        let reason = feed.close_reason();
                        self.state = down(FIRST_RETRY, None);
                        return LinkEvent::Lost(reason);
                    }
                },
                State::Down {
                    retry_at,
                    backoff,
                    reported,
                } => {
                    let (retry_at, backoff, reported) = (*retry_at, *backoff, reported.clone());
                    tokio::time::sleep_until(retry_at).await;
                    match self.reconnect(after).await {
                        Ok(event) => return event,
                        Err(error) => {
                            let message = describe(&error);
                            let fresh = reported.as_deref() != Some(message.as_str());
                            let next = (backoff * 2).min(MAX_RETRY);
                            self.state = down(next, Some(message.clone()));
                            if fresh {
                                return LinkEvent::StillDown(message);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Connect to a running daemon and resume the feed after `after`.
    async fn reconnect(&mut self, after: u64) -> Result<LinkEvent, ArchonError> {
        let (client, feed) = self.subscribe(after).await?;
        if feed.last_seq >= after {
            self.state = State::Up { client, feed };
            return Ok(LinkEvent::Restored { reset_to: None });
        }
        // Resubscribing on a fresh connection: the client refuses items at or
        // below the cursor it subscribed with, and this feed is behind it.
        let head = feed.last_seq;
        drop((client, feed));
        let (client, feed) = self.subscribe(head).await?;
        self.state = State::Up { client, feed };
        Ok(LinkEvent::Restored {
            reset_to: Some(head),
        })
    }

    async fn subscribe(&self, after: u64) -> Result<(ArchonClient, FeedSubscription), ArchonError> {
        let client = ArchonClient::connect(&self.socket, ClientKind::ArchonCli)
            .await?
            .acting_for(crate::connect::principal());
        // Always verbose: /progress then filters locally without resubscribing.
        let feed = client.subscribe_feed(Some(after), true).await?;
        Ok((client, feed))
    }
}

/// Why a reconnect failed, in the operator's terms.
fn describe(error: &ArchonError) -> String {
    match error {
        ArchonError::Unavailable(_) => NOT_RUNNING.to_owned(),
        other => other.to_string(),
    }
}

fn down(backoff: Duration, reported: Option<String>) -> State {
    assert!(!backoff.is_zero(), "a zero backoff would spin");
    State::Down {
        retry_at: Instant::now() + backoff,
        backoff,
        reported,
    }
}
