//! Evidence and readiness for the immutable requirement ledger.

use serde::{Deserialize, Serialize};

use crate::intent::{IntentBinding, Viewport};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Implemented,
    Blocked,
    Deviation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VisualPair {
    /// The reference image's id in the approved intent.
    pub reference: String,
    pub built: String,
    pub viewport: Viewport,
    pub theme: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequirementResult {
    pub id: String,
    pub status: RequirementStatus,
    /// Evidence for implementation, or the missing dependency/deviation reason.
    pub evidence: String,
    #[serde(default)]
    pub visual_pairs: Vec<VisualPair>,
    /// A standing operator directive authorizing this particular deviation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proceed_directive: Option<crate::id::DirectiveId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceVerdict {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequirementJudgment {
    pub id: String,
    pub verdict: AcceptanceVerdict,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AcceptanceResult {
    pub head: String,
    pub tree: String,
    pub intent: IntentBinding,
    pub requirements: Vec<RequirementJudgment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AcceptanceRecord {
    pub result: AcceptanceResult,
    pub reviewer: crate::aether_ids::AetherAgentId,
    pub model: crate::model::ModelRef,
    pub checked_at: crate::Timestamp,
    pub page: String,
    /// Whether a model apart from every builder judged it; `false` when none
    /// was permitted and connected, so the record never claims independence.
    #[serde(default = "independent_by_default")]
    pub independent: bool,
}

/// Records written before the flag existed were refused unless independent.
fn independent_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    #[default]
    NotRequired,
    Pending,
    Passed,
    Failed,
    Stale,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AcceptanceReadiness {
    pub status: AcceptanceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<AcceptanceRecord>,
}

impl AcceptanceReadiness {
    pub fn at_head(
        binding: Option<&IntentBinding>,
        record: Option<AcceptanceRecord>,
        head: &str,
        tree: &str,
    ) -> Self {
        let Some(binding) = binding else {
            return Self::default();
        };
        let status = match &record {
            None => AcceptanceStatus::Pending,
            Some(r)
                if &r.result.intent != binding
                    || r.result.head != head
                    || r.result.tree != tree =>
            {
                AcceptanceStatus::Stale
            }
            Some(r)
                if r.result.requirements.is_empty()
                    || r.result
                        .requirements
                        .iter()
                        .any(|r| r.verdict == AcceptanceVerdict::Fail) =>
            {
                AcceptanceStatus::Failed
            }
            Some(_) => AcceptanceStatus::Passed,
        };
        assert!(status != AcceptanceStatus::Passed || !head.is_empty());
        assert!(status != AcceptanceStatus::Passed || !tree.is_empty());
        Self { status, record }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_head_tree_or_intent_invalidates_acceptance_and_legacy_items_need_none() {
        let binding = IntentBinding {
            id: "approved".into(),
            version: 1,
            requirements: vec!["R1".into()],
        };
        let record = AcceptanceRecord {
            result: AcceptanceResult {
                head: "a".repeat(40),
                tree: "b".repeat(40),
                intent: binding.clone(),
                requirements: vec![RequirementJudgment {
                    id: "R1".into(),
                    verdict: AcceptanceVerdict::Pass,
                    reason: "verified".into(),
                }],
            },
            reviewer: crate::aether_ids::AetherAgentId::parse("agt_reviewer").unwrap(),
            model: "claude-code/reviewer".parse().unwrap(),
            checked_at: crate::now(),
            page: "acceptance/index.html".into(),
            independent: true,
        };
        let current = |binding: Option<&IntentBinding>, head: &str, tree: &str| {
            AcceptanceReadiness::at_head(binding, Some(record.clone()), head, tree).status
        };
        assert_eq!(
            current(Some(&binding), &record.result.head, &record.result.tree),
            AcceptanceStatus::Passed
        );
        assert_eq!(
            current(Some(&binding), &"c".repeat(40), &record.result.tree),
            AcceptanceStatus::Stale
        );
        assert_eq!(
            current(Some(&binding), &record.result.head, &"d".repeat(40)),
            AcceptanceStatus::Stale
        );
        let revised = IntentBinding {
            version: 2,
            ..binding.clone()
        };
        assert_eq!(
            current(Some(&revised), &record.result.head, &record.result.tree),
            AcceptanceStatus::Stale
        );
        assert_eq!(current(None, "", ""), AcceptanceStatus::NotRequired);
    }
}
