use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model::ModelPlan;
use archon_core::outcome::DiffSize;
use archon_core::policy::ModelPolicy;
use archon_pipeline::PipelineSpec;
use archon_queue::{Store, store::RouteEvidence};
use std::collections::BTreeMap;

/// What the evidence of `repo` picks for the stages of `spec` within
/// `policy`, pooled over every diff size; empty when nothing is learned.
pub(crate) fn default_plan(
    store: &Store,
    repo: Option<&RepoId>,
    spec: &PipelineSpec,
    policy: &ModelPolicy,
) -> Result<ModelPlan, ArchonError> {
    let (plan, _) = select_plan(store, repo, spec, policy, None, None)?;
    assert!(
        plan.0.keys().all(|stage| spec.stage(stage).is_some()),
        "evidence is read per stage of the pipeline"
    );
    policy.check_plan(&plan)?;
    Ok(plan)
}

/// The learned choices for the stages `explicit` leaves out, kept for the
/// item with the evidence behind each so `lessons why` can show it.
pub(crate) fn plan_for_item(
    store: &Store,
    item: &QueueItemId,
    repo: Option<&RepoId>,
    spec: &PipelineSpec,
    policy: &ModelPolicy,
    explicit: &ModelPlan,
) -> Result<ModelPlan, ArchonError> {
    if !store.learning_config()?.enabled {
        return Ok(ModelPlan::default());
    }
    let key = format!("learning/routing/{item}");
    let saved_key = format!("learning/choices/{item}");
    let saved = store
        .kv_get(&saved_key)?
        .map(|raw| {
            serde_json::from_str::<(ModelPlan, BTreeMap<String, String>)>(&raw)
                .map_err(|e| ArchonError::Store(format!("stored model defaults: {e}")))
        })
        .transpose()?;
    let (mut plan, mut reasons) = match saved {
        Some(saved) if policy.check_plan(&saved.0).is_ok() => saved,
        _ => select_plan(
            store,
            repo,
            spec,
            policy,
            Some(item),
            item_size(store, item)?,
        )?,
    };
    plan.0.retain(|stage, _| spec.stage(stage).is_some());
    reasons.retain(|stage, _| plan.0.contains_key(stage));
    for (stage, choices) in &explicit.0 {
        if plan.0.get(stage) != Some(choices) {
            reasons.remove(stage);
        }
    }
    store.save_learning_value(
        &saved_key,
        &serde_json::to_string(&(plan.clone(), reasons.clone()))
            .map_err(|e| ArchonError::Store(format!("encode learned plan: {e}")))?,
    )?;
    plan.0.retain(|stage, _| !explicit.0.contains_key(stage));
    assert!(plan.0.keys().all(|stage| !explicit.0.contains_key(stage)));
    policy.check_plan(&plan)?;
    store.save_learning_value(
        &key,
        &serde_json::to_string(&reasons)
            .map_err(|e| ArchonError::Store(format!("encode routing evidence: {e}")))?,
    )?;
    Ok(plan)
}

pub(crate) fn item_size(
    store: &Store,
    item: &QueueItemId,
) -> Result<Option<DiffSize>, ArchonError> {
    if let Some(raw) = store.kv_get(&format!("learning/size/{item}"))? {
        return serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| ArchonError::Store(format!("diff-size hint: {e}")));
    }
    let item = match store.item(item) {
        Ok(item) => item,
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    for tag in &item.tags {
        match tag.as_str() {
            "diff:small" => return Ok(Some(DiffSize::Small)),
            "diff:medium" => return Ok(Some(DiffSize::Medium)),
            "diff:large" => return Ok(Some(DiffSize::Large)),
            _ => {}
        }
    }
    let runs = store.runs_for_item(&item.id)?;
    runs.last()
        .map(|r| store.outcome(&r.id))
        .transpose()
        .map(|o| o.flatten().and_then(|o| o.size()))
}

fn select_plan(
    store: &Store,
    repo: Option<&RepoId>,
    spec: &PipelineSpec,
    policy: &ModelPolicy,
    item: Option<&QueueItemId>,
    size: Option<DiffSize>,
) -> Result<(ModelPlan, BTreeMap<String, String>), ArchonError> {
    let config = store.learning_config()?;
    let mut plan = ModelPlan::default();
    let mut reasons = BTreeMap::new();
    if !config.enabled {
        return Ok((plan, reasons));
    }
    for stage in &spec.file.stages {
        let mut evidence = store.route_evidence(repo, spec.name(), &stage.id, size)?;
        evidence.retain(|e| policy.check(&e.route.choice).is_ok());
        if !evidence.iter().any(|e| e.route.runs >= 5) && repo.is_some() {
            evidence = store.route_evidence(None, spec.name(), &stage.id, size)?;
            evidence.retain(|e| policy.check(&e.route.choice).is_ok());
        }
        let exploring = item.is_some_and(|id| exploration(id, config.explore_rate));
        let Some((chosen, explored)) = choose(&evidence, exploring) else {
            continue;
        };
        let route = &chosen.route;
        policy.check(&route.choice)?;
        assert!(route.passed <= route.runs);
        reasons.insert(
            stage.id.clone(),
            format!(
                "{} {}: {}/{} passed (n={}), median ${:.4}; {} evidence{}; diff-size={}",
                route.choice.model,
                route.choice.effort,
                route.passed,
                route.runs,
                route.runs,
                chosen.median_cost_usd,
                if route.repo.is_some() {
                    "repository"
                } else {
                    "global"
                },
                if explored {
                    "; cheaper-neighbour exploration"
                } else {
                    "; within 5 points of best"
                },
                size.map_or("unknown", |s| s.as_str())
            ),
        );
        plan.0.insert(stage.id.clone(), vec![route.choice.clone()]);
    }
    Ok((plan, reasons))
}

