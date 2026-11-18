use crate::context::DaemonContext;
use crate::resolve::RepoEnv;
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::intent::ApprovedIntent;
use archon_core::model::ModelRef;
use archon_core::ports::SpawnRequest;
use archon_core::queue::QueueItem;
use archon_core::reference::{Reference, ReferenceKind};
use archon_core::run::{PipelineRun, StageRun};
use archon_pipeline::{SpawnPlan, acceptance::STAGE};
use archon_queue::Store;

pub(in crate::executor) fn section(
    store: &Store,
    run: &PipelineRun,
    item: &QueueItem,
) -> Result<String, ArchonError> {
    let Some(record) = super::intent(store, item)? else {
        return Ok(String::new());
    };
    let binding = item.intent.as_deref().expect("intent found");
    let record = materialize(&run.workspace, record)?;
    let selected = record.selected_requirements(binding)?;
    let mut text = format!(
        "\n## Approved intent (binding acceptance)\n\nOperator's verbatim words:\n{}\n\nBinding: {}\nLedger: {}\nReference images (open every image): {}\n",
        record.operator_words,
        serde_json::to_string(binding)?,
        serde_json::to_string_pretty(&selected)?,
        serde_json::to_string_pretty(&record.images)?
    );
    text.push_str(&format!(
        "\nApproved artifacts: {}\n",
        serde_json::to_string_pretty(&record.artifacts)?
    ));
    text.push_str(RULES);
    if let Some(raw) = store.kv_get(&format!("intent_build:{}", run.id))? {
        text.push_str(&format!("\nBuild evidence on record:\n{raw}\n"));
    }
    text.push_str(&super::deviations::section(store, run)?);
    assert!(text.contains(&record.operator_words));
    assert!(selected.iter().all(|r| text.contains(&r.id)));
    Ok(text)
}

/// Whether `stage_id` is the acceptance stage of a run no stage recorded build
/// evidence for, as in a validation-only pipeline: it reports the results itself.
pub(in crate::executor) fn observes_requirements(
    store: &Store,
    run: &PipelineRun,
    stage_id: &str,
) -> Result<bool, ArchonError> {
    if stage_id != STAGE {
        return Ok(false);
    }
    let evidence = store.kv_get(&format!("intent_build:{}", run.id))?;
    Ok(evidence.is_none())
}

/// Attach the approved references to a stage's request and, to the stage that
/// reports the requirement results (`reports`), the exact shape it owes.
pub(in crate::executor) fn attach(
    store: &Store,
    item: &QueueItem,
    request: &mut SpawnRequest,
    (supports, reports): (bool, bool),
) -> Result<(), ArchonError> {
    let Some(record) = super::intent(store, item)? else {
        return Ok(());
    };
    if reports {
        let binding = item.intent.as_deref().expect("intent found");
        let owed = results_owed(&record.selected_requirements(binding)?)?;
        request.instructions.push_str(&owed);
    }
    let record = materialize(&request.workspace, record)?;
    if !supports {
        request
            .instructions
            .push_str("\nAether lacks references support: images are listed by path above. ");
        request.instructions.push_str(PATH_REFERENCES);
        return Ok(());
    }
    attach_references(&record, request)?;
    assert!(request.references.iter().all(|r| r.path.is_file()));
    assert!(request.references.iter().all(|r| r.label.is_some()));
    Ok(())
}

// Aether v1.4.0 aether-core/src/reference.rs: MAX_REFERENCES and MAX_REFERENCE_IMAGES.
const MAX_REFERENCES: usize = 16;
const MAX_REFERENCE_IMAGES: usize = 8;
const PATH_REFERENCES: &str = "Open them, and explicitly pass these paths and the unchanged ledger to every worker and reviewer.\n";

fn attach_references(
    record: &ApprovedIntent,
    request: &mut SpawnRequest,
) -> Result<(), ArchonError> {
    let files = record
        .artifacts
        .len()
        .min(MAX_REFERENCES.saturating_sub(request.references.len()));
    request
        .references
        .extend(record.artifacts[..files].iter().map(|artifact| Reference {
            path: artifact.path.clone(),
            kind: ReferenceKind::File,
            label: Some("Approved artifact".into()),
        }));
    let existing_images = request
        .references
        .iter()
        .filter(|r| r.kind == ReferenceKind::Image)
        .count();
    let images = record
        .images
        .len()
        .min(MAX_REFERENCES.saturating_sub(request.references.len()))
        .min(MAX_REFERENCE_IMAGES.saturating_sub(existing_images));
    for (index, image) in record.images.iter().enumerate() {
        if super::images::hash(&image.path)? != image.sha256 {
            return Err(ArchonError::Invalid(format!(
                "approved image {} changed",
                image.id
            )));
        }
        if index < images {
            request.references.push(Reference {
                path: image.path.clone(),
                kind: ReferenceKind::Image,
                label: Some(image.id.clone()),
            });
        }
    }
    if files < record.artifacts.len() || images < record.images.len() {
        request.instructions.push_str(&format!(
            "\nAether reference limits reached: remaining approved artifacts and images are listed by path below. {}\nApproved artifacts (path only): {}\nReference images (path only): {}\n",
            PATH_REFERENCES,
            serde_json::to_string_pretty(&record.artifacts[files..])?,
            serde_json::to_string_pretty(&record.images[images..])?,
        ));
    }
    assert!(request.references.len() <= MAX_REFERENCES);
    assert!(existing_images + images <= MAX_REFERENCE_IMAGES);
    Ok(())
}

