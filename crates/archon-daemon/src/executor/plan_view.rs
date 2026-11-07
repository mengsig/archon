//! A run's plan as `run_plan` serves it: the stages of its frozen pipeline
//! with the steers it ran under, and what each plan key resolves to. A run's
//! plan is kept in `<data>/run-plans/<run>.json` as it finishes, so it still
//! answers after the snapshot is gone, until the run's history is pruned.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::model::{Effort, ModelChoice, REVIEWERS_KEY_SUFFIX};
use archon_core::protocol::{ModelSource, RunPlan, SettingSource, StageSettings};
use archon_core::queue::QueueItem;
use archon_core::run::PipelineRun;
use archon_core::steer::RunEdit;
use archon_pipeline::PipelineSpec;
use archon_queue::Store;

use super::frozen;
use crate::context::DaemonContext;

fn plans_root(data_dir: &Path) -> PathBuf {
    data_dir.join("run-plans")
}

fn plan_file(run: &PipelineRunId) -> String {
    format!("{}.json", run.as_str())
}

/// The plan of run `run` of `item`, from its pipeline `spec` as `edits` left it.
pub(crate) fn plan_of(
    run: &PipelineRunId,
    spec: &PipelineSpec,
    edits: &[RunEdit],
    item: &QueueItem,
) -> Result<RunPlan, ArchonError> {
    assert!(!spec.file.stages.is_empty(), "a run keeps a stage");
    let plan = RunPlan {
        run: run.clone(),
        pipeline: spec.name().to_owned(),
        revision: revision(spec)?,
        stages: spec.summary().stages,
        edited: edits.iter().any(changes_stages),
        settings: settings(spec, item),
        finished: false,
    };
    assert_eq!(
        plan.stages.len(),
        spec.file.stages.len(),
        "every stage summarized"
    );
    Ok(plan)
}

/// Each stage's settings in order, its reviewers key after it, as the
/// item's plan and stage timeouts resolve them.
fn settings(spec: &PipelineSpec, item: &QueueItem) -> Vec<StageSettings> {
    let reviewers = spec.reviewers_key();
    let mut settings = Vec::with_capacity(spec.file.stages.len() + 1);
    for stage in &spec.file.stages {
        let choices = item.model_plan.choices_for(&stage.id).unwrap_or_default();
        let timeout = match item.stage_timeouts.get(&stage.id) {
            Some(minutes) => (*minutes, SettingSource::Override),
            None => (stage.timeout_minutes, SettingSource::Default),
        };
        settings.push(key_settings(
            &stage.id,
            choices,
            stage.effort,
            Some(timeout),
        ));
        let holds =
            |key: &&String| key.strip_suffix(REVIEWERS_KEY_SUFFIX) == Some(stage.id.as_str());
        let Some(key) = reviewers.as_ref().filter(holds) else {
            continue;
        };
        let (Some(named), Some(effort)) =
            (item.model_plan.choices_for(key), spec.default_effort(key))
        else {
            continue;
        };
        settings.push(key_settings(key, named, effort, None));
    }
    assert!(
        settings.len() >= spec.file.stages.len(),
        "a setting per stage"
    );
    settings
}

fn key_settings(
    key: &str,
    choices: &[ModelChoice],
    default_effort: Effort,
    timeout: Option<(u32, SettingSource)>,
) -> StageSettings {
    let model = match !choices.is_empty() && choices.iter().all(|c| c.tier.is_some()) {
        true => ModelSource::Tier,
        false => ModelSource::Exact,
    };
    let effort = match choices.iter().all(|c| c.effort == default_effort) {
        true => SettingSource::Default,
        false => SettingSource::Override,
    };
    StageSettings {
        key: key.to_owned(),
        choices: choices.to_vec(),
        model,
        effort,
        timeout_minutes: timeout.map(|(minutes, _)| minutes),
        timeout: timeout.map(|(_, source)| source),
    }
}

/// Keep `run`'s plan as it finishes, read while its snapshot still exists.
/// A plan not kept is logged: only `run_plan` of the finished run misses it.
pub(super) async fn keep(ctx: &DaemonContext, run: &PipelineRun) {
    assert!(
        run.state.is_terminal(),
        "a plan is kept as its run finishes"
    );
    if let Err(error) = try_keep(ctx, run).await {
        tracing::warn!(run = %run.id, %error, "the finished run's plan was not kept; run_plan cannot show it");
    }
}

async fn try_keep(ctx: &DaemonContext, run: &PipelineRun) -> Result<(), ArchonError> {
    let (recorded, item) = {
        let store = ctx.store.lock().await;
        (store.run_edits(&run.id)?, store.item(&run.item)?)
    };
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    let data_dir = &ctx.paths.data_dir;
    let spec = frozen::thaw_edited(data_dir, &run.id, &run.pipeline, &edits).await?;
    let plan = RunPlan {
        finished: true,
        ..plan_of(&run.id, &spec, &edits, &item)?
    };
    let text = serde_json::to_string(&plan).expect("a run plan encodes");
    let root = plans_root(data_dir);
    let name = plan_file(&run.id);
    tokio::task::spawn_blocking(move || frozen::write_kept(&root, &name, &text))
        .await
        .map_err(|e| ArchonError::io("run plan task", std::io::Error::other(e)))?
}

