//! The risk level every pull request Archon opens or updates is labelled
//! with (ARCHITECTURE §8.7). Strictly advisory: it says what a merge should
//! take, and nothing in Archon merges, gates or reviews by it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How much human attention a pull request asks for before it merges.
/// Ordered: `Low < Medium < High`, so the higher of two calls is their `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// The level `text` names, in any case; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(text))
    }

    /// The GitHub label: `risk: low`, `risk: medium` or `risk: high`.
    pub fn label(self) -> String {
        format!("{LABEL_PREFIX}{}", self.as_str())
    }

    /// The level a GitHub label names, when it is one of the three.
    pub fn of_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.label() == label)
    }

    /// The label's colour, as `gh label create --color` takes it.
    pub fn color(self) -> &'static str {
        match self {
            Self::Low => "0e8a16",
            Self::Medium => "fbca04",
            Self::High => "d73a4a",
        }
    }

    /// What the level means for merging, in one line: the label's
    /// description, and the first line of a pull request's risk section.
    pub fn meaning(self) -> &'static str {
        match self {
            Self::Low => "May be merged automatically, with no human review.",
            Self::Medium => "Needs at least one human reviewer; agents must not merge it.",
            Self::High => {
                "Needs at least two human reviewers who read the functional code changes."
            }
        }
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What every risk label starts with.
pub const LABEL_PREFIX: &str = "risk: ";

/// The rubric agents label by, as the stage contract and the docs state it.
pub const RUBRIC: &str = "\
- `low`: docs, comments, tests only, typo or formatting, and small isolated changes \
with no behaviour change, or a trivial one fully covered by tests.\n\
- `medium`: contained behaviour changes and new features behind existing interfaces, \
plus minor dependency bumps.\n\
- `high`: security, auth, permissions, or anything touching secrets; persistence \
formats and migrations, data deletion, concurrency or locking; protocol, API or CLI \
changes that break callers; merge, push, ship, release or deploy machinery; and large \
cross-cutting refactors.\n\
- When in doubt, pick the higher level.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_parse_and_name_their_labels() {
        assert!(RiskLevel::Low < RiskLevel::Medium && RiskLevel::Medium < RiskLevel::High);
        assert_eq!(RiskLevel::Low.max(RiskLevel::High), RiskLevel::High);
        for level in RiskLevel::ALL {
            assert_eq!(RiskLevel::parse(level.as_str()), Some(level));
            assert_eq!(RiskLevel::of_label(&level.label()), Some(level));
            assert!(
                level.meaning().chars().count() <= 100,
                "a GitHub label description"
            );
            assert_eq!(level.color().len(), 6);
        }
        assert_eq!(RiskLevel::parse(" HIGH "), Some(RiskLevel::High));
        assert_eq!(RiskLevel::parse("risky"), None);
        assert_eq!(RiskLevel::Medium.label(), "risk: medium");
        assert_eq!(RiskLevel::of_label("risk: urgent"), None);
        let json = serde_json::to_string(&RiskLevel::Medium).unwrap();
        assert_eq!(json, "\"medium\"");
    }
}
