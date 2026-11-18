use archon_core::error::ArchonError;
use archon_core::model::ModelPlan;
use archon_core::model_tier::ModelCatalog;
use archon_core::queue::QueueItem;
use archon_pipeline::SpawnPlan;

/// Repair an item's plan using its current pipeline, policy and learned defaults.
pub(crate) fn repair_item(
    ctx: &crate::context::DaemonContext,
    store: &archon_queue::Store,
    item: &QueueItem,
    catalog: &ModelCatalog,
    env: &crate::resolve::RepoEnv,
    spec: &archon_pipeline::PipelineSpec,
) -> Result<QueueItem, ArchonError> {
    let policy = env.resolved_policy(ctx);
    let max_tier = env.max_default_tier(&ctx.config);
    let tiers = crate::knobs::TierScope::new(&ctx.config, &policy, catalog);
    if !item.model_plan.needs_catalog_repair(&item.id, catalog) {
        let mut repaired = item.clone();
        repaired
            .model_plan
            .repair_catalog(&item.id, catalog, archon_core::now())?;
        return Ok(repaired);
    }
    let learned = if catalog.not_ready_reason().is_none() {
        crate::learned::default_plan(store, item.repo.as_ref(), spec, &policy, max_tier)?
    } else {
        ModelPlan::default()
    };
    let fallback = crate::knobs::default_choice(&ctx.config, tiers, max_tier, spec);
    let selection = Replacement {
        tiers,
        spec,
        learned: &learned,
        fallback: fallback.as_ref(),
        max_tier,
    };
    let mut repaired = item.clone();
    repaired.model_plan.repair_catalog_with(
        &item.id,
        catalog,
        archon_core::now(),
        |key, slot, choice, _| selection.choose(key, slot, choice),
    )?;
    assert_eq!(repaired.id, item.id);
    Ok(repaired)
}

struct Replacement<'a> {
    tiers: crate::knobs::TierScope<'a>,
    spec: &'a archon_pipeline::PipelineSpec,
    learned: &'a ModelPlan,
    fallback: Option<&'a archon_core::model::ModelChoice>,
    max_tier: archon_core::model_tier::Tier,
}

impl Replacement<'_> {
    fn choose(
        &self,
        key: &str,
        slot: usize,
        choice: &archon_core::model::ModelChoice,
    ) -> Result<Option<archon_core::plan_origin::CatalogReplacement>, ArchonError> {
        if let Some(tier) = &choice.tier {
            let floor = self.spec.tier_floor(key, tier.requested);
            let (model, resolved) = self
                .tiers
                .resolve((tier.requested, floor), tier.provider.as_deref())?;
            return Ok((resolved.resolved >= floor).then_some((model, Some(resolved))));
        }
        let catalog = self.tiers.catalog;
        let successor = catalog
            .successor(&choice.model)
            .or_else(|| catalog.superseded_by(&choice.model));
        let learned = self
            .learned
            .choices_for(key)
            .and_then(|choices| choices.get(slot % choices.len().max(1)))
            .map(|choice| &choice.model);
        let fallback = self.fallback.map(|choice| &choice.model);
        let model = [successor, learned, fallback]
            .into_iter()
            .flatten()
            .find(|model| self.eligible(model))
            .cloned();
        Ok(model.map(|model| (model, None)))
    }

    fn eligible(&self, model: &archon_core::model::ModelRef) -> bool {
        let catalog = self.tiers.catalog;
        self.tiers.policy.permits_model(model)
            && catalog.check(model).is_ok()
            && catalog.superseded_by(model).is_none()
            && catalog
                .tier_of(model)
                .is_none_or(|tier| tier <= self.max_tier)
    }
}

/// Repair the stored choices and their already-emitted pending counterparts.
pub(crate) fn repair_pending(
    ctx: &crate::context::DaemonContext,
    store: &archon_queue::Store,
    item: &QueueItem,
    plans: &[SpawnPlan],
    catalog: &ModelCatalog,
    env: &crate::resolve::RepoEnv,
    spec: &archon_pipeline::PipelineSpec,
) -> Result<(QueueItem, Vec<SpawnPlan>), ArchonError> {
    let repaired = repair_item(ctx, store, item, catalog, env, spec)?;
    let before = item.model_plan.plan();
    let after = repaired.model_plan.plan();
    let mut pending = plans.to_vec();
    for plan in &mut pending {
        repair_stage(plan, before, after);
        validate_spawn(plan, catalog)?;
    }
    assert_eq!(pending.len(), plans.len());
    assert_eq!(repaired.id, item.id);
    Ok((repaired, pending))
}

