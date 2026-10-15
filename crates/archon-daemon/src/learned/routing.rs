use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::feedback::ChoiceStanding;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model::{ModelChoice, ModelPlan, ModelRef};
use archon_core::model_tier::Tier;
use archon_core::outcome::DiffSize;
use archon_core::policy::ModelPolicy;
use archon_pipeline::PipelineSpec;
use archon_queue::Store;
use archon_queue::store::{CohortEvidence, RouteEvidence, RouteQuery};
use std::collections::BTreeMap;

/// What the evidence of `repo` picks for the stages of `spec` within
/// `policy`, pooled over every diff size; empty when nothing is learned.
pub(crate) fn default_plan(
    store: &Store,
    repo: Option<&RepoId>,
    spec: &PipelineSpec,
    policy: &ModelPolicy,
    max_tier: Tier,
) -> Result<ModelPlan, ArchonError> {
    let (plan, _) = select_plan(store, repo, spec, (policy, max_tier), None, None)?;
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
    (policy, max_tier): (&ModelPolicy, Tier),
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
            (policy, max_tier),
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
    (policy, max_tier): (&ModelPolicy, Tier),
    item: Option<&QueueItemId>,
    size: Option<DiffSize>,
) -> Result<(ModelPlan, BTreeMap<String, String>), ArchonError> {
    let config = store.learning_config()?;
    let mut plan = ModelPlan::default();
    let mut reasons = BTreeMap::new();
    if !config.enabled {
        return Ok((plan, reasons));
    }
    let as_of = archon_core::now();
    let context = route_context(store, item)?;
    for stage in &spec.file.stages {
        let query = RouteQuery {
            repo,
            pipeline: spec.name(),
            stage: &stage.id,
            size,
            context: context.as_ref(),
        };
        let mut cohort = stage_evidence(store, &query, (policy, max_tier), as_of)?;
        if let Some(item) = item {
            cohort.routes.sort_by_key(|e| {
                super::routing_context::fingerprint(&format!(
                    "{item}/{}/{}",
                    stage.id, e.route.choice.model
                ))
            });
        }
        // A cohort under an active operator correction gets no exploration.
        let exploring =
            !cohort.corrected && item.is_some_and(|id| exploration(id, config.explore_rate));
        let Some((chosen, explored)) = choose(&cohort.routes, exploring) else {
            continue;
        };
        policy.check(&chosen.route.choice)?;
        assert!(chosen.route.passed <= chosen.route.runs);
        assert!(!explored || !cohort.corrected);
        reasons.insert(
            stage.id.clone(),
            explanation(chosen, explored, &query, &cohort),
        );
        plan.0
            .insert(stage.id.clone(), vec![chosen.route.choice.clone()]);
    }
    Ok((plan, reasons))
}

fn route_context(
    store: &Store,
    item: Option<&QueueItemId>,
) -> Result<Option<archon_core::outcome::RouteContext>, ArchonError> {
    item.map(|id| store.kv_get(&format!("learning/context/{id}")))
        .transpose()?
        .flatten()
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(|e| ArchonError::Store(format!("routing context: {e}")))
}

/// A stage's permitted evidence, on models no higher than `max_tier` in
/// Aether's catalog. A thin repository cohort pools global evidence only
/// while it has no typed operator feedback.
fn stage_evidence(
    store: &Store,
    query: &RouteQuery<'_>,
    (policy, max_tier): (&ModelPolicy, Tier),
    as_of: Timestamp,
) -> Result<CohortEvidence, ArchonError> {
    let mut cohort = store.cohort_evidence(query, as_of)?;
    let thin = !cohort.routes.iter().any(|e| e.route.runs >= 5);
    if thin && query.repo.is_some() && !cohort.feedback_history {
        let global = RouteQuery {
            repo: None,
            ..*query
        };
        cohort.routes = store.cohort_evidence(&global, as_of)?.routes;
    }
    seed(store, &mut cohort.routes, query, policy);
    let catalog = store.model_catalog();
    let within = |m: &ModelRef| catalog.tier_of(m).is_none_or(|tier| tier <= max_tier);
    cohort
        .routes
        .retain(|e| policy.check(&e.route.choice).is_ok() && within(&e.route.choice.model));
    assert!(cohort.routes.iter().all(|e| e.route.stage == query.stage));
    assert!(cohort.routes.iter().all(|e| e.route.passed <= e.route.runs));
    let local = cohort.routes.iter().all(|e| e.route.repo.is_some());
    assert!(!cohort.feedback_history || local, "corrections never pool");
    Ok(cohort)
}

/// The corrected choice's samples in its cohort, read past the policy, and
/// what a new default of the cohort selects now.
pub(crate) fn standings(
    store: &Store,
    query: &RouteQuery<'_>,
    (policy, max_tier): (&ModelPolicy, Tier),
    corrected: Option<&ModelChoice>,
    as_of: Timestamp,
) -> Result<(Option<ChoiceStanding>, Option<ChoiceStanding>), ArchonError> {
    let standing = |e: &RouteEvidence| ChoiceStanding {
        choice: e.route.choice.clone(),
        runs: e.route.runs,
        passed: e.route.passed,
    };
    let corrected = match corrected {
        Some(choice) => {
            let routes = store.cohort_evidence(query, as_of)?.routes;
            routes
                .iter()
                .find(|e| &e.route.choice == choice)
                .map(standing)
        }
        None => None,
    };
    let cohort = stage_evidence(store, query, (policy, max_tier), as_of)?;
    let selected = archon_queue::store::preferred_route(&cohort.routes).map(standing);
    assert!(corrected.as_ref().is_none_or(|c| c.passed <= c.runs));
    assert!(selected.as_ref().is_none_or(|s| s.runs >= 5));
    Ok((corrected, selected))
}

