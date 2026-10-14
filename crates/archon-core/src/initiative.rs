//! Initiatives: several items, usually in several repositories, built
//! against one shared contract.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::autonomy::AutonomyPatch;
use crate::error::ArchonError;
use crate::id::{InitiativeId, QueueItemId};
use crate::model::ModelPlan;
use crate::protocol::RepoRef;
use crate::queue::QueueItem;

const MAX_TITLE_CHARS: usize = 200;

/// How large an initiative may be: `[limits] initiative_members` and
/// `initiative_contract_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitiativeLimits {
    pub members: usize,
    pub contract_bytes: usize,
}

impl InitiativeLimits {
    pub const DEFAULT: Self = Self {
        members: 16,
        contract_bytes: 2 * 1024,
    };
    /// The most `[limits]` may raise them to: the contract is in every member's
    /// stage contract, and a set's members merge together.
    pub const CEILING: Self = Self {
        members: 64,
        contract_bytes: 16 * 1024,
    };

    pub fn within(&self, other: &Self) -> bool {
        self.members <= other.members && self.contract_bytes <= other.contract_bytes
    }
}

/// How `update_initiative` changes the shared contract; the field named is
/// the variant (`contract_append` or `contract`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractChange {
    /// Text added after the contract, separated by a blank line.
    #[serde(rename = "contract_append")]
    Append(String),
    /// A whole new contract in place of the old one.
    #[serde(rename = "contract")]
    Replace(String),
}

impl ContractChange {
    pub fn text(&self) -> &str {
        match self {
            Self::Append(text) | Self::Replace(text) => text,
        }
    }