/// The plan kept when `run` finished; `None` when none was kept.
pub(crate) async fn kept(
    data_dir: &Path,
    run: &PipelineRunId,
) -> Result<Option<RunPlan>, ArchonError> {
    let path = plans_root(data_dir).join(plan_file(run));
    let read = tokio::task::spawn_blocking(move || match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchonError::io(format!("reading {}", path.display()), e)),
    });
    let read = read
        .await
        .map_err(|e| ArchonError::io("run plan task", std::io::Error::other(e)))??;
    let Some((path, text)) = read else {
        return Ok(None);
    };
    let plan: RunPlan = serde_json::from_str(&text).map_err(|e| {
        ArchonError::Corrupt(format!(
            "the kept run plan {} does not read: {e}",
            path.display()
        ))
    })?;
    assert_eq!(&plan.run, run, "a kept plan is filed under its run");
    assert!(plan.finished, "only a finished run's plan is kept");
    Ok(Some(plan))
}

/// Delete each kept plan whose run the store no longer holds; how many.
pub(crate) fn prune_kept(data_dir: &Path, store: &Store) -> Result<usize, ArchonError> {
    let root = plans_root(data_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(ArchonError::io(format!("listing {}", root.display()), e)),
    };
    let mut pruned = 0;
    for entry in entries {
        let path = entry
            .map_err(|e| ArchonError::io(format!("listing {}", root.display()), e))?
            .path();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Ok(run) = PipelineRunId::parse(stem) else {
            continue;
        };
        if !matches!(store.run(&run), Err(ArchonError::NotFound(_))) {
            continue;
        }
        std::fs::remove_file(&path)
            .map_err(|e| ArchonError::io(format!("removing {}", path.display()), e))?;
        pruned += 1;
    }
    Ok(pruned)
}

/// Whether a steer's edit changed the stages, not only how one runs.
fn changes_stages(edit: &RunEdit) -> bool {
    match edit {
        RunEdit::InsertStage { .. } | RunEdit::SwitchPipeline { .. } => true,
        RunEdit::AbortStage { .. } | RunEdit::AssignRoles { .. } => false,
    }
}

/// 64-bit FNV-1a, in hex, of the plan's manifest and prompts.
fn revision(spec: &PipelineSpec) -> Result<String, ArchonError> {
    let mut contents = spec.manifest()?;
    for (name, prompt) in &spec.prompts {
        contents.push_str(name);
        contents.push('\0');
        contents.push_str(prompt);
    }
    assert!(!contents.is_empty(), "a manifest is never empty");
    let hash = contents.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok(format!("{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn only_inserted_and_switched_stages_mark_a_plan_edited() {
        let insert = RunEdit::InsertStage {
            after: None,
            spec: serde_json::json!({}),
        };
        let switch = RunEdit::SwitchPipeline {
            pipeline: "quick".into(),
            stages: Vec::new(),
        };
        let roles = RunEdit::AssignRoles {
            stage: "team".into(),
            roles: vec!["a".into()],
            role_prompts: BTreeMap::new(),
        };
        assert!(changes_stages(&insert) && changes_stages(&switch));
        assert!(!changes_stages(&roles));
    }

    #[test]
    fn the_revision_is_fnv1a_of_the_manifest() {
        let catalog = archon_pipeline::catalog::Catalog::builtin();
        let spec = catalog.get("direct").expect("a built-in pipeline").clone();
        let revision = revision(&spec).unwrap();
        assert_eq!(revision.len(), 16, "{revision}");
        assert_eq!(revision, super::revision(&spec.clone()).unwrap(), "stable");
        let mut changed = spec;
        changed.prompts.insert("extra.md".into(), "more".into());
        assert_ne!(super::revision(&changed).unwrap(), revision);
    }

    #[test]
    fn a_pin_and_an_override_are_marked_and_defaults_are_not() {
        let tiered = ModelChoice {
            tier: Some(archon_core::model_tier::TierChoice {
                requested: archon_core::model_tier::Tier::Fast,
                provider: None,
                resolved: archon_core::model_tier::Tier::Fast,
            }),
            ..ModelChoice::pinned("p/fast".parse().unwrap(), Effort::Medium)
        };
        let pinned = ModelChoice::pinned("p/pin".parse().unwrap(), Effort::Low);
        let defaults = key_settings(
            "build",
            &[tiered.clone()],
            Effort::Medium,
            Some((30, SettingSource::Default)),
        );
        assert_eq!(
            (defaults.model, defaults.effort, defaults.timeout),
            (
                ModelSource::Tier,
                SettingSource::Default,
                Some(SettingSource::Default)
            )
        );
        let set = key_settings("build", &[tiered, pinned], Effort::Medium, None);
        assert_eq!(
            (set.model, set.effort),
            (ModelSource::Exact, SettingSource::Override)
        );
        assert_eq!(
            (set.timeout_minutes, set.timeout),
            (None, None),
            "a reviewers key"
        );
    }
}
