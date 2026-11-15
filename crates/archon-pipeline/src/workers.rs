//! What a delegating stage lead's workers run on: the item's `<stage>.workers`
//! plan key, else the pipeline's `[pipeline.workers]` default. The daemon
//! sends it to Aether as the lead's delegation policy.

use archon_core::delegation::{
    MAX_PARALLEL_WORKERS, MAX_WORKER_CHOICES, WorkerChoice, WorkerPolicy,
};
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan, WORKERS_KEY_SUFFIX};
use archon_core::model_tier::Tier;
use serde::{Deserialize, Serialize};

use crate::spec::{PipelineSpec, StageSpec};

/// `[pipeline.workers]`: any model of `tier` at `effort` or below, and at
/// most `max_parallel` at once when set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersDefault {
    pub tier: Tier,
    #[serde(default = "medium")]
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<u32>,
}

fn medium() -> Effort {
    Effort::Medium
}

impl PipelineSpec {
    /// The stage whose workers plan key `key` is; `None` for any other key.
    pub fn workers_stage(&self, key: &str) -> Option<&StageSpec> {
        let stage = key.strip_suffix(WORKERS_KEY_SUFFIX)?;
        self.stage(stage)
    }

    /// The effort a workers key gets when its choice names none: the
    /// pipeline's worker default, else medium. `None`: not a workers key.
    pub(crate) fn workers_default_effort(&self, key: &str) -> Option<Effort> {
        self.workers_stage(key)?;
        let default = self.file.pipeline.workers.as_ref();
        Some(default.map_or(Effort::Medium, |d| d.effort))
    }

