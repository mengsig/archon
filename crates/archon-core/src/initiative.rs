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

/// The shared contract is in every member's stage contract, so it stays small.
pub const MAX_CONTRACT_BYTES: usize = 2 * 1024;
pub const MAX_MEMBERS: usize = 16;
const MAX_TITLE_CHARS: usize = 200;

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
        check_contract(contract)?;
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

/// The whole contract, also after an append.
pub fn check_contract(contract: &str) -> Result<(), ArchonError> {
    let bytes = contract.trim().len();
    if bytes == 0 || bytes > MAX_CONTRACT_BYTES {
        return Err(ArchonError::Invalid(format!(
            "an initiative contract is 1 to {MAX_CONTRACT_BYTES} bytes, not {bytes}"
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
    pub fn check(&self) -> Result<(), ArchonError> {
        check_title(&self.title)?;
        check_contract(&self.contract)?;
        if self.members.is_empty() || self.members.len() > MAX_MEMBERS {
            return Err(ArchonError::Invalid(format!(
                "an initiative has 1 to {MAX_MEMBERS} members, not {}",
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
        assert!(check_contract(&"x".repeat(MAX_CONTRACT_BYTES)).is_ok());
        assert!(check_contract(&"x".repeat(MAX_CONTRACT_BYTES + 1)).is_err());
    }

    #[test]
    fn members_wait_only_for_earlier_members() {
        assert!(
            request(vec![member(&[]), member(&[0]), member(&[0, 1])])
                .check()
                .is_ok()
        );
        assert!(request(vec![]).check().is_err());
        assert!(request(vec![member(&[0])]).check().is_err());
        assert!(
            request(vec![member(&[]), member(&[2]), member(&[])])
                .check()
                .is_err()
        );
        assert!(request(vec![member(&[]); MAX_MEMBERS + 1]).check().is_err());
        let mut blank = member(&[]);
        blank.brief = " ".into();
        assert!(request(vec![blank]).check().is_err());
    }

    #[test]
    fn a_member_needs_only_a_repository_and_a_brief() {
        let parsed: InitiativeMember =
            serde_json::from_value(serde_json::json!({"repo": "api", "brief": "b"})).unwrap();
        assert_eq!(parsed.after, Vec::<usize>::new());
        assert_eq!(parsed.pipeline, None);
    }
}