fn choose(evidence: &[RouteEvidence], explore: bool) -> Option<(&RouteEvidence, bool)> {
    let best = evidence
        .iter()
        .filter(|e| e.route.runs >= 5)
        .map(pass_rate)
        .max_by(f64::total_cmp)?;
    let selected = evidence
        .iter()
        .position(|e| e.route.runs >= 5 && pass_rate(e) + 0.05 + f64::EPSILON >= best)?;
    let cheaper = evidence[..selected]
        .iter()
        .rposition(|e| e.median_cost_usd < evidence[selected].median_cost_usd);
    let index = if explore {
        cheaper.unwrap_or(selected)
    } else {
        selected
    };
    assert!(evidence[index].median_cost_usd <= evidence[selected].median_cost_usd);
    assert!(evidence[index].route.runs > 0);
    Some((&evidence[index], index != selected))
}

fn pass_rate(e: &RouteEvidence) -> f64 {
    f64::from(e.route.passed) / f64::from(e.route.runs)
}

fn exploration(item: &QueueItemId, rate: f64) -> bool {
    assert!((0.0..=0.1).contains(&rate));
    assert!(!item.as_str().is_empty());
    let hash = item
        .as_str()
        .bytes()
        .fold(14695981039346656037u64, |hash, b| {
            (hash ^ u64::from(b)).wrapping_mul(1099511628211)
        });
    hash % 10_000 < (rate * 10_000.0) as u64
}

pub(crate) fn hint_size(
    store: &Store,
    item: &QueueItemId,
    tags: &[String],
) -> Result<(), ArchonError> {
    if !store.learning_config()?.enabled {
        return Ok(());
    }
    let mut size = None;
    for tag in tags {
        let next = match tag.as_str() {
            "diff:small" => Some(DiffSize::Small),
            "diff:medium" => Some(DiffSize::Medium),
            "diff:large" => Some(DiffSize::Large),
            _ => None,
        };
        if next.is_some() && size.is_some() && size != next {
            return Err(ArchonError::Invalid("conflicting diff-size tags".into()));
        }
        size = next.or(size);
    }
    if let Some(size) = size {
        store.save_learning_value(
            &format!("learning/size/{item}"),
            &serde_json::to_string(&size)
                .map_err(|e| ArchonError::Store(format!("diff-size hint: {e}")))?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::{Effort, ModelChoice};
    use archon_core::outcome::LearnedRoute;
    fn evidence(name: &str, cost: f64, runs: u32, passed: u32) -> RouteEvidence {
        RouteEvidence {
            route: LearnedRoute {
                repo: None,
                pipeline: "p".into(),
                stage: "s".into(),
                size: None,
                choice: ModelChoice {
                    model: format!("codex/{name}").parse().unwrap(),
                    effort: Effort::Low,
                },
                runs,
                passed,
            },
            median_cost_usd: cost,
        }
    }
    #[test]
    fn routing_needs_five_runs_and_explores_only_the_nearest_cheaper_choice() {
        let options = vec![
            evidence("tiny", 0.01, 4, 4),
            evidence("cheaper", 0.1, 5, 3),
            evidence("balanced", 0.2, 20, 19),
            evidence("best", 1.0, 20, 20),
        ];
        let (normal, explored) = choose(&options, false).unwrap();
        assert_eq!(normal.route.choice.model.model, "balanced");
        assert!(!explored);
        let (trial, explored) = choose(&options, true).unwrap();
        assert_eq!(trial.route.choice.model.model, "cheaper");
        assert!(explored);
        assert!(choose(&options[..1], false).is_none());
    }
    #[test]
    fn exploration_is_stable_and_ten_percent_is_a_hard_configuration_ceiling() {
        let ids: Vec<_> = (0..10_000)
            .map(|n| QueueItemId::parse(&format!("qi_{n:026}")).unwrap())
            .collect();
        let count = ids.iter().filter(|id| exploration(id, 0.1)).count();
        assert!((850..1150).contains(&count), "{count}");
        for id in &ids {
            assert_eq!(exploration(id, 0.1), exploration(id, 0.1));
            assert!(!exploration(id, 0.0));
        }
        assert!(
            archon_core::config::LearningConfig {
                explore_rate: 0.11,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