    /// `current` after this change, within `limits`.
    pub fn apply(&self, current: &str, limits: &InitiativeLimits) -> Result<String, ArchonError> {
        let text = self.text().trim();
        if text.is_empty() {
            return Err(ArchonError::Invalid(
                "the contract change must not be empty".into(),
            ));
        }
        let changed = match self {
            Self::Append(_) => format!("{current}\n\n{text}"),
            Self::Replace(_) => text.to_owned(),
        };
        check_contract(&changed, limits)?;
        assert!(changed.ends_with(text), "the change is the contract's end");
        assert!(matches!(self, Self::Replace(_)) || changed.starts_with(current));
        Ok(changed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitiativeState {
    /// At least one member has not finished.
    Open,
    Shipping,
    #[serde(alias = "succeeded")]
    Done,
    Failed,
    Cancelled,
}

impl InitiativeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Shipping => "shipping",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Its members are the items whose `initiative` names it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Initiative {
    pub id: InitiativeId,
    pub title: String,
    pub contract: String,
    /// Keep every member's pull request a draft until all members pass.
    #[serde(default)]
    pub atomic: bool,
    pub state: InitiativeState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Initiative {
    pub fn new(
        title: &str,
        contract: &str,
        atomic: bool,
        at: Timestamp,
    ) -> Result<Self, ArchonError> {
        check_title(title)?;
        check_contract(contract, &InitiativeLimits::CEILING)?;
        Ok(Self {
            id: InitiativeId::new(),
            title: title.trim().to_owned(),
            contract: contract.trim().to_owned(),
            atomic,
            state: InitiativeState::Open,
            created_at: at,
            updated_at: at,
        })
    }
}

fn check_title(title: &str) -> Result<(), ArchonError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS || title.contains(['\n', '\r']) {
        return Err(ArchonError::Invalid(format!(
            "an initiative title is one nonempty line of at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(())
}

/// The whole contract, also after a change.
pub fn check_contract(contract: &str, limits: &InitiativeLimits) -> Result<(), ArchonError> {
    let bytes = contract.trim().len();
    let most = limits.contract_bytes;
    if bytes == 0 || bytes > most {
        return Err(ArchonError::Invalid(format!(
            "an initiative contract is 1 to {most} bytes \
             (`[limits] initiative_contract_bytes`), not {bytes}"
        )));
    }
    Ok(())
}

/// One item an `enqueue_initiative` creates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitiativeMember {
    pub repo: RepoRef,
    pub brief: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_plan: Option<ModelPlan>,
    /// Knobs for this member's item, as in an enqueue. An atomic
    /// initiative's members merge by hand, together.
    #[serde(default, skip_serializing_if = "AutonomyPatch::is_empty")]
    pub autonomy: AutonomyPatch,
    /// Positions in the request's `members` this one depends on; each is
    /// earlier than its own, so the members cannot form a cycle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnqueueInitiative {
    pub title: String,
    pub contract: String,
    #[serde(default)]
    pub atomic: bool,
    pub members: Vec<InitiativeMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl EnqueueInitiative {
    /// Everything that can be refused without the store or the catalog.
    pub fn check(&self, limits: &InitiativeLimits) -> Result<(), ArchonError> {
        check_title(&self.title)?;
        check_contract(&self.contract, limits)?;
        if self.members.is_empty() || self.members.len() > limits.members {
            return Err(ArchonError::Invalid(format!(
                "an initiative has 1 to {} members (`[limits] initiative_members`), not {}",
                limits.members,
                self.members.len()
            )));
        }
        for (position, member) in self.members.iter().enumerate() {
            if member.brief.trim().is_empty() {
                return Err(ArchonError::Invalid(format!(
                    "member {position} needs a nonempty brief"
                )));
            }
            if let Some(bad) = member.after.iter().find(|after| **after >= position) {
                return Err(ArchonError::Invalid(format!(
                    "member {position} lists after {bad}; a member waits only for earlier members"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitiativeDetail {
    pub initiative: Initiative,
    pub members: Vec<QueueItem>,
}

/// What a member's stage agents are told about its initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeBrief {
    pub title: String,
    pub contract: String,
    pub atomic: bool,
    pub siblings: Vec<Sibling>,
}

/// Another member, as far as its work exists yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sibling {
    pub item: QueueItemId,
    /// The repository's registry name.
    pub repo: String,
    pub title: String,
    pub state: String,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(after: &[usize]) -> InitiativeMember {
        InitiativeMember {
            repo: RepoRef("api".into()),
            brief: "Add the endpoint.".into(),
            title: None,
            pipeline: None,
            model_plan: None,
            autonomy: AutonomyPatch::default(),
            after: after.to_vec(),
        }
    }

    fn request(members: Vec<InitiativeMember>) -> EnqueueInitiative {
        EnqueueInitiative {
            title: "Rename the field".into(),
            contract: "The field is `account_id` everywhere.".into(),
            atomic: false,
            members,
            idempotency_key: None,
        }
    }

    #[test]
    fn a_new_initiative_is_open_and_trimmed() {
        let at = crate::now();
        let made = Initiative::new("  Rename  ", " shared \n", true, at).unwrap();
        assert_eq!(
            (made.title.as_str(), made.contract.as_str()),
            ("Rename", "shared")
        );
        assert_eq!(made.state, InitiativeState::Open);
        assert!(!made.state.is_terminal() && made.atomic);
        let json = serde_json::to_value(&made).unwrap();
        assert_eq!(json["state"], "open");
        assert_eq!(serde_json::from_value::<Initiative>(json).unwrap(), made);
    }

    #[test]
    fn titles_and_contracts_are_bounded() {
        let at = crate::now();
        assert!(Initiative::new(" ", "c", false, at).is_err());
        assert!(Initiative::new("two\nlines", "c", false, at).is_err());
        assert!(Initiative::new("t", "  ", false, at).is_err());
        let limits = InitiativeLimits::DEFAULT;
        assert!(check_contract(&"x".repeat(2048), &limits).is_ok());
        assert!(check_contract(&"x".repeat(2049), &limits).is_err());
        let raised = InitiativeLimits {
            contract_bytes: 4096,
            ..limits
        };
        assert!(check_contract(&"x".repeat(2049), &raised).is_ok());
        assert!(Initiative::new("t", &"x".repeat(16 * 1024 + 1), false, at).is_err());
    }

    #[test]
    fn a_contract_is_appended_to_or_replaced_within_its_limit() {
        let limits = InitiativeLimits::DEFAULT;
        let append = ContractChange::Append(" Old names stay. ".into());
        assert_eq!(
            append.apply("Use account_id.", &limits).unwrap(),
            "Use account_id.\n\nOld names stay."
        );
        let replace = ContractChange::Replace("Use customer_id.\n".into());
        assert_eq!(
            replace.apply("Use account_id.", &limits).unwrap(),
            "Use customer_id."
        );
        assert!(
            ContractChange::Replace(" ".into())
                .apply("c", &limits)
                .is_err()
        );
        let full = "x".repeat(2040);
        assert!(append.apply(&full, &limits).is_err());
        assert!(replace.apply(&full, &limits).is_ok());
        let json = |change: &ContractChange| serde_json::to_value(change).unwrap();
        assert_eq!(
            json(&append),
            serde_json::json!({"contract_append": " Old names stay. "})
        );
        assert_eq!(
            json(&replace),
            serde_json::json!({"contract": "Use customer_id.\n"})
        );
    }

    #[test]
    fn members_wait_only_for_earlier_members() {
        let limits = InitiativeLimits::DEFAULT;
        let check = |members| request(members).check(&limits);
        assert!(check(vec![member(&[]), member(&[0]), member(&[0, 1])]).is_ok());
        assert!(check(vec![]).is_err());
        assert!(check(vec![member(&[0])]).is_err());
        assert!(check(vec![member(&[]), member(&[2]), member(&[])]).is_err());
        assert!(check(vec![member(&[]); 17]).is_err());
        let mut blank = member(&[]);
        blank.brief = " ".into();
        assert!(check(vec![blank]).is_err());
        let three = InitiativeLimits {
            members: 3,
            ..limits
        };
        assert!(request(vec![member(&[]); 4]).check(&three).is_err());
        assert!(request(vec![member(&[]); 3]).check(&three).is_ok());
    }

    #[test]
    fn a_member_needs_only_a_repository_and_a_brief() {
        let parsed: InitiativeMember =
            serde_json::from_value(serde_json::json!({"repo": "api", "brief": "b"})).unwrap();
        assert_eq!(parsed.after, Vec::<usize>::new());
        assert_eq!(parsed.pipeline, None);
    }
}
