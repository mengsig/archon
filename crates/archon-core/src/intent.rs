//! Immutable records of the operator's approved requirements.

use crate::error::ArchonError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApprovedIntent {
    pub id: String,
    pub version: u32,
    pub operator_words: String,
    pub source: IntentSource,
    pub artifacts: Vec<IntentArtifact>,
    pub images: Vec<ReferenceImage>,
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntentSource {
    Inbox { seq: u64 },
    Gate { item: String, gate: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntentArtifact {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceImage {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    #[serde(default)]
    pub viewport: Option<Viewport>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequirementKind {
    Visual,
    Behaviour,
    Wording,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    pub kind: RequirementKind,
    pub images: Vec<String>,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntentBinding {
    pub id: String,
    pub version: u32,
    #[serde(default)]
    pub requirements: Vec<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Case, whitespace and trailing punctuation do not change a quote.
fn normalized(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined
        .trim_end_matches(['.', ',', ';', ':', '!'])
        .to_lowercase()
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl ApprovedIntent {
    pub fn validate(&self) -> Result<(), ArchonError> {
        if !valid_id(&self.id) || self.version == 0 || self.operator_words.trim().is_empty() {
            return Err(ArchonError::Invalid(
                "intent needs a valid id, positive version and operator words".into(),
            ));
        }
        match &self.source {
            IntentSource::Inbox { seq: 0 } => {
                return Err(ArchonError::Invalid(
                    "inbox sequence must be positive".into(),
                ));
            }
            IntentSource::Gate { item, gate }
                if item.trim().is_empty() || gate.trim().is_empty() =>
            {
                return Err(ArchonError::Invalid(
                    "gate source needs an item and gate".into(),
                ));
            }
            _ => {}
        }
        if self.artifacts.len() > 128 || self.images.len() > 128 || self.requirements.len() > 1024 {
            return Err(ArchonError::Invalid(
                "intent exceeds the artifact, image or requirement limit".into(),
            ));
        }
        if self.requirements.is_empty() {
            return Err(ArchonError::Invalid(
                "intent needs at least one requirement".into(),
            ));
        }
        let images = self.validate_images()?;
        self.validate_requirements(&images)
    }

    fn validate_images(&self) -> Result<HashSet<&str>, ArchonError> {
        let mut images = HashSet::new();
        for image in &self.images {
            if !valid_id(&image.id)
                || !images.insert(image.id.as_str())
                || image.path.as_os_str().is_empty()
                || !valid_hash(&image.sha256)
                || image
                    .viewport
                    .as_ref()
                    .is_some_and(|v| v.width == 0 || v.height == 0)
            {
                return Err(ArchonError::Invalid(format!(
                    "invalid reference image {:?}",
                    image.id
                )));
            }
        }
        if self
            .artifacts
            .iter()
            .any(|a| a.path.as_os_str().is_empty() || !valid_hash(&a.sha256))
        {
            return Err(ArchonError::Invalid("invalid intent artifact".into()));
        }
        Ok(images)
    }

    fn validate_requirements(&self, images: &HashSet<&str>) -> Result<(), ArchonError> {
        let mut ids = HashSet::new();
        for requirement in &self.requirements {
            if !valid_id(&requirement.id)
                || !ids.insert(requirement.id.as_str())
                || requirement.text.trim().is_empty()
                || requirement.owner.trim().is_empty()
                || requirement.images.iter().collect::<HashSet<_>>().len()
                    != requirement.images.len()
                || (requirement.kind == RequirementKind::Visual && requirement.images.is_empty())
                || requirement
                    .images
                    .iter()
                    .any(|id| !images.contains(id.as_str()))
            {
                return Err(ArchonError::Invalid(format!(
                    "invalid requirement {:?}",
                    requirement.id
                )));
            }
        }
        Ok(())
    }

    /// An inbox record is a ledger the operator approved only when it carries
    /// approved reference images or every requirement quotes the operator's words.
    pub fn check_operator_ledger(&self) -> Result<(), ArchonError> {
        assert!(
            matches!(self.source, IntentSource::Inbox { .. }),
            "only an inbox record needs proof of an operator-approved ledger"
        );
        if !self.images.is_empty() {
            return Ok(());
        }
        let words = normalized(&self.operator_words);
        assert!(!words.is_empty(), "validated operator words are non-empty");
        let unquoted = self
            .requirements
            .iter()
            .find(|r| normalized(&r.text).is_empty() || !words.contains(&normalized(&r.text)));
        match unquoted {
            None => Ok(()),
            Some(requirement) => Err(ArchonError::Invalid(format!(
                "a plain brief creates no approved intent: requirement {:?} is not quoted from \
                 the operator's words and the record has no approved reference images. File \
                 approve_intent only for approved references or a requirement list the operator \
                 wrote, each quoted verbatim; otherwise enqueue the item without an intent",
                requirement.id
            ))),
        }
    }

    /// `binding` with its selection spelled out, so no stored binding has an
    /// empty list that could be read as an intent with zero requirements.
    pub fn explicit_binding(&self, binding: &IntentBinding) -> Result<IntentBinding, ArchonError> {
        let requirements: Vec<String> = self
            .selected_requirements(binding)?
            .into_iter()
            .map(|r| r.id.clone())
            .collect();
        if requirements.is_empty() {
            return Err(ArchonError::Invalid(
                "an approved intent binding must select at least one requirement".into(),
            ));
        }
        assert!(
            binding.requirements.is_empty() || binding.requirements.len() == requirements.len()
        );
        Ok(IntentBinding {
            id: binding.id.clone(),
            version: binding.version,
            requirements,
        })
    }

    pub fn selected_requirements(
        &self,
        binding: &IntentBinding,
    ) -> Result<Vec<&Requirement>, ArchonError> {
        self.validate()?;
        if binding.id != self.id || binding.version != self.version {
            return Err(ArchonError::Invalid(
                "intent binding does not match record".into(),
            ));
        }
        let mut selected = HashSet::new();
        for id in &binding.requirements {
            if !selected.insert(id.as_str()) || !self.requirements.iter().any(|r| &r.id == id) {
                return Err(ArchonError::Invalid(format!(
                    "unknown or duplicate requirement {id:?}"
                )));
            }
        }
        Ok(self
            .requirements
            .iter()
            .filter(|r| selected.is_empty() || selected.contains(r.id.as_str()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbox(words: &str, texts: &[&str]) -> ApprovedIntent {
        ApprovedIntent {
            id: "brief".into(),
            version: 1,
            operator_words: words.into(),
            source: IntentSource::Inbox { seq: 2 },
            artifacts: vec![],
            images: vec![],
            requirements: texts
                .iter()
                .enumerate()
                .map(|(i, text)| Requirement {
                    id: format!("R{}", i + 1),
                    text: (*text).into(),
                    kind: RequirementKind::Behaviour,
                    images: vec![],
                    owner: "operator".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_plain_brief_paraphrased_into_requirements_is_no_operator_ledger() {
        let brief = "Implement `retry_budget.py` according to `SPEC.md`. Preserve the documented \
                     API, handle every specified error and boundary case, and add focused tests.";
        let invented = inbox(
            brief,
            &["Validate all arguments before any early return; ints only (no bool/float)."],
        );
        invented.validate().unwrap();
        let error = invented.check_operator_ledger().unwrap_err().to_string();
        assert!(
            error.contains("a plain brief creates no approved intent"),
            "{error}"
        );
        assert!(error.contains("\"R1\""), "{error}");
    }

    #[test]
    fn a_quoted_requirement_list_or_approved_references_make_a_ledger() {
        let words =
            "Approve these requirements:\n- The header stays  blue.\n- Errors name the file";
        let listed = inbox(words, &["the header stays blue", "Errors name the file."]);
        assert!(listed.check_operator_ledger().is_ok());
        let mut pictured = inbox("Match this mockup", &["Match the approved layout"]);
        pictured.images.push(ReferenceImage {
            id: "mockup".into(),
            path: "mockup.png".into(),
            sha256: "a".repeat(64),
            viewport: None,
            theme: None,
            state: None,
        });
        assert!(pictured.check_operator_ledger().is_ok());
    }
}