    pub(crate) fn validate_workers(&self) -> Result<(), ArchonError> {
        let Some(default) = &self.file.pipeline.workers else {
            return Ok(());
        };
        let parallel = default.max_parallel;
        if parallel.is_some_and(|n| !(1..=MAX_PARALLEL_WORKERS).contains(&n)) {
            return Err(ArchonError::Pipeline(format!(
                "pipeline {:?}: [pipeline.workers] max_parallel must be 1 to \
                 {MAX_PARALLEL_WORKERS}, not {}",
                self.name(),
                parallel.unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// What `stage`'s lead may delegate workers to under `plan`: the plan's
    /// `<stage>.workers` choices, else the pipeline default; `None` when
    /// neither names any, and Aether's own rule for unbounded leads stands.
    pub fn worker_policy(&self, stage: &StageSpec, plan: &ModelPlan) -> Option<WorkerPolicy> {
        assert!(self.stage(&stage.id).is_some(), "a stage of this pipeline");
        let default = self.file.pipeline.workers.as_ref();
        let parallel = default.and_then(|d| d.max_parallel);
        let named = plan
            .choices_for(&archon_core::model::workers_key(&stage.id))
            .filter(|choices| !choices.is_empty());
        let (workers, tier) = match (named, default) {
            (Some(choices), _) => {
                let tier = choices
                    .iter()
                    .find_map(|c| c.tier.as_ref().map(|t| t.requested))
                    .or(default.map(|d| d.tier));
                (choices.iter().map(WorkerChoice::from_plan).collect(), tier)
            }
            (None, Some(d)) => (vec![WorkerChoice::of_tier(d.tier, d.effort)], Some(d.tier)),
            (None, None) => return None,
        };
        let policy = WorkerPolicy::new(workers, tier, parallel)
            .expect("plan keys and [pipeline.workers] were validated");
        Some(policy)
    }
}

/// Refuse a workers key listing more choices than a lead is given.
pub(crate) fn check_worker_counts(plan: &ModelPlan) -> Result<(), ArchonError> {
    let crowded = plan
        .0
        .iter()
        .filter(|(key, _)| key.ends_with(WORKERS_KEY_SUFFIX))
        .find(|(_, choices)| choices.len() > MAX_WORKER_CHOICES);
    if let Some((key, choices)) = crowded {
        return Err(ArchonError::Invalid(format!(
            "model_plan {key:?} lists {} worker choices; a lead takes at most \
             {MAX_WORKER_CHOICES}",
            choices.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::{ModelChoice, workers_key};

    fn builtin(name: &str) -> PipelineSpec {
        crate::Catalog::builtin()
            .get(name)
            .expect("built-in")
            .clone()
    }

    #[test]
    fn adaptive_leads_default_to_two_balanced_workers_at_medium() {
        let spec = builtin("adaptive");
        let build = spec.stage("build").unwrap();
        let policy = spec.worker_policy(build, &ModelPlan::default()).unwrap();
        assert_eq!(
            policy.workers,
            [WorkerChoice::of_tier(Tier::Balanced, Effort::Medium)]
        );
        assert_eq!(policy.default_tier, Some(Tier::Balanced));
        assert_eq!(policy.max_effort, Some(Effort::Medium));
        assert_eq!(policy.max_parallel, Some(2));
    }

    #[test]
    fn swarm_workers_default_to_the_fast_tier() {
        let spec = builtin("swarm");
        let integrate = spec.stage("integrate").unwrap();
        let policy = spec
            .worker_policy(integrate, &ModelPlan::default())
            .unwrap();
        assert_eq!(policy.default_tier, Some(Tier::Fast));
    }

    #[test]
    fn a_plan_s_workers_key_replaces_the_default_and_keeps_its_parallel_cap() {
        let spec = builtin("adaptive");
        let build = spec.stage("build").unwrap();
        let mut plan = ModelPlan::default();
        let named = ModelChoice::pinned("codex/terra".parse().unwrap(), Effort::Low);
        plan.0.insert(workers_key("build"), vec![named.clone()]);
        let policy = spec.worker_policy(build, &plan).unwrap();
        assert_eq!(policy.workers, [WorkerChoice::from_plan(&named)]);
        assert_eq!(policy.max_effort, Some(Effort::Low));
        assert_eq!(policy.max_parallel, Some(2));
        assert_eq!(spec.default_effort("build.workers"), Some(Effort::Medium));
    }

    #[test]
    fn workers_keys_name_a_stage_and_a_few_choices() {
        let spec = builtin("adaptive");
        assert!(spec.is_plan_key("build.workers"));
        assert!(!spec.is_plan_key("plan.workers"), "no such stage");
        let choice = ModelChoice::pinned("codex/terra".parse().unwrap(), Effort::Low);
        let mut plan = ModelPlan::default();
        plan.0.insert("build".into(), vec![choice.clone()]);
        plan.0.insert(
            workers_key("build"),
            vec![choice.clone(); MAX_WORKER_CHOICES],
        );
        assert!(spec.check_plan_keys(&plan).is_ok());
        plan.0.insert(
            workers_key("build"),
            vec![choice.clone(); MAX_WORKER_CHOICES + 1],
        );
        let crowded = spec.check_plan_keys(&plan).unwrap_err().to_string();
        assert!(crowded.contains("at most 4"), "{crowded}");
        plan.0.remove(&workers_key("build"));
        plan.0.insert(workers_key("ship"), vec![choice]);
        let unknown = spec.check_plan_keys(&plan).unwrap_err().to_string();
        assert!(unknown.contains("\"ship.workers\""), "{unknown}");
    }

    #[test]
    fn a_pipeline_without_a_worker_default_leaves_its_leads_unbounded() {
        let spec = builtin("direct");
        let stage = &spec.file.stages[0];
        assert_eq!(spec.worker_policy(stage, &ModelPlan::default()), None);
    }

    #[test]
    fn a_parallel_cap_out_of_range_fails_validation() {
        let mut spec = builtin("adaptive");
        spec.file.pipeline.workers.as_mut().unwrap().max_parallel = Some(0);
        assert!(spec.validate("adaptive").is_err());
        spec.file.pipeline.workers.as_mut().unwrap().max_parallel = Some(MAX_PARALLEL_WORKERS);
        assert!(spec.validate("adaptive").is_ok());
    }
}
