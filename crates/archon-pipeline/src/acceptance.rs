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

/// How every refusal of a result's `requirement_results` opens: a mistake
/// the stage can answer with a corrected result. A blocked requirement is not.
const RESULTS_REFUSED: &str = "approved intent: requirement_results refused: ";

/// The refusal of a build's `requirement_results` that a failed stage's
/// `error` reports, which one automatic repair may answer; `None` otherwise.
pub fn results_refusal(error: &str) -> Option<&str> {
    if !error.starts_with("stage `") {
        return None;
    }
    error
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(RESULTS_REFUSED))
}

/// Whether stage `stage_id` of `spec` finishes the build, so an item bound to
/// approved intent must report `requirement_results` in its result. Exactly
/// one stage does: the one holding the ship record, else the last work stage.
pub fn reports_requirements(spec: &PipelineSpec, stage_id: &str) -> bool {
    if let Some(record) = spec.ship_record_stage() {
        return record.id == stage_id;
    }
    crate::autonomy::ships(spec)
        && spec
            .file
            .stages
            .iter()
            .rev()
            .find(|s| s.kind == StageKind::Work)
            .is_some_and(|s| s.id == stage_id)
}

pub fn build_results(
    intent: &ApprovedIntent,
    binding: &IntentBinding,
    structured: Option<&Value>,
) -> Result<Vec<RequirementResult>, ArchonError> {
    let selected = intent.selected_requirements(binding)?;
    let value = structured
        .and_then(|s| s.get("requirement_results"))
        .ok_or_else(|| results_refused("missing; report every approved requirement"))?;
    let results: Vec<RequirementResult> = serde_json::from_value(canonical_statuses(value))
        .map_err(|e| {
            results_refused(&format!(
                "not a list of results: {e}; `status` is exactly one of {STATUSES}"
            ))
        })?;
    let indexed = indexed_results(&results)?;
    if !exact_ids(
        selected.iter().map(|r| r.id.as_str()),
        indexed.keys().copied(),
    ) {
        return Err(results_refused(EXACT_IDS));
    }
    for requirement in &selected {
        let result = indexed[requirement.id.as_str()];
        if result.evidence.trim().is_empty() {
            return Err(results_refused(&format!(
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
            return Err(results_refused("results have empty or duplicate ids"));
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
            .ok_or_else(|| results_refused(&format!("unknown reference {}", pair.reference)))?;
        if !requirement.images.contains(&pair.reference) || !seen.insert(pair.reference.as_str()) {
            return Err(results_refused(
                "visual pair references must belong to the requirement and be unique",
            ));
        }
        // A theme or state is owed only where the reference records one: a
        // live-design picture records its viewport alone.
        if pair.built.trim().is_empty()
            || pair.viewport.width == 0
            || pair.viewport.height == 0
            || (image.theme.is_some() && pair.theme.trim().is_empty())
            || (image.state.is_some() && pair.state.trim().is_empty())
        {
            return Err(results_refused(
                "visual pairs need a built image, a viewport, and the theme and state \
                 the reference records",
            ));
        }
        let differ = context_differences(image, pair);
        if !differ.is_empty() {
            return Err(results_refused(&format!(
                "{} reference and built {} differ: {}",
                result.id,
                differ
                    .iter()
                    .map(|(what, _)| *what)
                    .collect::<Vec<_>>()
                    .join("/"),
                differ
                    .iter()
                    .map(|(_, how)| how.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
    }
    if requirement.kind == RequirementKind::Visual && seen.len() != requirement.images.len() {
        return Err(results_refused(&format!(
            "{} needs a reference | built pair for every reference",
            result.id
        )));
    }
    assert!(seen.len() <= requirement.images.len());
    assert_eq!(seen.len(), result.visual_pairs.len());
    Ok(())
}

/// Where `pair` was taken in other conditions than its reference records,
/// each as (what, the reference's value against the built one), so a refusal
/// names exactly what to retake.
fn context_differences(
    image: &archon_core::intent::ReferenceImage,
    pair: &archon_core::acceptance::VisualPair,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(v) = image.viewport.as_ref().filter(|v| **v != pair.viewport) {
        out.push((
            "viewport",
            format!(
                "the reference is {}x{}, the built image {}x{}",
                v.width, v.height, pair.viewport.width, pair.viewport.height
            ),
        ));
    }
    if let Some(t) = image.theme.as_ref().filter(|t| **t != pair.theme) {
        out.push((
            "theme",
            format!("the reference theme is {t:?}, the built {:?}", pair.theme),
        ));
    }
    if let Some(s) = image.state.as_ref().filter(|s| **s != pair.state) {
        out.push((
            "state",
            format!("the reference state is {s:?}, the built {:?}", pair.state),
        ));
    }
    out
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
    if !exact_ids(
        selected.iter().map(|r| r.id.as_str()),
        result.requirements.iter().map(|r| r.id.as_str()),
    ) {
        return Err(invalid(EXACT_IDS));
    }
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

const EXACT_IDS: &str =
    "every selected requirement needs exactly one result; omissions, duplicates and extra ids fail";

/// Whether `actual` names each `expected` id exactly once and nothing else.
fn exact_ids<'a>(
    expected: impl Iterator<Item = &'a str>,
    actual: impl Iterator<Item = &'a str>,
) -> bool {
    let expected: BTreeSet<_> = expected.collect();
    let actual: Vec<_> = actual.collect();
    let unique: BTreeSet<_> = actual.iter().copied().collect();
    let exact = unique == expected && unique.len() == actual.len();
    assert!(!exact || expected.len() == actual.len());
    assert!(unique.len() <= actual.len());
    exact
}

fn invalid(message: &str) -> ArchonError {
    ArchonError::Invalid(format!("approved intent: {message}"))
}

/// A mistake in the stage's `requirement_results`, which a corrected result answers.
pub fn results_refused(message: &str) -> ArchonError {
    ArchonError::Invalid(format!("{RESULTS_REFUSED}{message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::intent::{IntentSource, ReferenceImage, Viewport};
    use serde_json::json;

    fn stage_failure(error: &str) -> String {
        format!(
            "stage `implement` failed: 1 of 1 agent(s) did not complete\n\n**`implement` #1: failed**\n\n{error}\n"
        )
    }

    #[test]
    fn every_mistake_in_requirement_results_is_repairable_but_blocked_work_is_not() {
        let (intent, binding) = fixture();
        let edit = |change: &dyn Fn(&mut Value)| {
            let mut value = implemented();
            change(&mut value["requirement_results"]);
            value
        };
        let mistakes = [
            json!({}),
            json!({"requirement_results": "all done"}),
            json!({"requirement_results": []}),
            edit(&|r| r[0]["evidence"] = json!(" ")),
            edit(&|r| r[0]["visual_pairs"] = json!([])),
            edit(&|r| r[0]["visual_pairs"][0]["reference"] = json!("other")),
            edit(&|r| r[0]["visual_pairs"][0]["built"] = json!("")),
            edit(&|r| r[0]["visual_pairs"][0]["theme"] = json!("dark")),
            edit(&|r| r[0]["id"] = json!("")),
            edit(&|r| r[0]["id"] = json!("R9")),
        ];
        for given in mistakes {
            let Err(ArchonError::Invalid(error)) = build_results(&intent, &binding, Some(&given))
            else {
                panic!("{given} is refused")
            };
            assert!(error.starts_with(RESULTS_REFUSED), "{error}");
            assert_eq!(
                results_refusal(&stage_failure(&error)),
                Some(error.as_str())
            );
            assert_eq!(
                results_refusal(&error),
                None,
                "only a stage failure is repaired"
            );
        }
        let blocked = edit(&|r| r[0]["status"] = json!("blocked"));
        let error = build_results(&intent, &binding, Some(&blocked))
            .unwrap_err()
            .to_string();
        assert!(error.contains("R1 is blocked"), "{error}");
        assert_eq!(results_refusal(&stage_failure(&error)), None);
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
    fn exactly_the_stage_holding_the_ship_record_reports_requirements() {
        let reporting = |spec: &PipelineSpec| -> Vec<String> {
            let spec = augment(spec).unwrap();
            let stages = spec.file.stages.iter();
            stages
                .filter(|s| reports_requirements(&spec, &s.id))
                .map(|s| s.id.clone())
                .collect()
        };
        let catalog = crate::Catalog::builtin();
        assert_eq!(reporting(catalog.get("adaptive").unwrap()), ["build"]);
        assert_eq!(reporting(catalog.get("refactor").unwrap()), ["refactor"]);
        assert!(reporting(catalog.get("e2e-strict").unwrap()).is_empty());
        let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
        let team = crate::catalog::load_dir(
            &examples.join("pipelines/team-pr"),
            archon_core::catalog::PipelineSource::User,
        )
        .unwrap();
        let record = team.ship_record_stage().unwrap();
        assert_eq!(
            (record.id.as_str(), record.kind),
            ("review", StageKind::Review)
        );
        assert_eq!(
            reporting(&team),
            ["review"],
            "not its last work stage, plan"
        );
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

    /// A live-design picture records its viewport alone: a pair with no (or
    /// a `null`) theme and state passes at that viewport, and one at another
    /// viewport is refused naming both sizes, never as a type error.
    #[test]
    fn a_viewport_only_reference_owes_only_its_viewport() {
        let (mut intent, binding) = fixture();
        intent.images[0].theme = None;
        intent.images[0].state = None;
        let mut data = implemented();
        let pair = &mut data["requirement_results"][0]["visual_pairs"][0];
        pair["theme"] = Value::Null;
        pair["state"] = Value::Null;
        let results = build_results(&intent, &binding, Some(&data)).unwrap();
        assert_eq!(results[0].visual_pairs[0].theme, "");
        let pair = &mut data["requirement_results"][0]["visual_pairs"][0];
        pair.as_object_mut().unwrap().remove("theme");
        pair.as_object_mut().unwrap().remove("state");
        assert!(build_results(&intent, &binding, Some(&data)).is_ok());
        data["requirement_results"][0]["visual_pairs"][0]["viewport"] =
            json!({"width": 390, "height": 844});
        let error = build_results(&intent, &binding, Some(&data))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("R1 reference and built viewport differ: the reference is 1200x800, the built image 390x844"),
            "{error}"
        );
        // A reference that records a theme still owes it: a null is refused
        // as a missing theme, which a repair can answer.
        let (intent, binding) = fixture();
        let mut data = implemented();
        data["requirement_results"][0]["visual_pairs"][0]["theme"] = Value::Null;
        let Err(ArchonError::Invalid(error)) = build_results(&intent, &binding, Some(&data)) else {
            panic!("a null theme is refused where the reference records one")
        };
        assert!(
            error.contains("the theme and state the reference records"),
            "{error}"
        );
        assert!(error.starts_with(RESULTS_REFUSED), "{error}");
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
