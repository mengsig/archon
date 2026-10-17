//! Shape steers (ARCHITECTURE §6.6): working out the stage a steer inserts
//! or the pipeline it switches to, with the model plan and the knobs the
//! item has from then on, and the stage list a steered item keeps for its
//! later runs.

use std::collections::BTreeSet;

use archon_core::autonomy::{
    Autonomy, Knob as _, MergeMode, ProposalMode, ReportMode, ReviewLevel,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::ModelPlan;
use archon_core::policy::ModelPolicy;
use archon_core::queue::QueueItem;
use archon_core::run::PipelineRun;
use archon_core::steer::RunEdit;
use archon_pipeline::autonomy::{augment, review_level, ships};
use archon_pipeline::{PipelineSpec, reshape};
use archon_queue::Store;

use super::Asked;
use crate::context::DaemonContext;
use crate::knobs;
use crate::resolve::RepoEnv;

/// `kv`: `shape:<item>` → the JSON [`RunEdit`] holding the item's whole
/// steered stage list, which every later run of it starts with. Its
/// `pipeline` names the catalog pipeline the item was last switched to.
const KEY_PREFIX: &str = "shape:";

fn key(item: &QueueItemId) -> String {
    format!("{KEY_PREFIX}{item}")
}

/// The catalog pipelines a shape steer names, read before any lock is taken.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sources {
    /// The pipeline `insert_stage` takes its stage from.
    insert_from: Option<PipelineSpec>,
    /// The pipeline `switch_pipeline` names, as the catalog has it.
    switch_to: Option<PipelineSpec>,
}

impl Sources {
    pub(crate) fn read(
        env: &RepoEnv,
        item: &QueueItem,
        steer: &archon_core::steer::Steer,
    ) -> Result<Self, ArchonError> {
        if steer.insert_stage.is_none() && steer.switch_pipeline.is_none() {
            return Ok(Self::default());
        }
        let catalog = env.catalog()?;
        let insert_from = match &steer.insert_stage {
            Some(insert) => {
                let from = insert.from.as_deref().unwrap_or(&item.pipeline);
                Some(catalog.get(from)?.clone())
            }
            None => None,
        };
        let switch_to = match &steer.switch_pipeline {
            Some(name) => Some(catalog.get(name)?.clone()),
            None => None,
        };
        assert!(
            insert_from.is_none() || switch_to.is_none(),
            "checked by Steer"
        );
        Ok(Self {
            insert_from,
            switch_to,
        })
    }

    /// Whether the steer switches to `pipeline` itself.
    pub(crate) fn switches_to(&self, pipeline: &str) -> bool {
        self.switch_to
            .as_ref()
            .is_some_and(|to| to.name() == pipeline)
    }
}

/// A shape steer worked out, with nothing stored yet.
#[derive(Debug, Clone)]
pub(crate) struct Reshaped {
    pub edit: RunEdit,
    /// The pipeline the edit yields.
    pub spec: PipelineSpec,
    /// The item's plan for exactly the stages of `spec`.
    pub plan: ModelPlan,
    pub autonomy: Autonomy,
    /// For the item's history.
    pub what: String,
}

/// The item's knobs under pipeline `target`: one that does not ship takes
/// no proposal, report or merge, as an enqueue of it would not.
fn switched_knobs(target: &PipelineSpec, autonomy: Autonomy) -> Autonomy {
    if ships(target) {
        return autonomy;
    }
    Autonomy {
        proposal: ProposalMode::Off,
        report: ReportMode::Off,
        merge: MergeMode::Manual,
        ..autonomy
    }
}

/// The pipeline a shape steer is worked out against.
#[derive(Debug, Clone)]
pub(crate) struct Current<'a> {
    pub spec: &'a PipelineSpec,
    /// How many leading stages already started and stay as they are.
    pub prefix: usize,
    /// Stages a steer removed, which a run passes over.
    pub removed: BTreeSet<String>,
}