fn seed(
    store: &Store,
    evidence: &mut Vec<RouteEvidence>,
    query: &RouteQuery<'_>,
    policy: &ModelPolicy,
) {
    use archon_core::{model::Effort, outcome::LearnedRoute};
    let ready = store.learning_models();
    if !ready.is_empty() {
        evidence.retain(|e| ready.contains(&e.route.choice.model));
    }
    for model in ready {
        let choice = ModelChoice {
            model,
            effort: policy
                .max_effort
                .unwrap_or(Effort::Medium)
                .min(Effort::Medium),
            tier: None,
        };
        if policy.check(&choice).is_err()
            || evidence
                .iter()
                .any(|e| e.route.choice.model == choice.model)
        {
            continue;
        }
        evidence.push(RouteEvidence {
            median_cost_usd: 0.0,
            route: LearnedRoute {
                repo: query.repo.cloned(),
                pipeline: query.pipeline.into(),
                stage: query.stage.into(),
                size: query.size,
                choice,
                runs: 0,
                passed: 0,
            },
        });
    }
}

fn explanation(
    e: &RouteEvidence,
    explored: bool,
    query: &RouteQuery<'_>,
    cohort: &CohortEvidence,
) -> String {
    let (lower, upper) = wilson(e);
    let context = query.context;
    let feedback = if !cohort.penalties.is_empty() {
        let ids: Vec<_> = cohort.penalties.iter().map(|id| id.as_str()).collect();
        format!(
            "; after operator corrections {} (no exploration)",
            ids.join(", ")
        )
    } else if cohort.corrected {
        "; under an active operator correction (no exploration)".to_owned()
    } else if cohort.feedback_history {
        "; runs of the last 90 days (operator feedback history)".to_owned()
    } else {
        String::new()
    };
    format!(
        "{} {}: {}/{} passed (n={}), Wilson 95% [{lower:.2}, {upper:.2}], median ${:.4}; {} evidence; {}; diff-size={}; review={}; revision={}; task={}{feedback}",
        e.route.choice.model,
        e.route.choice.effort,
        e.route.passed,
        e.route.runs,
        e.route.runs,
        e.median_cost_usd,
        if e.route.repo.is_some() {
            "repository"
        } else {
            "global"
        },
        if explored {
            "bounded uncertainty exploration"
        } else {
            "conservative quality bound with cost"
        },
        query.size.map_or("unknown (pooled)", |s| s.as_str()),
        context.map_or("unknown", |c| &c.review),
        context.map_or("unknown", |c| &c.revision),
        context.map_or("general", |c| &c.task_class)
    )
}

fn choose(evidence: &[RouteEvidence], explore: bool) -> Option<(&RouteEvidence, bool)> {
    let selected = archon_queue::store::preferred_route(evidence);
    if !explore {
        return selected.map(|e| (e, false));
    }
    let floor = selected.map_or(0.3, |e| wilson(e).0.max(0.3));
    let trial = evidence
        .iter()
        .filter(|e| e.route.runs < 5 || wilson(e).1 >= floor)
        .max_by(|a, b| exploration_value(a).total_cmp(&exploration_value(b)));
    let chosen = trial.or(selected)?;
    assert!(chosen.route.passed <= chosen.route.runs);
    assert!(chosen.route.runs < 5 || wilson(chosen).1 >= floor || selected.is_some());
    Some((
        chosen,
        selected.is_none_or(|s| s.route.choice != chosen.route.choice),
    ))
}

fn exploration_value(e: &RouteEvidence) -> f64 {
    let (_, upper) = wilson(e);
    upper + 1.0 / f64::from(e.route.runs + 1) - 0.01 * e.median_cost_usd.ln_1p()
}

fn wilson(e: &RouteEvidence) -> (f64, f64) {
    e.quality_interval()
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
    pub(super) fn evidence(name: &str, cost: f64, runs: u32, passed: u32) -> RouteEvidence {
        RouteEvidence {
            route: LearnedRoute {
                repo: None,
                pipeline: "p".into(),
                stage: "s".into(),
                size: None,
                choice: ModelChoice {
                    model: format!("codex/{name}").parse().unwrap(),
                    effort: Effort::Low,
                    tier: None,
                },
                runs,
                passed,
            },
            median_cost_usd: cost,
        }
    }
    #[test]
    fn routing_uses_quality_bounds_and_explores_uncertain_candidates() {
        let options = vec![
            evidence("tiny", 0.01, 4, 4),
            evidence("cheaper", 0.1, 5, 3),
            evidence("balanced", 0.2, 20, 19),
            evidence("best", 1.0, 20, 20),
        ];
        let (normal, explored) = choose(&options, false).unwrap();
        assert_eq!(normal.route.choice.model.model, "best");
        assert!(!explored);
        let (trial, explored) = choose(&options, true).unwrap();
        assert_eq!(trial.route.choice.model.model, "tiny");
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

#[cfg(test)]
mod regression_tests {
    use super::*;
    #[test]
    fn an_observed_zero_for_hundred_is_never_explored_again() {
        let options = vec![
            super::tests::evidence("failed", 0.01, 100, 0),
            super::tests::evidence("reliable", 1.0, 100, 100),
        ];
        assert_eq!(
            choose(&options, true).unwrap().0.route.choice.model.model,
            "reliable"
        );
    }
}
