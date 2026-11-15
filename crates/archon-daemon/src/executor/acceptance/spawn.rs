use crate::context::DaemonContext;
use crate::resolve::RepoEnv;
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
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

pub(in crate::executor) fn attach(
    store: &Store,
    item: &QueueItem,
    request: &mut SpawnRequest,
    supports: bool,
) -> Result<(), ArchonError> {
    let Some(record) = super::intent(store, item)? else {
        return Ok(());
    };
    let record = materialize(&request.workspace, record)?;
    if !supports {
        request.instructions.push_str("\nAether lacks references support: images are listed by path above. Open them, and explicitly pass these paths and the unchanged ledger to every worker and reviewer.\n");
        return Ok(());
    }
    for artifact in record.artifacts {
        request.references.push(Reference {
            path: artifact.path,
            kind: ReferenceKind::File,
            label: Some("Approved artifact".into()),
        });
    }
    for image in record.images {
        if super::images::hash(&image.path)? != image.sha256 {
            return Err(ArchonError::Invalid(format!(
                "approved image {} changed",
                image.id
            )));
        }
        request.references.push(Reference {
            path: image.path,
            kind: ReferenceKind::Image,
            label: Some(image.id),
        });
    }
    assert!(request.references.iter().all(|r| r.path.is_file()));
    assert!(request.references.iter().all(|r| r.label.is_some()));
    Ok(())
}

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
    let catalog = store.model_catalog();
    let vision = vision_models(store)?;
    let builders = builder_models(&store.stage_runs(&run.id)?);
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&crate::resolve::repo_ref(id)))
        .transpose()?;
    let env = RepoEnv::new(repo)?;
    let policy = env.policy(&ctx.config);
    let mut candidates: Vec<_> = catalog
        .models()
        .iter()
        .filter(|m| {
            vision.contains(&m.model)
                && !builders.contains(&m.model)
                && policy.permits_model(&m.model)
        })
        .collect();
    candidates.sort_by_key(|m| {
        (
            builders
                .iter()
                .any(|b| vendor(&b.provider) == vendor(&m.model.provider)),
            std::cmp::Reverse(m.tier),
        )
    });
    let model = candidates.first().ok_or_else(|| ArchonError::Invalid("independent acceptance needs a permitted, connected vision model different from every builder".into()))?;
    for plan in plans.iter_mut().filter(|p| p.stage_id == STAGE) {
        plan.choice.model = model.model.clone();
        if let Some(cap) = policy.max_effort {
            plan.choice.effort = plan.choice.effort.min(cap);
        }
        plan.choice.tier = None;
        plan.allow_delegation = false;
        plan.keep = false;
    }
    assert!(!builders.contains(&model.model));
    assert!(vision.contains(&model.model));
    Ok(())
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

fn builder_models(stages: &[StageRun]) -> Vec<ModelRef> {
    stages
        .iter()
        .filter(|s| matches!(s.kind, Some(StageKind::Work | StageKind::Team)))
        .map(|s| s.choice.model.clone())
        .collect()
}

pub(super) async fn check_independent(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
) -> Result<(), ArchonError> {
    let stages = ctx.store.lock().await.stage_runs(&run.id)?;
    let builders = builder_models(&stages);
    if builders.contains(&stage.choice.model) {
        return Err(ArchonError::Invalid(
            "acceptance must use a different model from the builder".into(),
        ));
    }
    let vision = vision_models(&*ctx.store.lock().await)?;
    if !vision.contains(&stage.choice.model) {
        return Err(ArchonError::Invalid(
            "acceptance model has no confirmed vision capability".into(),
        ));
    }
    assert_eq!(stage.stage_id, STAGE);
    assert!(stage.aether_agent.is_some());
    Ok(())
}

const RULES: &str = r#"
Keep the complete selected ledger in every worker and reviewer brief. Never shrink
acceptance to match what you built. Pass references explicitly and set
inherit_references=true when delegating on an Aether that supports references.
Otherwise pass all image paths in text and require children to open them.

The final build result must include `requirement_results`, one entry per selected
requirement: {id, status: implemented|blocked|deviation, evidence, visual_pairs: []}.
Every visual requirement needs one pair per reference: {reference: <image id>,
built: <workspace image path>, viewport: {width,height}, theme, state}. Match the
approved viewport, theme and state. Evidence must exist, and images must remain
unchanged through acceptance. Blocked requirements fail the build.

A deviation needs a reason and the reference | proposed pair. Ask using
message_orchestrator kind=question when it matters. The default asks the operator
with pictures. An active operator directive authorizing proceeding without asking
permits a recorded deviation (optionally name proceed_directive); never wait under
such a directive. All decisions appear on the deviations to review page.

Code review and CI do not establish acceptance. Only the independent acceptance
stage can pass intent on the current head, tree and intent version. A new head
needs acceptance again, even when its tree is unchanged.
"#;

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
