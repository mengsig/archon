//! Required ledger coverage and the independent acceptance stage.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::acceptance::{
    AcceptanceResult, AcceptanceVerdict, RequirementResult, RequirementStatus,
};
use archon_core::catalog::{Gate, StageKind, Tier};
use archon_core::error::ArchonError;
use archon_core::intent::{ApprovedIntent, IntentBinding, Requirement, RequirementKind};
use archon_core::model::Effort;
use serde_json::Value;

use crate::spec::{PipelineSpec, StageSpec};

pub const STAGE: &str = "intent-acceptance";
const PROMPT: &str = "archon-intent-acceptance.md";
const TEMPLATE: &str = include_str!("../../../pipelines/_shared/intent-acceptance.md");

pub fn augment(spec: &PipelineSpec) -> Result<PipelineSpec, ArchonError> {
    if spec.file.pipeline.workspace == archon_core::catalog::WorkspaceMode::Scratch {
        return Err(invalid(
            "acceptance needs a repository workspace to pin its head and tree",
        ));
    }
    if spec.stage(STAGE).is_some() {
        if spec.prompts.get(PROMPT).map(String::as_str) == Some(TEMPLATE) {
            return Ok(spec.clone());
        }
        return Err(ArchonError::Invalid(format!(
            "stage {STAGE} is reserved for approved intent"
        )));
    }
    let mut out = spec.clone();
    let at = out
        .file
        .stages
        .iter()
        .position(|s| matches!(s.kind, StageKind::Report | StageKind::Ship))
        .unwrap_or(out.file.stages.len());
    out.file.stages.insert(
        at,
        StageSpec {
            id: STAGE.into(),
            kind: StageKind::Test,
            prompt: PROMPT.into(),
            fan_out: 1,
            gate: Gate::AllPass,
            timeout_minutes: 45,
            effort: Effort::High,
            min_tier: Some(Tier::Balanced),
            tier: Some(Tier::Strong),
            allow_delegation: false,
            nudge: None,
            on_fail: None,
            roles: Vec::new(),
            role_prompts: BTreeMap::new(),
            roles_from: None,
            scenarios: Default::default(),
        },
    );
    out.prompts.insert(PROMPT.into(), TEMPLATE.into());
    out.validate(spec.name())?;
    assert_eq!(out.file.stages.len(), spec.file.stages.len() + 1);
    assert_eq!(out.file.stages[at].id, STAGE);
    Ok(out)
}

/// How the refusals of a build's `requirement_results` open, which the
/// stage can answer with a corrected result; a blocked requirement is not one.
const RESULTS_REFUSALS: &[&str] = &[
    "approved intent: missing requirement_results",
    "approved intent: invalid requirement_results",
    "approved intent: requirement results have empty or duplicate ids",
    "approved intent: every selected requirement needs exactly one result",
];

/// The refusal of a build's `requirement_results` that a failed stage's
/// `error` reports, which one automatic repair may answer; `None` otherwise.
pub fn results_refusal(error: &str) -> Option<&str> {
    if !error.starts_with("stage `") {
        return None;
    }
    error.lines().map(str::trim).find(|line| {
        RESULTS_REFUSALS
            .iter()
            .any(|opening| line.starts_with(opening))
    })
}

/// Whether stage `stage_id` of `spec` finishes the build, so an item bound to
/// approved intent must report `requirement_results` in its result.
pub fn reports_requirements(spec: &PipelineSpec, stage_id: &str) -> bool {
    let last_work = crate::autonomy::ships(spec)
        && spec
            .file
            .stages
            .iter()
            .rev()
            .find(|s| s.kind == StageKind::Work)
            .is_some_and(|s| s.id == stage_id);
    last_work || spec.ship_record_stage().is_some_and(|s| s.id == stage_id)
}

