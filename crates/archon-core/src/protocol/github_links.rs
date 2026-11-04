//! Protocol 24's GitHub links and cached objects (CONTRACTS.md §1.3,
//! owners D1, D2 and D3).

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::id::QueueItemId;

/// The most objects one `list_github_objects` may ask for.
pub const MAX_GITHUB_OBJECTS: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GithubKind {
    Issue,
    PullRequest,
}

/// An issue or pull request on a GitHub host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GithubRef {
    pub host: String,
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub kind: GithubKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// GitHub's review decision on a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Pending,
    Success,
    Failure,
    Neutral,
}

/// A pull request's checks, rolled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChecksSummary {
    pub state: ChecksState,
    pub total: u32,
    pub failed: u32,
}

/// A pull request with what is known of its state; the reference's fields
/// sit beside the state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PrRef {
    #[serde(flatten)]
    pub reference: GithubRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PrState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksSummary>,
}

/// A GitHub comment Archon recorded but has not seen posted: it is retried,
/// at most once on GitHub, and never dropped (`DaemonStatus::github_outbox`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GithubOutboxPending {
    /// The token its hidden marker carries.
    pub token: String,
    /// `reply` or `ship`.
    pub purpose: String,
    /// `owner/name#number` on github.com.
    pub target: String,
    pub attempts: u32,
    pub next_attempt_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: Timestamp,
}

impl GithubOutboxPending {
    /// `reply on o/r#5: 2 attempts, next 12:00:00Z (HTTP 502)`, as `status` shows it.
    pub fn describe(&self) -> String {
        let next = self.next_attempt_at.format("%Y-%m-%d %H:%M:%SZ");
        let why = self
            .last_error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        format!(
            "{} on {} not confirmed posted: {} attempts, next {next}{why}",
            self.purpose, self.target, self.attempts
        )
    }
}

/// How an item and a GitHub object relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkRelation {
    Origin,
    Imported,
    Delivered,
    Closes,
    Mentioned,
}

/// The relations an operator may add or remove with `link_issue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManualLinkRelation {
    Closes,
    Mentioned,
}

impl From<ManualLinkRelation> for LinkRelation {
    fn from(relation: ManualLinkRelation) -> Self {
        match relation {
            ManualLinkRelation::Closes => Self::Closes,
            ManualLinkRelation::Mentioned => Self::Mentioned,
        }
    }
}

/// Who recorded a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkSource {
    Watcher,
    Operator,
    Import,
    Ship,
    Backfill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GithubLink {
    pub item: QueueItemId,
    #[serde(rename = "ref")]
    pub reference: GithubRef,
    pub relation: LinkRelation,
    pub source: LinkSource,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GithubObjectState {
    Open,
    Closed,
    Merged,
}

/// An issue or pull request as last read from GitHub. Its text is
/// untrusted and grants no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GithubObject {
    #[serde(rename = "ref")]
    pub reference: GithubRef,
    pub title: String,
    pub state: GithubObjectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub updated_at: Timestamp,
    pub fetched_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> GithubRef {
        GithubRef {
            host: "github.com".into(),
            repo: "mengsig/archon".into(),
            number: 379,
            kind: GithubKind::Issue,
        }
    }

    /// A `PrRef` is a `GithubRef` with state beside it, not nested in it.
    #[test]
    fn a_pr_ref_flattens_its_reference() {
        let pr = PrRef {
            reference: GithubRef {
                kind: GithubKind::PullRequest,
                ..issue()
            },
            state: Some(PrState::Draft),
            review_decision: None,
            checks: None,
        };
        let wire = serde_json::to_value(&pr).unwrap();
        assert_eq!(wire["kind"], "pull_request");
        assert_eq!(wire["state"], "draft");
        assert!(wire.get("review_decision").is_none(), "{wire}");
        assert_eq!(serde_json::from_value::<PrRef>(wire).unwrap(), pr);
    }

    /// A manual link takes only the relations an operator may set.
    #[test]
    fn a_manual_link_relation_refuses_the_recorded_ones() {
        let closes: ManualLinkRelation = serde_json::from_str("\"closes\"").unwrap();
        assert_eq!(LinkRelation::from(closes), LinkRelation::Closes);
        assert!(serde_json::from_str::<ManualLinkRelation>("\"origin\"").is_err());
    }
}
