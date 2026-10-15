//! Model selection vocabulary shared with Aether: a provider-qualified model
//! and a reasoning effort. Serialized exactly as Aether's protocol does, so
//! the same value can be handed to `create_agent` unchanged.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// `provider/model`, e.g. `claude-code/opus` or `codex/astra`. Serializes as
/// Aether's `{provider, model}` object; also deserializes from the string form
/// so config files and MCP arguments can write `"claude-code/opus"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let this = Self {
            provider: provider.into(),
            model: model.into(),
        };
        debug_assert!(!this.provider.is_empty() && !this.model.is_empty());
        this
    }

    /// Native harness providers whose agents run on the official CLIs.
    pub fn is_native_harness(&self) -> bool {
        matches!(self.provider.as_str(), "claude-code" | "codex")
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

impl FromStr for ModelRef {
    type Err = ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((provider, model)) = s.split_once('/') else {
            return Err(ArchonError::Invalid(format!(
                "model {s:?} must be written provider/model"
            )));
        };
        if provider.is_empty() || model.is_empty() {
            return Err(ArchonError::Invalid(format!(
                "model {s:?} has an empty provider or model"
            )));
        }
        Ok(Self::new(provider, model))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ModelRefWire {
    Text(String),
    Object { provider: String, model: String },
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        match ModelRefWire::deserialize(de)? {
            ModelRefWire::Text(text) => text.parse().map_err(serde::de::Error::custom),
            ModelRefWire::Object { provider, model } => format!("{provider}/{model}")
                .parse()
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Aether's effort scale; the wire spelling is snake_case (`x_high`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    #[serde(alias = "xhigh")]
    XHigh,
    Max,
}

impl FromStr for Effort {
    type Err = ArchonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "x_high" | "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(ArchonError::Invalid(format!(
                "effort {other:?} is not one of low, medium, high, x_high, max"
            ))),
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "x_high",
            Self::Max => "max",
        };
        f.write_str(s)
    }
}

/// What the orchestrator chose for one agent: both fields are mandatory,
/// mirroring Aether's `delegate`, which inherits neither.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub model: ModelRef,
    pub effort: Effort,
}

/// A [`ModelChoice`] as a request gives it: `effort` may be left out, and the
/// stage's default effort stands in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoiceDraft {
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl From<ModelChoice> for ModelChoiceDraft {
    fn from(choice: ModelChoice) -> Self {
        Self {
            model: choice.model,
            effort: Some(choice.effort),
        }
    }
}

/// A [`ModelPlan`] as a request gives it, keyed like it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelPlanDraft(pub std::collections::BTreeMap<String, Vec<ModelChoiceDraft>>);

impl From<ModelPlan> for ModelPlanDraft {
    fn from(plan: ModelPlan) -> Self {
        Self(
            plan.0
                .into_iter()
                .map(|(stage, choices)| (stage, choices.into_iter().map(Into::into).collect()))
                .collect(),
        )
    }
}

impl ModelPlanDraft {
    /// The plan with each missing effort set to `default_for(key)`; `None`
    /// from it names a key the caller already found to be no stage.
    pub fn resolve(self, default_for: impl Fn(&str) -> Option<Effort>) -> ModelPlan {
        let resolve = |stage: &str, draft: ModelChoiceDraft| {
            let effort = draft
                .effort
                .or_else(|| default_for(stage))
                .unwrap_or(Effort::Medium);
            ModelChoice {
                model: draft.model,
                effort,
            }
        };
        ModelPlan(
            self.0
                .into_iter()
                .map(|(stage, drafts)| {
                    let choices = drafts.into_iter().map(|d| resolve(&stage, d)).collect();
                    (stage, choices)
                })
                .collect(),
        )
    }
}

/// Optional explanations of the orchestrator's per-stage choices.
pub type ModelReasons = std::collections::BTreeMap<String, String>;

pub const MAX_MODEL_REASON_CHARS: usize = 300;

