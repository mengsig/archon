//! Operator provenance comes from the daemon's event stream, never tool arguments.

use serde::{Deserialize, Serialize};

use crate::{Timestamp, aether_ids::AetherAgentId, github::GithubRepo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum OperatorChannel {
    AetherChat,
    Github {
        repository: GithubRepo,
        number: u64,
        login: String,
    },
}

impl OperatorChannel {
    pub fn is_github(&self) -> bool {
        matches!(self, Self::Github { .. })
    }
}

/// A bounded copy of a message archond authenticated itself. The timestamp
/// is the event's original time, so reconnecting cannot refresh old consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorMessage {
    pub agent: AetherAgentId,
    pub seq: u64,
    pub channel: OperatorChannel,
    pub text: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthorityConfig {
    /// Whether verified GitHub operators may approve a loosening change.
    pub github_may_loosen: bool,
}

impl Default for AuthorityConfig {
    fn default() -> Self {
        Self {
            github_may_loosen: true,
        }
    }
}
