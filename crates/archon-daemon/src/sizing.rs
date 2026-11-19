//! Where the daemon applies the effort sizer (design 7.6, A11): every plan a
//! writer builds is finished here, so `[models] effort_sizing` and the caps
//! are read in one place, and a decision once stored is only ever read.

use std::collections::BTreeSet;

use archon_core::autonomy::ReviewLevel;
use archon_core::config::Config;
use archon_core::effort_sizing::{self, StageFacts};
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelChoice, REVIEWERS_KEY_SUFFIX, WORKERS_KEY_SUFFIX};
use archon_core::outcome::DiffSize;
use archon_core::plan_origin::{
    EffortSizer, PlanSource, PlanWriter, PlannedPlan, Sizing, SlotSizing,
};
use archon_core::policy::ModelPolicy;
use archon_core::queue::QueueItem;
use archon_core::run::AttemptSizing;
use archon_pipeline::{PipelineSpec, SpawnPlan};

/// What plan key `key` of `spec` is sized from: its own stage's declared
/// default effort and kind, never those of a choice it was filled from.
pub(crate) fn stage_facts(spec: &PipelineSpec, key: &str) -> Option<StageFacts> {
    let base = spec.default_effort(key)?;
    let reviewers = spec.reviewers_key().as_deref() == Some(key);
    let stage = spec.stage(stage_of(key))?;
    assert!(spec.is_plan_key(key), "a key with a default is a plan key");
    Some(StageFacts::new(base, stage.kind, reviewers))
}

/// The stage whose agents plan key `key` is read by: the stage itself, the
/// one running its workers, or the one holding its review.
fn stage_of(key: &str) -> &str {
    let stage = key
        .strip_suffix(WORKERS_KEY_SUFFIX)
        .or_else(|| key.strip_suffix(REVIEWERS_KEY_SUFFIX))
        .unwrap_or(key);
    debug_assert!(!stage.is_empty(), "a plan key names a stage");
    stage
}

/// The most the sizer sets now: the lower of the policy's cap and
/// `[models] sizer_max_effort`.
fn cap(config: &Config, policy: &ModelPolicy) -> Effort {
    effort_sizing::sizer_cap(policy.max_effort, config.models.sizer_max_effort())
}

/// The cap an effort the daemon sized is held to as a re-plan carries it:
/// the sizer's while `[models] effort_sizing` is on, else the policy's alone.
pub(crate) fn carried_cap(config: &Config, policy: &ModelPolicy) -> Option<Effort> {
    match config.models.effort_sizing() {
        true => Some(cap(config, policy)),
        false => policy.max_effort,
    }
}

/// The plan `writer` built for an item of `spec`. With `[models]
/// effort_sizing` on, each key written here is recorded with its own stage's
/// facts and sized as its item is stored; off, it stays as 1.6.3 fills it.
/// `measured` reads the diff size of the item's last run, and is asked only
/// for a plan sized for the first time.
pub(crate) fn finish(
    (config, policy): (&Config, &ModelPolicy),
    spec: &PipelineSpec,
    measured: impl FnOnce() -> Result<Option<DiffSize>, ArchonError>,
    writer: PlanWriter,
) -> Result<PlannedPlan, ArchonError> {
    if !config.models.effort_sizing() {
        let policy_cap = policy.max_effort;
        return Ok(writer.finish(Sizing::Off { policy_cap }));
    }
    let size = match writer.sizes_first() {
        true => measured()?,
        false => None,
    };
    let facts = |key: &str| stage_facts(spec, key);
    let sizer = EffortSizer {
        cap: cap(config, policy),
        size,
        facts: &facts,
    };
    assert!(policy.max_effort.is_none_or(|policy| sizer.cap <= policy));
    assert_eq!(Some(sizer.cap), carried_cap(config, policy));
    Ok(writer.finish(Sizing::On(sizer)))
}

/// What an item's history says when a steer sized its efforts again.
pub(crate) const RESIZED: &str = "sized the efforts of the stages still to run again";

/// What the sizer reads of an item, kept before an operation changes it, so
/// only an operation that moves it sizes stages again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inputs {
    title: String,
    brief: String,
    tags: Vec<String>,
    review: ReviewLevel,
}

impl Inputs {
    pub(crate) fn of(item: &QueueItem) -> Self {
        Self {
            title: item.title.clone(),
            brief: item.brief.clone(),
            tags: item.tags.clone(),
            review: item.autonomy.review,
        }
    }
}

/// Size `item` again after an operation changed its title, brief, tags or
/// review level from `before`: only the keys of stages not in `started`, and
/// only while sizing is on. Keys the operation added are sized first, moved
/// or not. `true` when a key was sized again.
pub(crate) fn resize(
    (config, policy): (&Config, &ModelPolicy),
    item: &mut QueueItem,
    started: &BTreeSet<String>,
    before: &Inputs,
) -> bool {
    let at = archon_core::now();
    item.settle_sizing(at);
    assert!(
        !item.model_plan.has_unsized(),
        "every recorded key is sized"
    );
    // A carried plan stays as frozen until its item's own inputs move (a retry's amendment does not).
    if !config.models.effort_sizing() || Inputs::of(item) == *before {
        return false;
    }
    let is_started = |key: &str| started.contains(stage_of(key));
    let resized = item.resize_unstarted((cap(config, policy), at), &is_started);
    debug_assert!(item.model_plan.fault(&item.id).is_none());
    resized
}

