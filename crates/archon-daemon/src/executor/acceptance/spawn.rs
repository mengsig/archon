use crate::context::DaemonContext;
use crate::resolve::RepoEnv;
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::intent::ApprovedIntent;
use archon_core::model::ModelRef;
use archon_core::plan_origin::{Authority, PlanSource};
use archon_core::ports::SpawnRequest;
use archon_core::queue::QueueItem;
use archon_core::reference::{Reference, ReferenceKind};
use archon_core::run::{PipelineRun, StageRun, StageState};
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
        let owed = results_owed(&record.selected_requirements(binding)?, &record.images)?;
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

/// Place the acceptance stage of `plans` on the model that judges the ledger.
/// A model the item's plan pins for it (named by the operator or the
/// orchestrator) judges as given while policy permits it, the catalog lists it
/// and it can see the images; unless `[review] independent_acceptance` holds,
/// sharing a builder's model or vendor only labels the record. Without a pin:
/// independent of every builder where one is permitted and connected, else,
/// unless that knob holds, the strongest one, labelled. Either way a judge
/// whose provider is out of quota is skipped. A `woken` judge the catalog and
/// policy still accept keeps judging. `failed` is the attempt being replaced,
/// not yet stored.
pub(in crate::executor) fn select_model(
    ctx: &DaemonContext,
    store: &mut Store,
    item: &QueueItem,
    run: &PipelineRun,
    plans: &mut [SpawnPlan],
    (woken, failed): (Option<&ModelRef>, Option<&StageRun>),
) -> Result<(), ArchonError> {
    let Some(first) = plans.iter().find(|p| p.stage_id == STAGE) else {
        return Ok(());
    };
    let pin = pinned(item, first);
    super::records::invalidate(store, &item.id)?;
    let visual = super::intent(store, item)?.is_some_and(|r| !r.images.is_empty());
    let mut stages = store.stage_runs(&run.id)?;
    if let Some(failed) = failed {
        stages.retain(|s| s.id != failed.id);
        stages.push(failed.clone());
    }
    let builders = builder_models(&stages);
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&crate::resolve::repo_ref(id)))
        .transpose()?;
    let policy = RepoEnv::new(repo)?.resolved_policy(ctx);
    let vision = vision_models(store)?;
    let catalog = store.model_catalog();
    let judges = |m: &ModelRef| policy.permits_model(m) && (!visual || vision.contains(m));
    let permitted = judge_pool(&catalog, judges, &builders);
    let out_of_quota = Quota::of(crate::quota::standing(ctx), &stages);
    // A provider the reading shows spent is never used. One the latest attempt
    // failed on, with no reading to say otherwise, is only passed over while
    // another judge can run: a lone provider is retried, as it always was.
    let usable: Vec<_> = permitted
        .iter()
        .copied()
        .filter(|m| !out_of_quota.spent(&m.model))
        .collect();
    let moved: Vec<_> = usable
        .iter()
        .copied()
        .filter(|m| !out_of_quota.failed_on(&m.model))
        .collect();
    let eligible = if moved.is_empty() { usable } else { moved };
    let still_judges = |judge: &&ModelRef| {
        catalog.check(judge).is_ok_and(|listed| listed == **judge)
            && policy.permits_model(judge)
            && (!visual || vision.contains(judge))
            && eligible.iter().any(|m| m.model == **judge)
    };
    let pinned_judge = match pin.as_ref().and_then(|m| catalog.check(m).ok()) {
        Some(model) if eligible.iter().any(|m| m.model == model) => Some(model),
        _ => None,
    };
    if let (Some(model), true) = (&pinned_judge, ctx.config.review.independent_acceptance)
        && shares_builder(model, &builders)
    {
        return Err(ArchonError::Invalid(format!(
            "independent acceptance needs a model different from every builder in model and \
             vendor, but the plan pins `{model}` for {STAGE} ([review] independent_acceptance = true)"
        )));
    }
    if let (Some(pin), None) = (&pin, &pinned_judge) {
        tracing::info!(item = %item.id, model = %pin, "the pinned acceptance model cannot judge now; another is selected");
    }
    let model = match woken.filter(still_judges).or(pinned_judge.as_ref()) {
        Some(judge) => judge.clone(),
        None if eligible.is_empty() && !permitted.is_empty() => {
            return Err(ArchonError::Invalid(format!(
                "no eligible acceptance judge: every permitted, connected model's provider is out \
                 of quota ({})",
                out_of_quota.names(permitted.iter().map(|m| &m.model))
            )));
        }
        None => judge_model(
            &eligible,
            &builders,
            visual,
            ctx.config.review.independent_acceptance,
        )?,
    };
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