pub fn build_results(
    intent: &ApprovedIntent,
    binding: &IntentBinding,
    structured: Option<&Value>,
) -> Result<Vec<RequirementResult>, ArchonError> {
    let selected = intent.selected_requirements(binding)?;
    let value = structured
        .and_then(|s| s.get("requirement_results"))
        .ok_or_else(|| invalid("missing requirement_results: report every approved requirement"))?;
    let results: Vec<RequirementResult> = serde_json::from_value(canonical_statuses(value))
        .map_err(|e| {
            invalid(&format!(
                "invalid requirement_results: {e}; `status` is exactly one of {STATUSES}"
            ))
        })?;
    let indexed = indexed_results(&results)?;
    exact_ids(
        selected.iter().map(|r| r.id.as_str()),
        indexed.keys().copied(),
    )?;
    for requirement in &selected {
        let result = indexed[requirement.id.as_str()];
        if result.evidence.trim().is_empty() {
            return Err(invalid(&format!(
                "{} has no evidence or reason",
                requirement.id
            )));
        }
        if result.status == RequirementStatus::Blocked {
            return Err(invalid(&format!(
                "{} is blocked: {}",
                result.id, result.evidence
            )));
        }
        validate_pairs(intent, requirement, result)?;
    }
    assert_eq!(results.len(), selected.len());
    assert!(results.iter().all(|r| !r.evidence.trim().is_empty()));
    Ok(results)
}

/// The status vocabulary, as refusals and repair notes quote it.
pub const STATUSES: &str = "`implemented`, `deviation` or `blocked`";

/// Words an agent plainly means as `implemented`; anything else unknown,
/// such as `fail`, stays a refusal, since it may mean blocked or deviation.
const IMPLEMENTED_SYNONYMS: &[&str] = &["pass", "passed", "done", "met", "complete", "completed"];

/// `value` with each result's status in its canonical spelling: case and
/// surrounding space ignored, and plain synonyms of `implemented` mapped.
fn canonical_statuses(value: &Value) -> Value {
    let Some(results) = value.as_array() else {
        return value.clone();
    };
    let canonical: Vec<Value> = results
        .iter()
        .map(|result| {
            let mut result = result.clone();
            let status = result.get("status").and_then(Value::as_str);
            let Some(word) = status.map(|s| s.trim().to_ascii_lowercase()) else {
                return result;
            };
            let word = match IMPLEMENTED_SYNONYMS.contains(&word.as_str()) {
                true => "implemented".to_owned(),
                false => word,
            };
            result["status"] = Value::String(word);
            result
        })
        .collect();
    assert_eq!(canonical.len(), results.len());
    Value::Array(canonical)
}

fn indexed_results(
    results: &[RequirementResult],
) -> Result<BTreeMap<&str, &RequirementResult>, ArchonError> {
    let mut indexed = BTreeMap::new();
    for result in results {
        if result.id.is_empty() || indexed.insert(result.id.as_str(), result).is_some() {
            return Err(invalid("requirement results have empty or duplicate ids"));
        }
    }
    assert_eq!(indexed.len(), results.len());
    assert!(indexed.keys().all(|id| !id.is_empty()));
    Ok(indexed)
}

fn validate_pairs(
    intent: &ApprovedIntent,
    requirement: &Requirement,
    result: &RequirementResult,
) -> Result<(), ArchonError> {
    if requirement.kind == RequirementKind::Visual && requirement.images.is_empty() {
        return Err(invalid(&format!(
            "{} has no approved reference image",
            requirement.id
        )));
    }
    let mut seen = BTreeSet::new();
    for pair in &result.visual_pairs {
        let image = intent
            .images
            .iter()
            .find(|image| image.id == pair.reference)
            .ok_or_else(|| invalid(&format!("unknown reference {}", pair.reference)))?;
        if !requirement.images.contains(&pair.reference) || !seen.insert(pair.reference.as_str()) {
            return Err(invalid(
                "visual pair references must belong to the requirement and be unique",
            ));
        }
        if pair.built.trim().is_empty()
            || pair.viewport.width == 0
            || pair.viewport.height == 0
            || pair.theme.trim().is_empty()
            || pair.state.trim().is_empty()
        {
            return Err(invalid(
                "visual pairs need a built image, viewport, theme and state",
            ));
        }
        if image.viewport.as_ref().is_some_and(|v| v != &pair.viewport)
            || image.theme.as_ref().is_some_and(|t| t != &pair.theme)
            || image.state.as_ref().is_some_and(|s| s != &pair.state)
        {
            return Err(invalid(&format!(
                "{} reference and built viewport/theme/state differ",
                result.id
            )));
        }
    }
    if requirement.kind == RequirementKind::Visual && seen.len() != requirement.images.len() {
        return Err(invalid(&format!(
            "{} needs a reference | built pair for every reference",
            result.id
        )));
    }
    assert!(seen.len() <= requirement.images.len());
    assert_eq!(seen.len(), result.visual_pairs.len());
    Ok(())
}

