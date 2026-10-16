//! Where a repository's integration branch stands (issue #82): verified
//! off the items' path, as `status` and `queue_status` show it.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::id::QueueItemId;

/// What the latest look at an integration branch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationHealth {
    /// Its head passed the full check.
    Green,
    /// Its head failed the full check.
    Red,
    /// The full check, or GitHub's checks, run on its head now.
    Verifying,
    /// Merges since the last verdict wait for their batch.
    Pending,
    /// Nothing could be verified yet; `problem` says why.
    Unknown,
}

impl IntegrationHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Verifying => "verifying",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
        }
    }
}

/// One repository's integration branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub repo: String,
    pub branch: String,
    pub health: IntegrationHealth,
    /// The head archond last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_green: Option<String>,
    /// First-parent commits from the last green commit (or the fork from
    /// the release branch) to the head.
    pub merges_since_green: u32,
    /// The first merge a bisection found red, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub culprit: Option<String>,
    /// The item queued to repair a red head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_item: Option<QueueItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<Timestamp>,
}

impl IntegrationStatus {
    /// `archon/next: red at 1a2b3c4d; last green 9f8e7d6c, 4 merges since; …`,
    /// as `status` and `queue_status` show it.
    pub fn describe(&self) -> String {
        assert!(!self.branch.is_empty(), "a watched branch has a name");
        let head = self.head.as_deref().map_or("no head yet", short);
        let green = self
            .last_green
            .as_deref()
            .map_or("never green".to_owned(), |c| {
                format!("last green {}", short(c))
            });
        let mut text = format!(
            "{}/{}: {} at {head}; {green}, {} merge(s) since",
            self.repo,
            self.branch,
            self.health.as_str(),
            self.merges_since_green
        );
        if let Some(culprit) = &self.culprit {
            text.push_str(&format!("; broken by {culprit}"));
        }
        if let Some(item) = &self.fix_item {
            text.push_str(&format!("; fix item {item}"));
        }
        if let Some(problem) = &self.problem {
            text.push_str(&format!("; {problem}"));
        }
        text
    }
}

/// The first eight characters of a commit id.
pub fn short(commit: &str) -> &str {
    let end = commit
        .char_indices()
        .nth(8)
        .map_or(commit.len(), |(i, _)| i);
    &commit[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_red_branch_names_its_last_green_commit_culprit_and_fix() {
        let status = IntegrationStatus {
            repo: "archon".into(),
            branch: "next".into(),
            health: IntegrationHealth::Red,
            head: Some("1a2b3c4d5e6f".into()),
            last_green: Some("9f8e7d6c5b4a".into()),
            merges_since_green: 4,
            culprit: Some("5d6e7f80 Merge pull request #12".into()),
            fix_item: None,
            problem: None,
            verified_at: None,
        };
        assert_eq!(
            status.describe(),
            "archon/next: red at 1a2b3c4d; last green 9f8e7d6c, 4 merge(s) since; \
             broken by 5d6e7f80 Merge pull request #12"
        );
        let fresh = IntegrationStatus {
            health: IntegrationHealth::Unknown,
            head: None,
            last_green: None,
            merges_since_green: 0,
            culprit: None,
            problem: Some("no checks.full".into()),
            ..status
        };
        assert_eq!(
            fresh.describe(),
            "archon/next: unknown at no head yet; never green, 0 merge(s) since; no checks.full"
        );
        assert_eq!(short("abc"), "abc");
    }
}
