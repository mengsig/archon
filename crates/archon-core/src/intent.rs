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

/// The operator's list items: lines led by `-`, `*`, `•`, `1.` or `1)`,
/// marker removed and [`normalized`].
fn listed_lines(words: &str) -> Vec<String> {
    let item = |line: &str| -> Option<String> {
        let line = line.trim_start();
        let rest = match line.strip_prefix(['-', '*', '•']) {
            Some(rest) => rest,
            None => {
                let digits = line.bytes().take_while(u8::is_ascii_digit).count();
                let rest = line[digits..].strip_prefix(['.', ')'])?;
                (digits > 0).then_some(rest)?
            }
        };
        let text = normalized(rest.strip_prefix([' ', '\t'])?);
        (!text.is_empty()).then_some(text)
    };
    words.lines().filter_map(item).collect()
}

/// Whether `words` name the file at `path`: a whole word that is its file
/// name or ends in `/` and its file name.
fn names_file(words: &str, path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let suffix = format!("/{name}");
    words
        .split(|c: char| c.is_whitespace() || "`\"'()<>[],".contains(c))
        .map(|word| word.trim_end_matches(['.', ':', ';', '!', '?']))
        .any(|word| word == name || word.ends_with(&suffix))
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

    /// Ties an inbox record's inputs to the operator: each image is one they
    /// attached to the quoted message or named, each artifact one they named.
    pub fn check_provenance(&self, attached: &[String]) -> Result<(), ArchonError> {
        assert!(
            matches!(self.source, IntentSource::Inbox { .. }),
            "only an inbox record is checked against the operator's message"
        );
        assert!(attached.iter().all(|digest| !digest.is_empty()));
        let words = &self.operator_words;
        let sent = |i: &&ReferenceImage| attached.contains(&i.sha256) || names_file(words, &i.path);
        if let Some(image) = self.images.iter().find(|i| !sent(i)) {
            return Err(ArchonError::Invalid(format!(
                "reference image {:?} is neither attached to nor named in the quoted operator \
                 message; only images the operator sent are approved references",
                image.id
            )));
        }
        if let Some(artifact) = self.artifacts.iter().find(|a| !names_file(words, &a.path)) {
            return Err(ArchonError::Invalid(format!(
                "intent artifact {} is not named in the operator's words",
                artifact.path.display()
            )));
        }
        Ok(())
    }

    /// An inbox record is a ledger the operator approved only when it carries
    /// their reference images or binds exactly the requirement list they wrote.
    pub fn check_operator_ledger(&self) -> Result<(), ArchonError> {
        assert!(
            matches!(self.source, IntentSource::Inbox { .. }),
            "only an inbox record needs proof of an operator-approved ledger"
        );
        assert!(
            !self.requirements.is_empty(),
            "a validated record has requirements"
        );
        if !self.images.is_empty() {
            return Ok(());
        }
        let mut listed = listed_lines(&self.operator_words);
        let refused = |detail: String| {
            ArchonError::Invalid(format!(
                "a plain brief creates no approved intent: {detail}. File approve_intent only for \
                 reference images the operator attached or a requirement list they wrote (one \
                 `-`, `*` or numbered line each), binding every listed line quoted whole; \
                 otherwise enqueue the item without an intent"
            ))
        };
        if listed.is_empty() {
            return Err(refused(
                "the operator's words hold no requirement list and the record has no reference \
                 images"
                    .into(),
            ));
        }
        for requirement in &self.requirements {
            let at = listed
                .iter()
                .position(|line| *line == normalized(&requirement.text));
            let Some(at) = at else {
                return Err(refused(format!(
                    "requirement {:?} is not one of the operator's listed lines",
                    requirement.id
                )));
            };
            listed.swap_remove(at);
        }
        match listed.first() {
            None => Ok(()),
            Some(line) => Err(refused(format!(
                "the operator's listed line {line:?} has no requirement"
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
    fn a_plain_brief_paraphrased_or_excerpted_into_requirements_is_no_operator_ledger() {
        let invented = inbox(
            BRIEF,
            &["Validate all arguments before any early return; ints only (no bool/float)."],
        );
        let excerpt = inbox(BRIEF, &["add focused tests"]);
        for record in [invented, excerpt] {
            record.validate().unwrap();
            let error = record.check_operator_ledger().unwrap_err().to_string();
            assert!(
                error.contains("a plain brief creates no approved intent"),
                "{error}"
            );
            assert!(error.contains("hold no requirement list"), "{error}");
        }
    }

    #[test]
    fn a_requirement_list_binds_only_whole_listed_lines_and_all_of_them() {
        let words =
            "Approve these requirements:\n- The header stays  blue.\n2) Errors name the file";
        let listed = inbox(words, &["the header stays blue", "Errors name the file."]);
        assert!(listed.check_operator_ledger().is_ok());
        let partial = inbox(words, &["the header stays blue"]).check_operator_ledger();
        let error = partial.unwrap_err().to_string();
        assert!(
            error.contains("\"errors name the file\" has no requirement"),
            "{error}"
        );
        let excerpt = inbox(words, &["the header stays", "Errors name the file"]);
        let error = excerpt.check_operator_ledger().unwrap_err().to_string();
        assert!(
            error.contains("\"R1\" is not one of the operator's listed lines"),
            "{error}"
        );
    }

    #[test]
    fn only_images_the_operator_attached_and_artifacts_they_named_count() {
        let digest = "a".repeat(64);
        let mut pictured = inbox("Match this mockup", &["Match the approved layout"]);
        pictured.images.push(image(&digest));
        assert!(
            pictured
                .check_provenance(std::slice::from_ref(&digest))
                .is_ok()
        );
        assert!(pictured.check_operator_ledger().is_ok());
        let error = pictured.check_provenance(&["b".repeat(64)]).unwrap_err();
        assert!(
            error.to_string().contains("neither attached to nor named"),
            "{error}"
        );
        pictured.operator_words = "Match the layout in `~/shots/mockup.png`.".into();
        assert!(pictured.check_provenance(&[]).is_ok());
        pictured.operator_words = "Match the layout in old-mockup.png".into();
        assert!(pictured.check_provenance(&[]).is_err());
        let mut specified = inbox(BRIEF, &["add focused tests"]);
        let spec = |path: &str| IntentArtifact {
            path: path.into(),
            sha256: digest.clone(),
        };
        specified.artifacts.push(spec("/work/SPEC.md"));
        assert!(specified.check_provenance(&[]).is_ok());
        specified.artifacts.push(spec("/work/design.html"));
        let error = specified.check_provenance(&[]).unwrap_err().to_string();
        assert!(error.contains("design.html is not named"), "{error}");
    }
}