pub(super) fn repair_stage(plan: &mut SpawnPlan, before: &ModelPlan, after: &ModelPlan) {
    if let (Some(old), Some(new)) = (
        before.choices_for(&plan.stage_id),
        after.choices_for(&plan.stage_id),
    ) && !old.is_empty()
        && old.len() == new.len()
    {
        let slot = plan.fan_index as usize % old.len();
        if plan.choice.model == old[slot].model {
            plan.choice.model = new[slot].model.clone();
        }
    }
    if let (Some(named), Some(old), Some(new)) = (
        plan.reviewers.as_mut(),
        before.reviewer_choices(),
        after.reviewer_choices(),
    ) && named.len() == old.len()
        && old.len() == new.len()
    {
        for ((choice, was), now) in named.iter_mut().zip(old).zip(new) {
            if choice.model == was.model {
                choice.model = now.model.clone();
            }
        }
    }
    if let (Some(policy), Some(old), Some(new)) = (
        plan.workers.as_mut(),
        before.reviewer_choices(),
        after.reviewer_choices(),
    ) && old.len() == new.len()
    {
        for model in &mut policy.exempt_models {
            let mut matches = old.iter().zip(new).filter(|(was, _)| &was.model == model);
            if let Some((_, now)) = matches.next()
                && matches.all(|(_, other)| other.model == now.model)
            {
                *model = now.model.clone();
            }
        }
    }
    let key = archon_core::model::workers_key(&plan.stage_id);
    if let (Some(policy), Some(old), Some(new)) = (
        plan.workers.as_mut(),
        before.choices_for(&key),
        after.choices_for(&key),
    ) && policy.workers.len() == old.len()
        && old.len() == new.len()
    {
        for ((choice, was), now) in policy.workers.iter_mut().zip(old).zip(new) {
            if choice.model.as_ref() == Some(&was.model) {
                choice.model = Some(now.model.clone());
            }
        }
    }
}

fn validate_spawn(plan: &SpawnPlan, catalog: &ModelCatalog) -> Result<(), ArchonError> {
    let choices = std::iter::once(&plan.choice.model)
        .chain(plan.reviewers.iter().flatten().map(|choice| &choice.model));
    let workers = plan.workers.iter().flat_map(|policy| {
        policy
            .workers
            .iter()
            .filter_map(|choice| choice.model.as_ref())
            .chain(policy.exempt_models.iter())
    });
    for model in choices.chain(workers) {
        catalog.check(model)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::catalog::{StageKind, WorkspaceMode};
    use archon_core::delegation::{WorkerChoice, WorkerPolicy};
    use archon_core::model::{Effort, ModelChoice, ModelRef};
    use std::collections::BTreeMap;

    fn choice(model: &str) -> ModelChoice {
        ModelChoice::pinned(model.parse().unwrap(), Effort::Medium)
    }

    #[test]
    fn repairs_each_emitted_choice_and_preserves_unrelated_models() {
        let old = choice("p/old");
        let new = choice("p/new");
        let other = choice("p/other");
        let before = ModelPlan(BTreeMap::from([
            ("build".into(), vec![other.clone(), old.clone()]),
            ("build.review".into(), vec![old.clone()]),
            ("build.workers".into(), vec![old.clone()]),
        ]));
        let after = ModelPlan(BTreeMap::from([
            ("build".into(), vec![other.clone(), new.clone()]),
            ("build.review".into(), vec![new.clone()]),
            ("build.workers".into(), vec![new.clone()]),
        ]));
        let mut spawn = SpawnPlan {
            stage_id: "build".into(),
            kind: StageKind::Work,
            attempt: 1,
            fan_index: 1,
            fan_count: 2,
            choice: old.clone(),
            prompt: "task".into(),
            instructions: "contract".into(),
            timeout_minutes: 1,
            allow_delegation: true,
            workspace: WorkspaceMode::Scratch,
            team: None,
            keep: false,
            resume: None,
            reviewers: Some(vec![old.clone()]),
            workers: Some(
                WorkerPolicy::new(vec![WorkerChoice::from_plan(&old)], None, None)
                    .unwrap()
                    .with_reviewers(vec![old.model.clone()]),
            ),
            reports_requirements: false,
        };
        repair_stage(&mut spawn, &before, &after);
        let expected: ModelRef = "p/new".parse().unwrap();
        assert_eq!(spawn.choice.model, expected);
        assert_eq!(spawn.reviewers.unwrap()[0].model, expected);
        let workers = spawn.workers.unwrap();
        assert_eq!(workers.workers[0].model.as_ref(), Some(&expected));
        assert_eq!(workers.exempt_models, vec![expected]);
        assert_eq!(before.0["build"][0], other);
    }
}
