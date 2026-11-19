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
    repair_item_in(ctx, store, item, catalog, env, (spec, &|_| true))
}

/// [`repair_item`] for the plan keys `in_scope` accepts; the rest are kept as stored.
fn repair_item_in(
    ctx: &crate::context::DaemonContext,
    store: &archon_queue::Store,
    item: &QueueItem,
    catalog: &ModelCatalog,
    env: &crate::resolve::RepoEnv,
    (spec, in_scope): (&archon_pipeline::PipelineSpec, &dyn Fn(&str) -> bool),
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
    repaired.model_plan.repair_catalog_in(
        &item.id,
        catalog,
        (archon_core::now(), in_scope),
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
    let keys: std::collections::BTreeSet<String> = plans
        .iter()
        .flat_map(|plan| {
            [
                plan.stage_id.clone(),
                archon_core::model::workers_key(&plan.stage_id),
            ]
        })
        .collect();
    // A finished stage's pins no longer run: only the pending stages' keys are repaired.
    let in_scope =
        |key: &str| keys.contains(key) || key.ends_with(archon_core::model::REVIEWERS_KEY_SUFFIX);
    let repaired = repair_item_in(ctx, store, item, catalog, env, (spec, &in_scope))?;
    let before = item.model_plan.plan();
    let after = repaired.model_plan.plan();
    let mut pending = plans.to_vec();
    for plan in &mut pending {
        repair_spawn(plan, (before, after), catalog)?;
    }
    assert_eq!(pending.len(), plans.len());
    assert_eq!(repaired.id, item.id);
    Ok((repaired, pending))
}

pub(super) const RETRY_WAIT: &str = "model catalog holds stage replacement";

pub(super) fn retry_plan(
    ctx: &crate::context::DaemonContext,
    store: &mut archon_queue::Store,
    table: &mut super::RunTable,
    run: &archon_core::run::PipelineRun,
    plan: SpawnPlan,
    catalog: &ModelCatalog,
    spec: &archon_pipeline::PipelineSpec,
) -> Result<SpawnPlan, ArchonError> {
    if crate::learned::cached_catalog(ctx, store)?.as_ref() != Some(catalog) {
        return Err(ArchonError::Unavailable(format!(
            "{RETRY_WAIT}: the catalog changed"
        )));
    }
    let item = store.item(&run.item)?;
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&crate::resolve::repo_ref(id)))
        .transpose()?;
    let env = crate::resolve::RepoEnv::new(repo)?;
    let (mut repaired, mut plans) = repair_pending(
        ctx,
        store,
        &item,
        std::slice::from_ref(&plan),
        catalog,
        &env,
        spec,
    )
    .map_err(|error| {
        if error.is_store() {
            error
        } else {
            ArchonError::Unavailable(format!("{RETRY_WAIT}: {error}"))
        }
    })?;
    if repaired.model_plan != item.model_plan {
        repaired.updated_at = archon_core::now().max(item.updated_at);
        store.update_item(&repaired)?;
        follow_models(
            table,
            &run.id,
            item.model_plan.plan(),
            repaired.model_plan.plan(),
        );
    }
    assert_eq!(plans.len(), 1, "one retry plan");
    Ok(plans.pop().expect("checked above"))
}

pub(super) fn follow_models(
    table: &mut super::RunTable,
    run: &archon_core::id::PipelineRunId,
    before: &ModelPlan,
    after: &ModelPlan,
) {
    if let Some(driver) = table.drivers.get_mut(run) {
        driver.follow_models(before, after);
    }
    if let Some(pending) = table.pending.get_mut(run) {
        for plan in pending {
            repair_stage(plan, before, after);
        }
    }
}

