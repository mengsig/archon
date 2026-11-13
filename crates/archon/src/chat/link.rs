//! The chat's connection to `archond`: one client that reads the
//! orchestrator's conversation page by page, each read waiting for what
//! follows the chat's cursor. It reconnects with backoff after the daemon
//! restarts or the connection drops, and resumes after the cursor.
//! Reconnecting never starts `archond`: an operator who stopped it meant it.

use std::path::PathBuf;
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::conversation::ConversationPage;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Reply, Request};
use tokio::time::Instant;

const FIRST_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(15);
const NOT_RUNNING: &str = "archond is not running; `archon daemon start` starts it";

#[derive(Debug)]
pub enum LinkEvent {
    Page(Box<ConversationPage>),
    /// The connection just dropped, with why.
    Lost(String),
    /// Connected again.
    Restored,
    /// A reconnect attempt failed for a reason not reported yet.
    StillDown(String),
    /// archond is there but could not read the conversation (Aether down),
    /// for a reason not reported yet; the read is retried.
    Unreadable(String),
}

#[derive(Debug)]
enum State {
    Up {
        client: ArchonClient,
        /// When the next read may start, after a failed one.
        retry_at: Option<Instant>,
        backoff: Duration,
        reported: Option<String>,
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
    pub fn up(socket: PathBuf, client: ArchonClient) -> Self {
        assert!(!socket.as_os_str().is_empty(), "the chat knows its socket");
        Self {
            socket,
            state: up(client),
        }
    }

    /// The live client, or `None` while reconnecting.
    pub fn client(&self) -> Option<&ArchonClient> {
        match &self.state {
            State::Up { client, .. } if !client.is_closed() => Some(client),
            _ => None,
        }
    }

    /// The next thing that happened after `after` (the newest page of
    /// `history` events when `None`). Cancel-safe: a dropped read is asked
    /// again, and an interrupted reconnect is simply retried.
    pub async fn next(&mut self, after: Option<u64>, history: u32) -> LinkEvent {
        assert!(history > 0, "a page reads at least one event");
        loop {
            let event = match &mut self.state {
                State::Up { .. } => self.read(after, history).await,
                State::Down { .. } => self.retry().await,
            };
            if let Some(event) = event {
                return event;
            }
        }
    }

    /// One conversation read; `None` when a repeated failure was reported.
    async fn read(&mut self, after: Option<u64>, history: u32) -> Option<LinkEvent> {
        let State::Up {
            client,
            retry_at,
            backoff,
            reported,
        } = &mut self.state
        else {
            unreachable!("only an up link reads");
        };
        // Cleared only after the wait, so a read dropped mid-wait still waits.
        if let Some(at) = *retry_at {
            tokio::time::sleep_until(at).await;
            *retry_at = None;
        }
        let request = Request::Conversation {
            after,
            before: None,
            limit: after.is_none().then_some(history),
            wait: after.is_some(),
        };
        let error = match client.request(request).await {
            Ok(Reply::Conversation(page)) => {
                (*backoff, *reported) = (FIRST_RETRY, None);
                return Some(LinkEvent::Page(Box::new(page)));
            }
            Ok(other) => ArchonError::Protocol(format!("conversation answered {other:?}")),
            Err(error) => error,
        };
        if client.is_closed() {
            self.state = down(FIRST_RETRY, None);
            return Some(LinkEvent::Lost(error.to_string()));
        }
        let message = error.to_string();
        let fresh = reported.as_deref() != Some(message.as_str());
        *retry_at = Some(Instant::now() + *backoff);
        *backoff = (*backoff * 2).min(MAX_RETRY);
        *reported = Some(message.clone());
        fresh.then_some(LinkEvent::Unreadable(message))
    }

    /// One reconnect attempt; `None` when a repeated failure was reported.
    async fn retry(&mut self) -> Option<LinkEvent> {
        let State::Down {
            retry_at,
            backoff,
            reported,
        } = &self.state
        else {
            unreachable!("only a down link reconnects");
        };
        let (retry_at, backoff, reported) = (*retry_at, *backoff, reported.clone());
        tokio::time::sleep_until(retry_at).await;
        let connected = ArchonClient::connect(&self.socket, ClientKind::ArchonCli).await;
        match connected {
            Ok(client) => {
                self.state = up(client.acting_for(crate::connect::principal()));
                Some(LinkEvent::Restored)
            }
            Err(error) => {
                let message = describe(&error);
                let fresh = reported.as_deref() != Some(message.as_str());
                self.state = down((backoff * 2).min(MAX_RETRY), Some(message.clone()));
                fresh.then_some(LinkEvent::StillDown(message))
            }
        }
    }
}

fn up(client: ArchonClient) -> State {
    State::Up {
        client,
        retry_at: None,
        backoff: FIRST_RETRY,
        reported: None,
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
