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

    /// An inbox message approves a ledger only through the images the operator
    /// attached to it (`attached`, their digests); prose proves no ledger.
    /// Archond, not the caller, writes its requirements: one match per image.
    pub fn into_inbox_ledger(mut self, attached: &[String]) -> Result<Self, ArchonError> {
        assert!(
            matches!(self.source, IntentSource::Inbox { .. }),
            "only an inbox record is checked against the operator's message"
        );
        assert!(attached.iter().all(|digest| !digest.is_empty()));
        let refused = |detail: String| {
            ArchonError::Invalid(format!(
                "a plain brief creates no approved intent: {detail}. An inbox message approves \
                 a ledger only through reference images the operator attached to it, with \
                 `requirements: []`: Archon records one visual requirement per image, its id \
                 the image id. A written requirement list or a named file becomes a ledger \
                 only when the operator approves that exact ledger through a gate (the \
                 proposal gate showing it); otherwise enqueue the item without an intent"
            ))
        };
        if self.images.is_empty() {
            return Err(refused(
                "the quoted operator message carries no attached reference image".into(),
            ));
        }
        if let Some(image) = self.images.iter().find(|i| !attached.contains(&i.sha256)) {
            return Err(refused(format!(
                "reference image {:?} is not an image the operator attached to the quoted message",
                image.id
            )));
        }
        if let Some(artifact) = self.artifacts.first() {
            return Err(refused(format!(
                "artifact {} is not an attachment the operator sent",
                artifact.path.display()
            )));
        }
        if let Some(written) = self.requirements.first() {
            return Err(refused(format!(
                "requirement {:?} is written by the caller, not the operator",
                written.id
            )));
        }
        self.requirements = self.images.iter().map(image_match).collect();
        assert_eq!(self.requirements.len(), self.images.len());
        Ok(self)
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

/// The one requirement an attached reference image approves: that the work
/// matches it, in the context the image records.
fn image_match(image: &ReferenceImage) -> Requirement {
    Requirement {
        id: image.id.clone(),
        text: format!(
            "Match the reference image {:?} the operator attached",
            image.id
        ),
        kind: RequirementKind::Visual,
        images: vec![image.id.clone()],
        owner: "operator".into(),
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

    fn image(sha256: &str) -> ReferenceImage {
        ReferenceImage {
            id: "mockup".into(),
            path: "mockup.png".into(),
            sha256: sha256.into(),
            viewport: None,
            theme: None,
            state: None,
        }
    }

    const BRIEF: &str = "Implement `retry_budget.py` according to `SPEC.md`. Preserve the \
                         documented API, handle every specified error and boundary case, and \
                         add focused tests.";

    #[test]
    fn prose_requirements_or_paths_without_an_attached_image_are_no_inbox_ledger() {
        let invented = inbox(BRIEF, &["Validate all arguments before any early return"]);
        let excerpt = inbox(BRIEF, &["add focused tests"]);
        let listed = inbox(
            "Requirements:\n- The header stays blue",
            &["The header stays blue"],
        );
        let mut pathed = inbox(
            "Match /srv/shots/mockup.png",
            &["Match the approved layout"],
        );
        pathed.images.push(image(&"a".repeat(64)));
        for record in [invented, excerpt, listed, pathed] {
            record.validate().unwrap();
            let error = record.into_inbox_ledger(&[]).unwrap_err().to_string();
            assert!(
                error.contains("a plain brief creates no approved intent"),
                "{error}"
            );
            assert!(error.contains("through a gate"), "{error}");
        }
    }

    #[test]
    fn only_images_the_operator_attached_make_an_inbox_ledger() {
        let digest = "a".repeat(64);
        let mut pictured = inbox("Match this mockup", &[]);
        pictured.images.push(image(&digest));
        let ledger = pictured
            .clone()
            .into_inbox_ledger(std::slice::from_ref(&digest))
            .unwrap();
        ledger.validate().unwrap();
        let only = &ledger.requirements;
        assert_eq!(only.len(), 1);
        assert_eq!(
            (only[0].id.as_str(), &only[0].kind, &only[0].images),
            (
                "mockup",
                &RequirementKind::Visual,
                &vec!["mockup".to_owned()]
            )
        );
        let error = pictured
            .clone()
            .into_inbox_ledger(&["b".repeat(64)])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not an image the operator attached"),
            "{error}"
        );
        let mut artifact = pictured.clone();
        artifact.artifacts.push(IntentArtifact {
            path: "/srv/design/page.html".into(),
            sha256: digest.clone(),
        });
        let error = artifact
            .into_inbox_ledger(std::slice::from_ref(&digest))
            .unwrap_err()
            .to_string();
        assert!(error.contains("page.html is not an attachment"), "{error}");
    }

    #[test]
    fn a_caller_written_requirement_beside_an_attached_image_is_refused() {
        let digest = "a".repeat(64);
        let mut padded = inbox("Match this mockup", &["Rewrite the login flow"]);
        padded.images.push(image(&digest));
        let mislabelled = {
            let mut record = padded.clone();
            record.requirements[0].kind = RequirementKind::Visual;
            record.requirements[0].images = vec!["mockup".into()];
            record
        };
        for record in [padded, mislabelled] {
            record.validate().unwrap();
            let error = record
                .into_inbox_ledger(std::slice::from_ref(&digest))
                .unwrap_err()
                .to_string();
            assert!(error.contains("\"R1\" is written by the caller"), "{error}");
            assert!(error.contains("`requirements: []`"), "{error}");
        }
    }
}