/// Work out the insert or the switch `asked` for against `current`; `None`
/// for neither. Refused when the model policy or the ceiling forbids it.
pub(crate) fn reshape(
    ctx: &DaemonContext,
    env: &RepoEnv,
    store: &Store,
    item: &QueueItem,
    current: Current<'_>,
    asked: &Asked<'_>,
) -> Result<Option<Reshaped>, ArchonError> {
    let (steer, sources) = (asked.steer, asked.sources);
    let Current {
        spec,
        prefix,
        mut removed,
    } = current;
    assert!(prefix <= spec.file.stages.len(), "a prefix of the stages");
    let (edit, reshaped, knobs, what) = if let Some(insert) = &steer.insert_stage {
        let source = sources.insert_from.as_ref().expect("read with the steer");
        let after = insert.after.as_deref();
        let (edit, reshaped) = reshape::insert_edit(spec, prefix, source, &insert.stage, after)?;
        let what = format!("inserted stage `{}` of `{}`", insert.stage, source.name());
        (edit, reshaped, item.autonomy, what)
    } else if steer.switch_pipeline.is_some() {
        let base = sources.switch_to.as_ref().expect("read with the steer");
        let knobs = switched_knobs(base, item.autonomy);
        let target = augment(base, &knobs)?;
        let (edit, reshaped) = reshape::switch_edit(spec, prefix, &target)?;
        let what = format!("switched the stages still to run to `{}`", base.name());
        // The removed stages go with the pipeline switched away.
        removed.clear();
        (edit, reshaped, knobs, what)
    } else {
        return Ok(None);
    };
    let autonomy = within_ceiling(ctx, env, &reshaped, &removed, item.autonomy, knobs)?;
    let what = noting_review(what, item.autonomy.review, autonomy.review);
    let repo = env.repo.as_ref().map(|repo| &repo.id);
    let policy = env.policy(&ctx.config);
    let learning = Learning {
        store,
        repo,
        policy: &policy,
        max_tier: env.max_default_tier(&ctx.config),
    };
    let plan = projected_plan(&learning, spec, &reshaped, &item.model_plan)?;
    policy.check_plan(&plan)?;
    Ok(Some(Reshaped {
        edit,
        spec: reshaped,
        plan,
        autonomy,
        what,
    }))
}

/// The review `spec` still performs once the `removed` stages are passed
/// over.
fn review_after(spec: &PipelineSpec, removed: &BTreeSet<String>) -> ReviewLevel {
    let mut left = spec.clone();
    left.file
        .stages
        .retain(|stage| !removed.contains(&stage.id));
    let declared = left.file.pipeline.review.take();
    left.file.pipeline.review = declared.filter(|review| !removed.contains(&review.stage));
    assert!(left.file.stages.len() <= spec.file.stages.len());
    review_level(&left)
}

/// `what` a steer did, saying so when that moved the item's review level
/// from what it `had`.
pub(crate) fn noting_review(what: String, had: ReviewLevel, now: ReviewLevel) -> String {
    assert!(!what.is_empty(), "a steer that changed something");
    if had == now {
        return what;
    }
    let way = if now.rank() < had.rank() {
        "raised"
    } else {
        "lowered"
    };
    format!("{what}; {way} the review level from {had} to {now}")
}

/// `knobs` with the review `spec` performs without the `removed` stages.
/// Where that differs from what the item `had`, the ceiling must allow it.
pub(crate) fn within_ceiling(
    ctx: &DaemonContext,
    env: &RepoEnv,
    spec: &PipelineSpec,
    removed: &BTreeSet<String>,
    had: Autonomy,
    knobs: Autonomy,
) -> Result<Autonomy, ArchonError> {
    assert!(
        removed.iter().all(|id| spec.stage(id).is_some()),
        "only stages of this pipeline are passed over"
    );
    let autonomy = Autonomy {
        review: review_after(spec, removed),
        ..knobs
    };
    if autonomy == had {
        return Ok(autonomy);
    }
    knobs::check_ceiling(&env.autonomy(ctx), env, spec, autonomy)?;
    Ok(autonomy)
}

/// Where the learned choices of a repository come from, and what may be picked.
struct Learning<'a> {
    store: &'a Store,
    repo: Option<&'a archon_core::id::RepoId>,
    policy: &'a ModelPolicy,
    max_tier: archon_core::model_tier::Tier,
}

/// `plan` for the stages of `new`: the choices of the stages it keeps, then
/// what was learned for the repository, then the first choice it had.
fn projected_plan(
    learning: &Learning<'_>,
    old: &PipelineSpec,
    new: &PipelineSpec,
    plan: &ModelPlan,
) -> Result<ModelPlan, ArchonError> {
    let stages = old.file.stages.iter();
    let fallback = stages
        .filter_map(|stage| plan.choices_for(&stage.id))
        .find_map(|choices| choices.first().cloned());
    let mut kept = plan.clone();
    let reviewers = new.reviewers_key();
    kept.0
        .retain(|id, _| new.stage(id).is_some() || Some(id) == reviewers.as_ref());
    for (id, choices) in &mut kept.0 {
        if let Some(stage) = new.stage(id) {
            choices.truncate(stage.choice_limit() as usize);
        }
    }
    let Learning {
        store,
        repo,
        policy,
        max_tier,
    } = *learning;
    let bounds = (policy, max_tier);
    let filled = knobs::fill_plan(store, repo, (new, new), kept, bounds, fallback.as_ref())?;
    let unfilled = new.file.stages.iter();
    if let Some(stage) = unfilled
        .into_iter()
        .find(|s| filled.choices_for(&s.id).is_none())
    {
        return Err(ArchonError::Invalid(format!(
            "no model choice for stage {:?}; give the item a model_plan first",
            stage.id
        )));
    }
    new.check_plan_keys(&filled)?;
    Ok(filled)
}