/// Place the acceptance stage of `plans` on the model that judges the ledger:
/// independent of every builder where one is permitted and connected, else,
/// unless `[review] independent_acceptance` holds, the strongest one, labelled.
pub(in crate::executor) fn select_model(
    ctx: &DaemonContext,
    store: &mut Store,
    item: &QueueItem,
    run: &PipelineRun,
    plans: &mut [SpawnPlan],
) -> Result<(), ArchonError> {
    if !plans.iter().any(|p| p.stage_id == STAGE) {
        return Ok(());
    }
    super::records::invalidate(store, &item.id)?;
    let visual = super::intent(store, item)?.is_some_and(|r| !r.images.is_empty());
    let builders = builder_models(&store.stage_runs(&run.id)?);
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&crate::resolve::repo_ref(id)))
        .transpose()?;
    let policy = RepoEnv::new(repo)?.resolved_policy(ctx);
    let vision = vision_models(store)?;
    let catalog = store.model_catalog();
    let permitted: Vec<_> = catalog
        .candidates()
        .filter(|m| policy.permits_model(&m.model) && (!visual || vision.contains(&m.model)))
        .collect();
    let model = judge_model(
        &permitted,
        &builders,
        visual,
        ctx.config.review.independent_acceptance,
    )?;
    for plan in plans.iter_mut().filter(|p| p.stage_id == STAGE) {
        plan.choice.model = model.clone();
        if let Some(cap) = policy.max_effort {
            plan.choice.effort = plan.choice.effort.min(cap);
        }
        plan.choice.tier = None;
        plan.allow_delegation = false;
        plan.keep = false;
    }
    assert!(!visual || vision.contains(&model));
    assert!(policy.permits_model(&model));
    Ok(())
}

/// The model apart from every builder that prefers another vendor and then
/// the strongest tier; with none, the strongest permitted one unless `strict`.
fn judge_model(
    permitted: &[&archon_core::model_tier::CatalogModel],
    builders: &[ModelRef],
    visual: bool,
    strict: bool,
) -> Result<ModelRef, ArchonError> {
    let shares_vendor = |m: &ModelRef| {
        builders
            .iter()
            .any(|b| vendor(&b.provider) == vendor(&m.provider))
    };
    let independent = permitted
        .iter()
        .filter(|m| !builders.contains(&m.model))
        .min_by_key(|m| (shares_vendor(&m.model), std::cmp::Reverse(m.tier)));
    if let Some(found) = independent {
        return Ok(found.model.clone());
    }
    let judged = if visual {
        "approved reference images"
    } else {
        "the approved ledger"
    };
    if strict {
        return Err(ArchonError::Invalid(format!(
            "independent acceptance of {judged} needs a permitted, connected {}model different \
             from every builder ([review] independent_acceptance = true)",
            if visual { "vision " } else { "" }
        )));
    }
    let strongest = permitted.iter().max_by_key(|m| m.tier).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "acceptance of {judged} needs a permitted, connected {}model",
            if visual { "vision " } else { "" }
        ))
    })?;
    assert!(
        builders.contains(&strongest.model),
        "only a builder's model is a fallback"
    );
    Ok(strongest.model.clone())
}

fn vision_models(store: &Store) -> Result<Vec<ModelRef>, ArchonError> {
    store
        .kv_get("acceptance_vision_models")?
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|e| ArchonError::Corrupt(format!("vision model catalog: {e}")))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn vendor(provider: &str) -> &str {
    match provider {
        "codex" | "openai" => "openai",
        "claude-code" | "anthropic" => "anthropic",
        other => other,
    }
}

pub(super) fn builder_models(stages: &[StageRun]) -> Vec<ModelRef> {
    stages
        .iter()
        .filter(|s| matches!(s.kind, Some(StageKind::Work | StageKind::Team)))
        .map(|s| s.choice.model.clone())
        .collect()
}

/// Whether the acceptance `stage` judged apart from every builder. Refused
/// when it could not see the approved images, or under the strict knob when
/// it was not independent.
pub(super) async fn check_judge(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    visual: bool,
) -> Result<bool, ArchonError> {
    let (stages, vision) = {
        let store = ctx.store.lock().await;
        (store.stage_runs(&run.id)?, vision_models(&store)?)
    };
    let independent = !builder_models(&stages).contains(&stage.choice.model);
    if !independent && ctx.config.review.independent_acceptance {
        return Err(ArchonError::Invalid(
            "acceptance must use a different model from the builder".into(),
        ));
    }
    if visual && !vision.contains(&stage.choice.model) {
        return Err(ArchonError::Invalid(
            "acceptance model has no confirmed vision capability".into(),
        ));
    }
    assert_eq!(stage.stage_id, STAGE);
    assert!(stage.aether_agent.is_some());
    Ok(independent)
}