pub fn judge(
    intent: &ApprovedIntent,
    binding: &IntentBinding,
    structured: Option<&Value>,
    head: &str,
    tree: &str,
) -> Result<AcceptanceResult, ArchonError> {
    let selected = intent.selected_requirements(binding)?;
    let value = structured
        .and_then(|s| s.get("acceptance"))
        .ok_or_else(|| invalid("acceptance stage omitted its per-requirement judgment"))?;
    let result: AcceptanceResult = serde_json::from_value(value.clone())
        .map_err(|e| invalid(&format!("invalid acceptance result: {e}")))?;
    if result.intent != *binding || result.head != head || result.tree != tree {
        return Err(invalid(
            "acceptance is not pinned to the current head, tree and intent version",
        ));
    }
    exact_ids(
        selected.iter().map(|r| r.id.as_str()),
        result.requirements.iter().map(|r| r.id.as_str()),
    )?;
    if let Some(failed) = result
        .requirements
        .iter()
        .find(|r| r.verdict != AcceptanceVerdict::Pass || r.reason.trim().is_empty())
    {
        return Err(invalid(&format!(
            "{} failed acceptance: {}",
            failed.id, failed.reason
        )));
    }
    assert!(!head.is_empty() && !tree.is_empty());
    assert_eq!(result.requirements.len(), selected.len());
    Ok(result)
}

fn exact_ids<'a>(
    expected: impl Iterator<Item = &'a str>,
    actual: impl Iterator<Item = &'a str>,
) -> Result<(), ArchonError> {
    let expected: BTreeSet<_> = expected.collect();
    let actual: Vec<_> = actual.collect();
    let unique: BTreeSet<_> = actual.iter().copied().collect();
    if unique != expected || unique.len() != actual.len() {
        return Err(invalid(
            "every selected requirement needs exactly one result; omissions, duplicates and extra ids fail",
        ));
    }
    assert_eq!(expected.len(), actual.len());
    assert_eq!(expected, unique);
    Ok(())
}