/// Length is checked at both the MCP and daemon boundaries.
pub fn check_reason_lengths(reasons: &ModelReasons) -> Result<(), crate::ArchonError> {
    for (stage, reason) in reasons {
        if reason.chars().count() > MAX_MODEL_REASON_CHARS || reason.contains(['\n', '\r']) {
            return Err(crate::ArchonError::Invalid(format!(
                "model_reasons stage {stage:?} must be one line of at most {MAX_MODEL_REASON_CHARS} characters"
            )));
        }
    }
    Ok(())
}

/// Per-stage choices for one pipeline run, keyed by stage id. A fan-out
/// stage lists one choice per parallel agent; every other stage lists one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelPlan(pub std::collections::BTreeMap<String, Vec<ModelChoice>>);

/// Ends the `model_plan` and `model_reasons` key (`<stage>.review`) naming the
/// reviewers a stage delegates; stage ids hold no `.`.
pub const REVIEWERS_KEY_SUFFIX: &str = ".review";

/// The key naming the reviewers `stage` delegates.
pub fn reviewers_key(stage: &str) -> String {
    format!("{stage}{REVIEWERS_KEY_SUFFIX}")
}

impl ModelPlan {
    /// The models the plan lets delegated reviewers run on; `None` leaves them free.
    pub fn reviewer_choices(&self) -> Option<&[ModelChoice]> {
        self.0
            .iter()
            .find(|(key, _)| key.ends_with(REVIEWERS_KEY_SUFFIX))
            .map(|(_, choices)| choices.as_slice())
    }

    pub fn choices_for(&self, stage_id: &str) -> Option<&[ModelChoice]> {
        self.0.get(stage_id).map(Vec::as_slice)
    }

    /// Distinct providers named anywhere in the plan; a strict pipeline's
    /// cross-vendor nudge is checked against this.
    pub fn providers(&self) -> std::collections::BTreeSet<&str> {
        self.0
            .values()
            .flatten()
            .map(|c| c.model.provider.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choices_reject_unknown_keys_with_the_key_named() {
        let error = serde_json::from_value::<ModelChoice>(serde_json::json!({
            "model": "codex/test", "effort": "high", "effrot": "low"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("effrot"), "{error}");
    }

    #[test]
    fn parses_and_prints_model_refs() {
        let m: ModelRef = "claude-code/opus".parse().unwrap();
        assert_eq!(m.provider, "claude-code");
        assert_eq!(m.to_string(), "claude-code/opus");
        assert!(m.is_native_harness());
        assert!("opus".parse::<ModelRef>().is_err());
        assert!("/opus".parse::<ModelRef>().is_err());
    }

    #[test]
    fn model_refs_deserialize_from_string_or_object() {
        let from_text: ModelRef = serde_json::from_str("\"codex/astra\"").unwrap();
        let from_object: ModelRef =
            serde_json::from_str(r#"{"provider":"codex","model":"astra"}"#).unwrap();
        assert_eq!(from_text, from_object);
        assert_eq!(
            serde_json::to_value(&from_text).unwrap(),
            serde_json::json!({"provider": "codex", "model": "astra"})
        );
        assert!(serde_json::from_str::<ModelRef>("\"astra\"").is_err());
    }

    #[test]
    fn a_plan_finds_the_reviewers_it_names() {
        let choice = |model: &str| ModelChoice {
            model: model.parse().unwrap(),
            effort: Effort::Low,
        };
        let mut plan = ModelPlan::default();
        plan.0.insert("implement".into(), vec![choice("a/one")]);
        assert_eq!(plan.reviewer_choices(), None);
        plan.0
            .insert(reviewers_key("implement"), vec![choice("b/two")]);
        assert_eq!(plan.reviewer_choices(), Some(&[choice("b/two")][..]));
    }

    #[test]
    fn effort_matches_aether_wire_spelling() {
        assert_eq!(serde_json::to_string(&Effort::XHigh).unwrap(), "\"x_high\"");
        assert_eq!("xhigh".parse::<Effort>().unwrap(), Effort::XHigh);
        assert!("extreme".parse::<Effort>().is_err());
    }
}