/// The model the item's plan pins for the acceptance stage `plan`: one the
/// operator or the orchestrator named, not one the daemon filled in.
fn pinned(item: &QueueItem, plan: &SpawnPlan) -> Option<ModelRef> {
    let slots = item.model_plan.choices_for(STAGE)?;
    let slot = plan.fan_index as usize % slots.len().max(1);
    let origin = item.model_plan.origin(&item.id, STAGE, slot);
    let named = origin.model.source == PlanSource::Explicit
        && matches!(
            origin.model.by,
            Authority::Operator | Authority::Orchestrator
        );
    named.then(|| slots[slot].model.clone())
}

/// Providers a judge should not run on now. `spent` is what Aether's trusted
/// reading shows has no usage left, and holds whatever else is known; `failed`
/// is the provider the run's latest acceptance attempt ended on for a usage
/// limit, which only steers the choice while another judge is eligible and a
/// reading that shows the provider with usage left overrides.
struct Quota {
    reading: Option<std::collections::BTreeMap<String, bool>>,
    failed: Option<String>,
}

impl Quota {
    fn of(reading: Option<std::collections::BTreeMap<String, bool>>, stages: &[StageRun]) -> Self {
        let failed = stages
            .iter()
            .filter(|s| s.stage_id == STAGE)
            .max_by_key(|s| (s.started_at, s.attempt))
            .filter(|s| match &s.state {
                StageState::Failed { error } => crate::executor::failure_class::is_quota(error),
                _ => false,
            })
            .map(|s| s.current_choice().model.provider.clone());
        Self { reading, failed }
    }

    /// The reading shows `model`'s provider with no usage left.
    fn spent(&self, model: &ModelRef) -> bool {
        let known = self.reading.as_ref().and_then(|r| r.get(&model.provider));
        known.copied().unwrap_or(false)
    }

    /// The latest acceptance attempt failed on `model`'s provider's usage
    /// limit, and no trusted reading says the provider has usage left.
    fn failed_on(&self, model: &ModelRef) -> bool {
        let known = self
            .reading
            .as_ref()
            .is_some_and(|r| r.contains_key(&model.provider));
        !known && self.failed.as_deref() == Some(model.provider.as_str())
    }

    fn names<'m>(&self, models: impl Iterator<Item = &'m ModelRef>) -> String {
        let mut names: Vec<&str> = models
            .filter(|m| self.spent(m))
            .map(|m| m.provider.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names.join(", ")
    }
}

/// Whether `model` is a builder's or of a builder's vendor.
pub(super) fn shares_builder(model: &ModelRef, builders: &[ModelRef]) -> bool {
    builders
        .iter()
        .any(|b| b == model || vendor(&b.provider) == vendor(&model.provider))
}

/// The model apart from every builder that prefers another vendor and then
/// the strongest tier; with none, the strongest permitted one unless `strict`.
/// Listed models `judges` accepts. A superseded one is dropped only for a
/// same-provider successor that judges too, and as independently of every builder.
fn judge_pool<'c>(
    catalog: &'c archon_core::model_tier::ModelCatalog,
    judges: impl Fn(&ModelRef) -> bool,
    builders: &[ModelRef],
) -> Vec<&'c archon_core::model_tier::CatalogModel> {
    let pool: Vec<_> = catalog
        .models()
        .iter()
        .filter(|m| !catalog.is_retired(&m.model) && judges(&m.model))
        .filter(|m| {
            let independent = !builders.contains(&m.model);
            !catalog.replaced_for(&m.model, |s| {
                s.model.provider == m.model.provider
                    && judges(&s.model)
                    && (!independent || !builders.contains(&s.model))
            })
        })
        .collect();
    debug_assert!(pool.iter().all(|m| judges(&m.model)));
    pool
}

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

