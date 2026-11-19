//! Where the daemon applies the effort sizer (design 7.6, A11): every plan a
//! writer builds is finished here, so `[models] effort_sizing` and the caps
//! are read in one place, and a decision once stored is only ever read.

use std::collections::BTreeSet;

use archon_core::config::Config;
use archon_core::effort_sizing::{self, StageFacts};
use archon_core::model::{Effort, REVIEWERS_KEY_SUFFIX, WORKERS_KEY_SUFFIX};
use archon_core::outcome::DiffSize;
use archon_core::plan_origin::{EffortSizer, PlanWriter, PlannedPlan, Sizing};
use archon_core::policy::ModelPolicy;
use archon_core::queue::QueueItem;
use archon_pipeline::PipelineSpec;

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

/// The plan `writer` built for an item of `spec`. With `[models]
/// effort_sizing` on, each key written here is recorded with its own stage's
/// facts and sized as its item is stored; off, it stays as 1.6.3 fills it.
pub(crate) fn finish(
    (config, policy): (&Config, &ModelPolicy),
    spec: &PipelineSpec,
    size: Option<DiffSize>,
    writer: PlanWriter,
) -> PlannedPlan {
    if !config.models.effort_sizing() {
        let policy_cap = policy.max_effort;
        return writer.finish(Sizing::Off { policy_cap });
    }
    let facts = |key: &str| stage_facts(spec, key);
    let sizer = EffortSizer {
        cap: cap(config, policy),
        size,
        facts: &facts,
    };
    assert!(policy.max_effort.is_none_or(|policy| sizer.cap <= policy));
    writer.finish(Sizing::On(sizer))
}

/// What an item's history says when a steer sized its efforts again.
pub(crate) const RESIZED: &str = "sized the efforts of the stages still to run again";

/// Size `item` again after a steer changed its title, brief, tags or review
/// level: only the keys of stages not in `started`, and only while sizing is
/// on. `true` when a key was sized again.
pub(crate) fn resize(
    (config, policy): (&Config, &ModelPolicy),
    item: &mut QueueItem,
    started: &BTreeSet<String>,
) -> bool {
    let at = archon_core::now();
    if !config.models.effort_sizing() {
        item.settle_sizing(at);
        return false;
    }
    let is_started = |key: &str| started.contains(stage_of(key));
    let resized = item.resize_unstarted((cap(config, policy), at), &is_started);
    debug_assert!(item.model_plan.fault(&item.id).is_none());
    resized
}

/// Hold every effort the daemon sized for `item` to the policy's cap as it
/// is now, with the reason; a pinned effort over it is left for the policy
/// check to refuse. `true` when one was lowered.
pub(crate) fn clip_to_policy(policy: &ModelPolicy, item: &mut QueueItem) -> bool {
    let at = archon_core::now();
    let clipped = item.model_plan.clip_sized(&item.id, policy.max_effort, at);
    debug_assert!(!clipped || policy.max_effort.is_some(), "a cap clips");
    clipped
}

#[cfg(test)]
#[path = "sizing/tests.rs"]
mod tests;
