//! Tracked changes (ARCHITECTURE §11): every change `archon mcp` forwards
//! carries a request id, so a caller whose answer never came can learn the
//! outcome with `request_status`. With an idempotency key, a repeat of the
//! same call returns the first outcome instead of applying it again.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::ArchonError;
use crate::id::RequestId;
use crate::protocol::{ErrorReply, Reply};

/// How long archond keeps a finished request, its key and its outcome.
pub const RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// Longest idempotency key, in bytes.
pub const MAX_KEY_BYTES: usize = 128;

/// The MCP tools that take an `idempotency_key` of their own beside their
/// arguments; `enqueue` and `enqueue_initiative` keep theirs in the request.
pub const KEYED_TOOLS: [&str; 9] = [
    "refresh_item",
    "retry_item",
    "steer_item",
    "update_item",
    "cancel_item",
    "notify",
    "github_reply",
    "request_merge",
    "message_orchestrator",
];

/// Refuses a key that is empty, longer than [`MAX_KEY_BYTES`], or holds
/// anything but printable ASCII without spaces.
pub fn check_key(key: &str) -> Result<(), ArchonError> {
    let printable = key.bytes().all(|b| b.is_ascii_graphic());
    if key.is_empty() || key.len() > MAX_KEY_BYTES || !printable {
        return Err(ArchonError::Invalid(format!(
            "idempotency_key must be 1-{MAX_KEY_BYTES} bytes of printable ASCII without spaces, \
             not {key:?}"
        )));
    }
    Ok(())
}

/// Where a tracked request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackedState {
    /// archond is still carrying it out.
    Pending,
    /// It was carried out; `reply` holds its answer.
    Applied,
    /// It was refused or failed; `error` says why.
    Failed,
    /// archond stopped while it ran, so whether it took effect is unknown.
    Interrupted,
}

impl TrackedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ArchonError> {
        Ok(match raw {
            "pending" => Self::Pending,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            "interrupted" => Self::Interrupted,
            other => {
                return Err(ArchonError::Corrupt(format!(
                    "unknown tracked request state `{other}`"
                )));
            }
        })
    }
}

/// What `request_status`, or a repeat under the same key, reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestStatus {
    pub request: RequestId,
    /// The wire name of the change (`cmd`), e.g. `refresh_item`.
    pub command: String,
    pub state: TrackedState,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// The answer of an applied request.
    #[serde(default)]
    pub reply: Option<Box<Reply>>,
    /// Why a failed request failed.
    #[serde(default)]
    pub error: Option<ErrorReply>,
    pub started_at: Timestamp,
    #[serde(default)]
    pub finished_at: Option<Timestamp>,
}

impl RequestStatus {
    /// Whether the fields agree with the state: only an applied request has
    /// a reply, only a failed one an error, and a pending one no end.
    pub fn is_consistent(&self) -> bool {
        let finished = self.finished_at.is_some();
        match self.state {
            TrackedState::Pending => !finished && self.reply.is_none() && self.error.is_none(),
            TrackedState::Applied => finished && self.reply.is_some() && self.error.is_none(),
            TrackedState::Failed => finished && self.reply.is_none() && self.error.is_some(),
            TrackedState::Interrupted => self.reply.is_none() && self.error.is_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_short_printable_ascii_without_spaces() {
        assert!(check_key("retry-qi_01-attempt-2").is_ok());
        assert!(check_key(&"k".repeat(MAX_KEY_BYTES)).is_ok());
        for bad in [
            "",
            "two words",
            "tab\there",
            "é",
            &"k".repeat(MAX_KEY_BYTES + 1),
        ] {
            let refused = check_key(bad).unwrap_err().to_string();
            assert!(refused.contains("idempotency_key"), "{refused}");
        }
    }

    #[test]
    fn every_state_round_trips_through_its_name() {
        use TrackedState::*;
        for state in [Pending, Applied, Failed, Interrupted] {
            assert_eq!(TrackedState::parse(state.as_str()).unwrap(), state);
        }
        assert!(
            TrackedState::parse("done")
                .unwrap_err()
                .to_string()
                .contains("done")
        );
    }
}