/// Keep `spec` as the stage list every later run of `item` starts with,
/// named for the pipeline `switched` to, else the one it was last switched to.
pub(crate) fn keep(
    store: &mut Store,
    item: &QueueItemId,
    spec: &PipelineSpec,
    switched: Option<&str>,
) -> Result<(), ArchonError> {
    // A switch from nothing to itself carries every stage with its prompt.
    let (whole, same) = reshape::switch_edit(spec, 0, spec)?;
    assert_eq!(same.file.stages, spec.file.stages, "kept as it is");
    let RunEdit::SwitchPipeline { stages, .. } = whole else {
        unreachable!("switch_edit yields a switch, not {whole:?}");
    };
    let pipeline = match switched {
        Some(name) => name.to_owned(),
        None => kept_name(store, item)?.unwrap_or_else(|| spec.name().to_owned()),
    };
    let whole = RunEdit::SwitchPipeline { pipeline, stages };
    store.kv_set(&key(item), &serde_json::to_string(&whole)?)
}

/// The pipeline the stage list kept for `item` is named for, if one is kept.
fn kept_name(store: &Store, item: &QueueItemId) -> Result<Option<String>, ArchonError> {
    let Some(raw) = store.kv_get(&key(item))? else {
        return Ok(None);
    };
    let whole: RunEdit = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("steered stages of item {item}: {e}")))?;
    let RunEdit::SwitchPipeline { pipeline, .. } = whole else {
        return Err(ArchonError::Store(format!(
            "steered stages of item {item} are kept as a {} edit, not a switch",
            whole.kind()
        )));
    };
    assert!(!pipeline.is_empty(), "a kept stage list names its pipeline");
    Ok(Some(pipeline))
}

/// The catalog pipeline a steer switched `item` to, when it is not the
/// item's own: what `queue show` and `status` name beside it.
pub(crate) fn switched_to(store: &Store, item: &QueueItem) -> Result<Option<String>, ArchonError> {
    let name = kept_name(store, &item.id)?;
    Ok(name.filter(|name| name != &item.pipeline))
}

/// Forget the steered stage list: the item runs its pipeline as the catalog
/// has it again.
pub(crate) fn forget(store: &mut Store, item: &QueueItemId) -> Result<(), ArchonError> {
    store.kv_delete(&key(item))
}

/// Mark a run that starts with an edited shape: it is
/// no evidence for the pipeline it is named after (ARCHITECTURE §19).
pub(crate) fn correct_shaped(store: &Store, run: &PipelineRun) -> Result<(), ArchonError> {
    assert!(!run.state.is_terminal(), "counted as the run starts");
    if store.kv_get(&key(&run.item))?.is_none() {
        return Ok(());
    }
    store.save_learning_value(&format!("learning/reshaped/{}", run.id), "1")
}

/// `spec`, the item's pipeline as the catalog has it with its knobs' stages,
/// with the stage list a steer gave the item. The same when none did.
pub(crate) async fn shaped(
    ctx: &DaemonContext,
    item: &QueueItem,
    spec: PipelineSpec,
) -> Result<PipelineSpec, ArchonError> {
    assert_eq!(spec.name(), item.pipeline, "the item's own pipeline");
    let Some(raw) = ctx.store.lock().await.kv_get(&key(&item.id))? else {
        return Ok(spec);
    };
    let whole: RunEdit = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("steered stages of item {}: {e}", item.id)))?;
    let shaped = reshape::apply(&spec, &whole).map_err(|error| {
        ArchonError::Pipeline(format!(
            "the stages steered into item {} no longer fit pipeline {:?}: {error}; steer it \
             with switch_pipeline = {:?} to run the pipeline as it is now",
            item.id, item.pipeline, item.pipeline
        ))
    })?;
    assert_eq!(
        shaped.name(),
        spec.name(),
        "a steered item keeps its pipeline"
    );
    Ok(shaped)
}