/// Whether the acceptance `stage` judged apart from every builder, in model
/// and vendor. Refused when it could not see the approved images, or under
/// the strict knob when it ran on a builder's model.
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
    let builders = builder_models(&stages);
    if builders.contains(&stage.choice.model) && ctx.config.review.independent_acceptance {
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
    Ok(!shares_builder(&stage.choice.model, &builders))
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
/// The skeleton names each reference's recorded viewport, theme and state
/// exactly, which the gate compares with the built image's: the builder
/// takes its picture in those conditions and copies them. Where a reference
/// records none, the skeleton keeps a placeholder (`null` for a theme or
/// state, which the gate then accepts).
pub(in crate::executor) fn results_owed(
    selected: &[&archon_core::intent::Requirement],
    images: &[archon_core::intent::ReferenceImage],
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
                .map(|id| {
                    let image = images.iter().find(|i| &i.id == id);
                    let viewport = match image.and_then(|i| i.viewport.as_ref()) {
                        Some(v) => serde_json::json!({"width": v.width, "height": v.height}),
                        None => serde_json::json!({"width": "<px>", "height": "<px>"}),
                    };
                    let recorded = |value: Option<&String>| match (image, value) {
                        (_, Some(value)) => serde_json::json!(value),
                        (Some(_), None) => serde_json::Value::Null,
                        (None, None) => serde_json::json!("<as the reference>"),
                    };
                    serde_json::json!({
                        "reference": id, "built": "<workspace path of the built image>",
                        "viewport": viewport,
                        "theme": recorded(image.and_then(|i| i.theme.as_ref())),
                        "state": recorded(image.and_then(|i| i.state.as_ref())),
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
         keeps `[]`. Take each built image at the `viewport`, `theme` and `state` shown \
         (its reference's) and keep those values as they are: the gate refuses a pair taken \
         in other conditions. A `null` theme or state means the reference records none.\n\n\
         ```json\n{shape}\n```\n"
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

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};

    fn listed(models: &[&str]) -> ModelCatalog {
        ModelCatalog::new(
            models
                .iter()
                .map(|m| CatalogModel {
                    model: m.parse().unwrap(),
                    tier: Some(Tier::Strong),
                    is_default: false,
                })
                .collect(),
        )
    }

    fn names(pool: &[&CatalogModel]) -> Vec<String> {
        pool.iter().map(|m| m.model.to_string()).collect()
    }

    #[test]
    fn a_superseded_judge_stays_while_its_successor_cannot_judge() {
        let mut catalog = listed(&["codex/build", "claude-code/old", "claude-code/blind"]);
        catalog.add_superseded(
            "claude-code/old".parse().unwrap(),
            "claude-code/blind".parse().unwrap(),
        );
        let vision: Vec<ModelRef> = [
            "codex/build".parse().unwrap(),
            "claude-code/old".parse().unwrap(),
        ]
        .into();
        let builders: Vec<ModelRef> = vec!["codex/build".parse().unwrap()];
        let pool = judge_pool(&catalog, |m| vision.contains(m), &builders);
        assert_eq!(names(&pool), ["codex/build", "claude-code/old"]);
        let model = judge_model(&pool, &builders, true, true).unwrap();
        assert_eq!(model.to_string(), "claude-code/old");
    }

    #[test]
    fn an_independent_judge_is_not_replaced_by_a_builder_s_model() {
        let mut catalog = listed(&["codex/build", "claude-code/old"]);
        catalog.add_superseded(
            "claude-code/old".parse().unwrap(),
            "codex/build".parse().unwrap(),
        );
        let builders: Vec<ModelRef> = vec!["codex/build".parse().unwrap()];
        let pool = judge_pool(&catalog, |_| true, &builders);
        assert_eq!(
            judge_model(&pool, &builders, false, true)
                .unwrap()
                .to_string(),
            "claude-code/old"
        );
        let mut plain = listed(&["claude-code/old", "claude-code/new"]);
        plain.add_superseded(
            "claude-code/old".parse().unwrap(),
            "claude-code/new".parse().unwrap(),
        );
        let pool = judge_pool(&plain, |_| true, &builders);
        assert_eq!(
            names(&pool),
            ["claude-code/new"],
            "a judging successor replaces it"
        );
    }

    #[test]
    fn an_other_vendor_judge_is_not_replaced_by_a_cross_provider_successor() {
        let mut catalog = listed(&["codex/build", "claude-code/old", "codex/new"]);
        catalog.add_superseded(
            "claude-code/old".parse().unwrap(),
            "codex/new".parse().unwrap(),
        );
        let builders: Vec<ModelRef> = vec!["codex/build".parse().unwrap()];
        let pool = judge_pool(&catalog, |_| true, &builders);
        assert!(names(&pool).contains(&"claude-code/old".to_owned()));
        let model = judge_model(&pool, &builders, false, true).unwrap();
        assert_eq!(
            model.to_string(),
            "claude-code/old",
            "the other vendor judges"
        );
    }
}