fn invalid(message: &str) -> ArchonError {
    ArchonError::Invalid(format!("approved intent: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, ReferenceImage, Viewport};
    use serde_json::json;

    #[test]
    fn only_a_stage_failure_over_its_requirement_results_is_repairable() {
        let ArchonError::Invalid(missing) =
            invalid("missing requirement_results: report every approved requirement")
        else {
            unreachable!()
        };
        let error = format!(
            "stage `implement` failed: 1 of 1 agent(s) did not complete\n\n**`implement` #1: failed**\n\n{missing}\n"
        );
        assert_eq!(
            results_refusal(&error),
            Some("approved intent: missing requirement_results: report every approved requirement")
        );
        let blocked = error.replace("missing requirement_results", "R1 is blocked: no API key;");
        assert_eq!(results_refusal(&blocked), None);
        assert_eq!(results_refusal(&missing), None);
    }

    fn fixture() -> (ApprovedIntent, IntentBinding) {
        let intent = ApprovedIntent {
            id: "intent_1".into(),
            version: 1,
            operator_words: "Keep the approved labels and layout".into(),
            source: IntentSource::Inbox { seq: 7 },
            artifacts: vec![],
            images: vec![ReferenceImage {
                id: "mockup".into(),
                path: "/mockup.png".into(),
                sha256: "a".repeat(64),
                viewport: Some(Viewport {
                    width: 1200,
                    height: 800,
                }),
                theme: Some("light".into()),
                state: Some("loaded".into()),
            }],
            requirements: vec![Requirement {
                id: "R1".into(),
                text: "Keep ISO labels".into(),
                kind: RequirementKind::Visual,
                images: vec!["mockup".into()],
                owner: "app".into(),
            }],
        };
        let binding = IntentBinding {
            id: intent.id.clone(),
            version: 1,
            requirements: vec![],
        };
        (intent, binding)
    }

    fn implemented() -> Value {
        json!({"requirement_results":[{"id":"R1","status":"implemented","evidence":"ISO labels visible",
            "visual_pairs":[{"reference":"mockup","built":".archon/evidence/built.png","viewport":{"width":1200,"height":800},"theme":"light","state":"loaded"}]}]})
    }

    #[test]
    fn plain_synonyms_of_implemented_map_and_other_statuses_are_repairable() {
        let (intent, binding) = fixture();
        for word in ["pass", "Done", " met ", "IMPLEMENTED"] {
            let mut given = implemented();
            given["requirement_results"][0]["status"] = json!(word);
            let results = build_results(&intent, &binding, Some(&given)).unwrap();
            assert_eq!(results[0].status, RequirementStatus::Implemented, "{word}");
        }
        let mut failed = implemented();
        failed["requirement_results"][0]["status"] = json!("fail");
        let Err(ArchonError::Invalid(error)) = build_results(&intent, &binding, Some(&failed))
        else {
            panic!("an ambiguous status is refused")
        };
        assert!(error.contains(STATUSES), "{error}");
        let stage =
            format!("stage `implement` failed: 1 of 1 agent(s) did not complete\n\n{error}");
        assert_eq!(results_refusal(&stage), Some(error.as_str()));
    }

    #[test]
    fn built_in_work_and_validation_pipelines_gain_exactly_one_gate() {
        let catalog = crate::Catalog::builtin();
        for name in [
            "adaptive",
            "direct",
            "swarm",
            "refactor",
            "performance",
            "e2e-strict",
            "review",
        ] {
            let original = catalog.get(name).unwrap();
            let added = augment(original).unwrap();
            assert_eq!(
                added.file.stages.iter().filter(|s| s.id == STAGE).count(),
                1
            );
            assert_eq!(augment(&added).unwrap().file.stages, added.file.stages);
            let at = added.stage_index(STAGE).unwrap();
            assert!(
                added.file.stages[at + 1..]
                    .iter()
                    .all(|s| !matches!(s.kind, StageKind::Work | StageKind::Team))
            );
        }
        assert!(augment(catalog.get("errand").unwrap()).is_err());
    }

    #[test]
    fn every_requirement_and_visual_pair_is_mandatory() {
        let (intent, binding) = fixture();
        let valid = implemented();
        assert!(build_results(&intent, &binding, Some(&valid)).is_ok());
        for value in [
            json!({}),
            json!({"requirement_results":[]}),
            json!({"requirement_results":[{"id":"R1","status":"implemented","evidence":"done"}]}),
        ] {
            assert!(
                build_results(&intent, &binding, Some(&value)).is_err(),
                "{value}"
            );
        }
        let mut duplicate = valid.clone();
        let rows = duplicate["requirement_results"].as_array_mut().unwrap();
        rows.push(rows[0].clone());
        assert!(build_results(&intent, &binding, Some(&duplicate)).is_err());
    }

    #[test]
    fn evidence_must_match_reference_context_and_blocked_work_fails() {
        let (intent, binding) = fixture();
        for (key, value) in [
            ("theme", json!("dark")),
            ("state", json!("empty")),
            ("viewport", json!({"width":800,"height":600})),
        ] {
            let mut data = implemented();
            data["requirement_results"][0]["visual_pairs"][0][key] = value;
            assert!(build_results(&intent, &binding, Some(&data)).is_err());
        }
        let mut data = implemented();
        data["requirement_results"][0]["status"] = json!("blocked");
        assert!(
            build_results(&intent, &binding, Some(&data))
                .unwrap_err()
                .to_string()
                .contains("blocked")
        );
        data["requirement_results"][0]["status"] = json!("deviation");
        assert!(
            build_results(&intent, &binding, Some(&data)).is_ok(),
            "runtime must authorize the deviation separately"
        );
    }

    #[test]
    fn independent_gate_rejects_gaps_and_stale_pins() {
        let (intent, binding) = fixture();
        let valid = json!({"acceptance":{"head":"head","tree":"tree","intent":binding,"requirements":[{"id":"R1","verdict":"pass","reason":"opened both images; labels agree"}]}});
        assert!(judge(&intent, &binding, Some(&valid), "head", "tree").is_ok());
        assert!(judge(&intent, &binding, Some(&valid), "new-head", "tree").is_err());
        assert!(judge(&intent, &binding, Some(&valid), "head", "new-tree").is_err());
        let mut failed = valid.clone();
        failed["acceptance"]["requirements"][0]["verdict"] = json!("fail");
        failed["acceptance"]["requirements"][0]["reason"] = json!("ISO labels missing");
        assert!(judge(&intent, &binding, Some(&failed), "head", "tree").is_err());
        failed["acceptance"]["requirements"] = json!([]);
        assert!(judge(&intent, &binding, Some(&failed), "head", "tree").is_err());
    }
}