const RULES: &str = r#"
Keep the complete selected ledger in every worker and reviewer brief. Never shrink
acceptance to match what you built. Set inherit_references=true when delegating on
an Aether that supports references, and pass all path-only references in text.
Inherited references count toward Aether's limits: 16 total, including at most 8
images. Do not reattach inherited references; list any additional references that
do not fit by path. Require every worker and reviewer to open all listed images
and pass these paths and the unchanged ledger to their own delegates.
Otherwise pass all image paths in text and require children to open them.

The stage that finishes the build returns `requirement_results`, one entry per
selected requirement: {id, status: implemented|blocked|deviation, evidence,
visual_pairs: []}; its contract shows the exact shape. Every visual requirement needs
one pair per reference: {reference: <image id>, built: <workspace image path>,
viewport: {width,height}, theme, state}. Match the approved viewport, theme and
state. Evidence must exist, and images must remain unchanged through acceptance.
Blocked requirements fail the build.

A deviation needs a reason and the reference | proposed pair. Ask using
message_orchestrator kind=question when it matters. The default asks the operator
with pictures. An active operator directive authorizing proceeding without asking
permits a recorded deviation (optionally name proceed_directive); never wait under
such a directive. All decisions appear on the deviations to review page.

Code review and CI do not establish acceptance. Only the independent acceptance
stage can pass intent on the current head, tree and intent version. A new head
needs acceptance again, even when its tree is unchanged.
"#;

/// The section telling the stage that finishes the build exactly which
/// `requirement_results` its result must carry, one skeleton entry per id.
pub(in crate::executor) fn results_owed(
    selected: &[&archon_core::intent::Requirement],
) -> Result<String, ArchonError> {
    assert!(
        !selected.is_empty(),
        "a binding selects at least one requirement"
    );
    let entries: Vec<_> = selected
        .iter()
        .map(|r| {
            let pairs: Vec<_> = r
                .images
                .iter()
                .map(|image| {
                    serde_json::json!({
                        "reference": image, "built": "<workspace path of the built image>",
                        "viewport": {"width": "<px>", "height": "<px>"},
                        "theme": "<theme>", "state": "<state>"
                    })
                })
                .collect();
            serde_json::json!({"id": r.id, "status": "implemented",
                "evidence": "<the test, file or check that shows it>", "visual_pairs": pairs})
        })
        .collect();
    let shape = serde_json::to_string_pretty(&serde_json::json!({"requirement_results": entries}))?;
    let text = format!(
        "\n## Requirement results this stage owes\n\nThis stage reports the requirement results \
         of an approved intent. Archon refuses your result unless `complete_task`'s `structured` holds \
         `requirement_results` beside any other keys it asks for: exactly one entry per \
         requirement below, no more and no fewer. `status` is exactly one of `implemented`, \
         `deviation` (give the reason in `evidence`) or `blocked` (fails the build), never \
         `pass` or `fail`. `evidence` is never empty. A \
         visual requirement keeps one `visual_pairs` entry per reference image; every other \
         keeps `[]`.\n\n```json\n{shape}\n```\n"
    );
    assert!(selected.iter().all(|r| text.contains(&r.id)));
    assert!(text.contains("\"requirement_results\""));
    Ok(text)
}

fn materialize(
    workspace: &std::path::Path,
    mut record: archon_core::intent::ApprovedIntent,
) -> Result<archon_core::intent::ApprovedIntent, ArchonError> {
    record.validate()?;
    let root = workspace
        .join(".archon/intent")
        .join(&record.id)
        .join(record.version.to_string());
    super::pages::directory_in_workspace(workspace, &root)?;
    let canonical_root = root
        .canonicalize()
        .map_err(|e| ArchonError::io("resolve stage intent directory", e))?;
    if !canonical_root.starts_with(
        workspace
            .canonicalize()
            .map_err(|e| ArchonError::io("resolve stage workspace", e))?,
    ) {
        return Err(ArchonError::Invalid(
            "intent image directory resolves outside the workspace".into(),
        ));
    }
    for artifact in &mut record.artifacts {
        artifact.path = crate::handlers::intent::preserve(&root, &artifact.path, &artifact.sha256)?;
    }
    for image in &mut record.images {
        if super::images::hash(&image.path)? != image.sha256 {
            return Err(ArchonError::Invalid(format!(
                "approved image {} changed",
                image.id
            )));
        }
        let filename = format!(
            "{}.{}",
            image.id,
            image
                .path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("png")
        );
        let target = root.join(filename);
        if target
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(ArchonError::Invalid(
                "intent image target is a symlink".into(),
            ));
        }
        std::fs::copy(&image.path, &target)
            .map_err(|e| ArchonError::io("copy stage reference image", e))?;
        image.path = target;
    }
    assert!(record.images.iter().all(|i| i.path.starts_with(&root)));
    assert!(record.images.iter().all(|i| i.path.is_file()));
    Ok(record)
}