pub(super) fn repair_stage(plan: &mut SpawnPlan, before: &ModelPlan, after: &ModelPlan) {
    if let (Some(old), Some(new)) = (
        before.choices_for(&plan.stage_id),
        after.choices_for(&plan.stage_id),
    ) && !old.is_empty()
        && old.len() == new.len()
    {
        let slot = plan.fan_index as usize % old.len();
        plan.choice.follow(&old[slot], &new[slot]);
    }
    if let (Some(named), Some(old), Some(new)) = (
        plan.reviewers.as_mut(),
        before.reviewer_choices(),
        after.reviewer_choices(),
    ) && named.len() == old.len()
        && old.len() == new.len()
    {
        for ((choice, was), now) in named.iter_mut().zip(old).zip(new) {
            choice.follow(was, now);
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

/// Move one emitted plan with its stored entries, matched by canonical id, and
/// refuse it if a model it names is still not listed.
fn repair_spawn(
    plan: &mut SpawnPlan,
    (before, after): (&ModelPlan, &ModelPlan),
    catalog: &ModelCatalog,
) -> Result<(), ArchonError> {
    canonicalize_spawn(plan, catalog);
    repair_stage(plan, before, after);
    validate_spawn(plan, catalog)
}

/// Each model `plan` names, mutably: its choice, reviewers and workers.
fn spawn_models(plan: &mut SpawnPlan) -> impl Iterator<Item = &mut archon_core::model::ModelRef> {
    let choices = std::iter::once(&mut plan.choice.model).chain(
        plan.reviewers
            .iter_mut()
            .flatten()
            .map(|choice| &mut choice.model),
    );
    let workers = plan.workers.iter_mut().flat_map(|policy| {
        policy
            .workers
            .iter_mut()
            .filter_map(|choice| choice.model.as_mut())
            .chain(policy.exempt_models.iter_mut())
    });
    choices.chain(workers)
}

/// Name each alias by its canonical id, as the stored plan it is matched with does.
fn canonicalize_spawn(plan: &mut SpawnPlan, catalog: &ModelCatalog) {
    assert!(
        catalog.not_ready_reason().is_none(),
        "aliases come from a ready catalog"
    );
    for model in spawn_models(plan) {
        let canonical = catalog.canonical_alias(model).clone();
        *model = canonical;
    }
}

/// Refuse a model the catalog does not list, keeping each canonical id it returns.
fn validate_spawn(plan: &mut SpawnPlan, catalog: &ModelCatalog) -> Result<(), ArchonError> {
    for model in spawn_models(plan) {
        let canonical = catalog.check(model)?;
        assert_eq!(
            &canonical,
            catalog.canonical_alias(&canonical),
            "a checked id is canonical"
        );
        *model = canonical;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::catalog::{StageKind, WorkspaceMode};
    use archon_core::delegation::{WorkerChoice, WorkerPolicy};
    use archon_core::model::{Effort, ModelChoice, ModelRef};
    use archon_core::model_tier::Tier;
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
            record_repair: archon_core::run::RecordRepair::Open,
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

    fn spawn(choice: ModelChoice) -> SpawnPlan {
        SpawnPlan {
            stage_id: "build".into(),
            kind: StageKind::Work,
            attempt: 1,
            fan_index: 0,
            fan_count: 1,
            choice,
            prompt: "task".into(),
            instructions: "contract".into(),
            timeout_minutes: 1,
            allow_delegation: false,
            workspace: WorkspaceMode::Scratch,
            team: None,
            keep: false,
            resume: None,
            reviewers: None,
            workers: None,
            reports_requirements: false,
            record_repair: archon_core::run::RecordRepair::Open,
        }
    }

    fn plan(choice: &ModelChoice) -> ModelPlan {
        ModelPlan(BTreeMap::from([("build".into(), vec![choice.clone()])]))
    }

    fn tiered(model: &str, requested: Tier, resolved: Tier) -> ModelChoice {
        let tier = archon_core::model_tier::TierChoice {
            requested,
            provider: None,
            resolved,
        };
        ModelChoice {
            tier: Some(tier),
            ..choice(model)
        }
    }

    #[test]
    fn a_replacement_carries_its_tier_unless_the_copy_set_its_own() {
        let old = tiered("p/old", Tier::Strong, Tier::Strong);
        let new = tiered("p/new", Tier::Strong, Tier::Balanced);
        let mut follows = spawn(old.clone());
        repair_stage(&mut follows, &plan(&old), &plan(&new));
        assert_eq!(
            follows.choice, new,
            "the copy follows its entry, tier and all"
        );
        let override_tier = tiered("p/old", Tier::Fast, Tier::Fast);
        let mut own = spawn(override_tier.clone());
        repair_stage(&mut own, &plan(&old), &plan(&new));
        assert_eq!(own.choice.model, new.model, "the retired model is replaced");
        assert_eq!(
            own.choice.tier, override_tier.tier,
            "an override keeps its tier"
        );
    }

    #[test]
    fn a_pending_alias_is_repaired_as_its_canonical_id() {
        let (old, new) = (choice("p/old"), choice("p/new"));
        let mut catalog = ModelCatalog::new(
            ["p/new", "p/old"]
                .map(|id| archon_core::model_tier::CatalogModel {
                    model: id.parse().unwrap(),
                    tier: None,
                    is_default: false,
                })
                .to_vec(),
        );
        catalog.add_alias("p/alias".parse().unwrap(), old.model.clone());
        let mut pending = spawn(choice("p/alias"));
        repair_spawn(&mut pending, (&plan(&old), &plan(&new)), &catalog).unwrap();
        assert_eq!(
            pending.choice.model, new.model,
            "the alias follows its entry"
        );
        let mut kept = spawn(choice("p/alias"));
        repair_spawn(&mut kept, (&plan(&new), &plan(&new)), &catalog).unwrap();
        assert_eq!(
            kept.choice.model, old.model,
            "a kept alias is sent canonical"
        );
    }
}
