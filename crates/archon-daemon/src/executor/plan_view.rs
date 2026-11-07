//! A run's plan as `run_plan` serves it: the stages of its frozen pipeline
//! with the steers it ran under, and what each plan key resolves to. The
//! store keeps a run's plan as it finishes, so it still answers after the
//! snapshot is gone, until the run's history is pruned.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::model::{Effort, ModelChoice, REVIEWERS_KEY_SUFFIX};
use archon_core::protocol::{ModelSource, RunPlan, SettingSource, StageSettings};
use archon_core::queue::QueueItem;
use archon_core::run::{PipelineRun, RunState};
use archon_core::steer::RunEdit;
use archon_pipeline::PipelineSpec;

use super::RunTable;
use super::frozen;
use crate::context::DaemonContext;

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

/// The plan `run` ran, to keep as it ends in `state`: from its snapshot with
/// its recorded edits, or its driver's pipeline when the snapshot does not
/// read. `None`, logged, when neither gives one.
pub(super) async fn finished(
    ctx: &DaemonContext,
    table: &RunTable,
    (run, state): (&PipelineRun, &RunState),
) -> Option<RunPlan> {
    assert!(state.is_terminal(), "a plan is kept as its run finishes");
    assert!(
        !run.state.is_terminal(),
        "built before the run is shown finished"
    );
    let built = match build(ctx, table, run).await {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(run = %run.id, %error, "the finished run's plan cannot be built; run_plan cannot show it");
            return None;
        }
    };
    assert!(built.finished && built.run == run.id, "the run's own plan");
    Some(built)
}

async fn build(
    ctx: &DaemonContext,
    table: &RunTable,
    run: &PipelineRun,
) -> Result<RunPlan, ArchonError> {
    let (recorded, item) = {
        let store = ctx.store.lock().await;
        (store.run_edits(&run.id)?, store.item(&run.item)?)
    };
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    let thawed = frozen::thaw_edited(&ctx.paths.data_dir, &run.id, &run.pipeline, &edits).await;
    let spec = match (thawed, table.drivers.get(&run.id)) {
        (Ok(spec), _) => spec,
        (Err(error), Some(driver)) => {
            tracing::warn!(run = %run.id, %error, "the run's snapshot does not read; its plan is its driver's");
            driver.spec().as_ref().clone()
        }
        (Err(error), None) => return Err(error),
    };
    Ok(RunPlan {
        finished: true,
        ..plan_of(&run.id, &spec, &edits, &item)?
    })
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
            std::slice::from_ref(&tiered),
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