/// Hold every effort the daemon sized for `item` to the policy's cap as it
/// is now and to what its model's engine runs, with the reason; a pinned
/// effort over the cap is left for the policy check to refuse. `true` when
/// one was lowered.
pub(crate) fn clip_to_policy(policy: &ModelPolicy, item: &mut QueueItem) -> bool {
    let at = archon_core::now();
    let clipped = item.model_plan.clip_sized(&item.id, policy.max_effort, at);
    debug_assert!(item.model_plan.fault(&item.id).is_none());
    clipped
}

/// The effort `item`'s stored plan holds for slot `fan_index` of `stage`,
/// with how the daemon sized it; `None` when it did not: a pin, or a key
/// never sized.
fn daemon_sized(item: &QueueItem, (stage, fan_index): (&str, u32)) -> Option<(Effort, SlotSizing)> {
    let choices = item.model_plan.choices_for(stage)?;
    let slot = (fan_index as usize).checked_rem(choices.len())?;
    let sized = item.model_plan.slot_sizing(&item.id, stage, slot)?;
    let by_daemon = matches!(
        sized.effort_source,
        PlanSource::Sizer | PlanSource::Escalation
    );
    by_daemon.then(|| (choices[slot].effort, sized))
}

/// The effort an agent of `slot` runs `choice` at: `high` for an `x_high`
/// the daemon sized whose model moved to an engine that runs none. A pinned
/// effort stays as 1.6.3 leaves it.
pub(crate) fn engine_effort(item: &QueueItem, slot: (&str, u32), choice: &ModelChoice) -> Effort {
    let runs = effort_sizing::supports_x_high(&choice.model);
    if choice.effort != Effort::XHigh || runs || daemon_sized(item, slot).is_none() {
        return choice.effort;
    }
    Effort::High
}

/// Hold a spawn of a slot the daemon sized to what the item's stored plan
/// holds for it now, which a quota move may have lowered since its driver
/// read it, and to what its engine runs. A pin or a key never sized is kept.
pub(crate) fn hold_to_plan(item: &QueueItem, plan: &mut SpawnPlan) {
    let slot = (plan.stage_id.as_str(), plan.fan_index);
    let stored = daemon_sized(item, slot).map(|(stored, _)| stored);
    if let Some(stored) = stored {
        plan.choice.effort = plan.choice.effort.min(stored);
    }
    plan.choice.effort = engine_effort(item, slot, &plan.choice);
    assert!(
        stored.is_none_or(|stored| plan.choice.effort <= stored),
        "a sized slot never runs above its stored plan"
    );
}

/// Ruling (a): hold a spawn of a slot the daemon sized to `cap`, the policy's
/// as it is now; the item's stored plan is kept. A pin is left for the policy
/// check to refuse, as in 1.6.3.
pub(crate) fn hold_to_policy(item: &QueueItem, cap: Option<Effort>, plan: &mut SpawnPlan) {
    let slot = (plan.stage_id.as_str(), plan.fan_index);
    let Some(cap) = cap.filter(|cap| plan.choice.effort > *cap) else {
        return;
    };
    let Some((stored, _)) = daemon_sized(item, slot) else {
        return;
    };
    assert!(
        plan.choice.effort <= stored,
        "held to the stored plan first"
    );
    plan.choice.effort = cap;
}

/// A wake of `plan`'s slot keeps the agent, so a slot the daemon sized is
/// requested at the effort the agent runs (`ran`), not the driver's copy.
/// Any other slot keeps its plan, as in 1.6.3.
pub(crate) fn hold_to_woken(item: &QueueItem, ran: &ModelChoice, plan: &mut SpawnPlan) {
    let slot = (plan.stage_id.as_str(), plan.fan_index);
    if ran.model != plan.choice.model || daemon_sized(item, slot).is_none() {
        return;
    }
    plan.choice.effort = ran.effort;
}

/// How the effort `plan`'s slot runs at was chosen, read from the item's
/// stored plan: nothing when the sizer never decided the stage's key. A slot
/// the daemon sized that runs lower than its plan holds says why.
pub(crate) fn attempt(item: &QueueItem, plan: &SpawnPlan) -> AttemptSizing {
    let choices = item.model_plan.choices_for(&plan.stage_id);
    let Some(choices) = choices.filter(|choices| !choices.is_empty()) else {
        return AttemptSizing::default();
    };
    let slot = plan.fan_index as usize % choices.len();
    let ran = plan.choice.effort;
    if choices[slot].effort == ran {
        let sized = item.model_plan.slot_sizing(&item.id, &plan.stage_id, slot);
        return sized.map(Into::into).unwrap_or_default();
    }
    let lowered = daemon_sized(item, (&plan.stage_id, plan.fan_index));
    let Some((planned, mut sized)) = lowered.filter(|(planned, _)| ran < *planned) else {
        return AttemptSizing::default();
    };
    let engine = &plan.choice.model;
    let why = match ran == Effort::High && !effort_sizing::supports_x_high(engine) {
        true => format!("{} runs no more than high, so high", engine.provider),
        false => format!("clipped from {planned} to the cap {ran}"),
    };
    sized.reason.push_str(&format!("; this attempt: {why}"));
    assert!(ran < planned, "only a slot run lower says why");
    sized.into()
}

#[cfg(test)]
#[path = "sizing/tests.rs"]
mod tests;
