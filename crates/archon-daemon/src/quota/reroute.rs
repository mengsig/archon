//! A running item's stages, as they are reserved: they leave a provider out
//! of quota, follow the item's new provider, or wait while every one is out.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model_tier::ModelCatalog;
use archon_core::policy::ModelPolicy;
use archon_core::provider_route::{ProviderRoute, ProviderWeights, Routed};
use archon_core::queue::QueueItem;
use archon_core::quota::{Headroom, QuotaHold, QuotaView};
use archon_core::repo::Repo;
use archon_pipeline::{PipelineSpec, SpawnPlan};
use archon_queue::Store;
use archon_queue::dispatch::PendingSpawn;

use crate::context::DaemonContext;
use crate::knobs::{self, ItemRoute, TierScope};
use crate::resolve::RepoEnv;

/// Whether a run's pending spawns are reserved now or wait for quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reserve {
    Go,
    Wait,
}

/// One line for `get_item`: when `scope` moved from `from` to `to` and why.
/// `None` unless it moved off a provider `quota` has out of quota.
pub(crate) fn switch_note(
    (from, to): (Option<&str>, Option<&str>),
    quota: Option<&QuotaView>,
    scope: &str,
) -> Option<String> {
    let (from, to) = (from?, to?);
    let spent = quota?.get(from).filter(|h| h.exhausted())?;
    if from == to {
        return None;
    }
    let reserve = match spent.reserve {
        0 => String::new(),
        percent => format!(" above its {percent}% reserve"),
    };
    let now = minute(archon_core::now());
    let note = format!(
        "{now}: {scope} moved from {from} to {to}: {from} has no quota{reserve} ({})",
        reset_text(spent.resets_at)
    );
    debug_assert!(!note.contains('\n'), "one line");
    Some(note)
}

fn minute(at: archon_core::Timestamp) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn reset_text(at: Option<archon_core::Timestamp>) -> String {
    at.map_or("no reset reported".to_owned(), |at| {
        format!("resets {}", minute(at))
    })
}

/// Route `plans` of `item`'s run in `spec` by quota before they are reserved,
/// storing the item when its provider, plan or note changed.
pub(crate) fn reroute_spawns(
    ctx: &DaemonContext,
    store: &mut Store,
    (item, spec): (&mut QueueItem, &PipelineSpec),
    plans: &mut [SpawnPlan],
    repo: Option<&Repo>,
) -> Result<Reserve, ArchonError> {
    assert!(!plans.is_empty(), "only pending spawns are routed");
    assert!(
        (plans.iter()).all(|p| spec.file.stages.iter().any(|s| s.id == p.stage_id)),
        "the plans are stages of the run's pipeline"
    );
    let view = super::view(ctx);
    let spent = view.as_ref().is_some_and(QuotaView::any_exhausted);
    if !spent && item.provider_switch.is_none() {
        return Ok(Reserve::Go);
    }
    let Some((weights, policy)) = routing(ctx, item, repo) else {
        return Ok(Reserve::Go);
    };
    let catalog = store.model_catalog();
    if catalog.is_empty() {
        return Ok(Reserve::Go);
    }
    if let Some(spent) = view.as_ref().filter(|v| v.any_exhausted())
        && let Some(hold) = first_hold(item, (spec, plans), (&weights, &policy), (&catalog, spent))
    {
        record_wait(store, item, &hold)?;
        return Ok(Reserve::Wait);
    }
    let quota = (Some(&weights), view.as_ref());
    let held = knobs::item_provider(quota, &catalog, &policy, ItemRoute::new(item));
    let id = item.id.clone();
    let route = ItemRoute {
        id: &id,
        held: held.as_deref(),
    }
    .by(quota.0, quota.1);
    let route = route.expect("weights route the item");
    let tiers = TierScope::new(&ctx.config, &policy, &catalog).routed(Some(route));
    let first = move_plans(plans, (spec, tiers), (held.as_deref(), view.as_ref()));
    if first.is_none() && item.routed_provider == held {
        return Ok(Reserve::Go);
    }
    item.routed_provider = held.clone();
    if let Some((stage, from, to)) = first {
        let scope = format!("stage {stage}");
        let note = switch_note((Some(&from), Some(&to)), view.as_ref(), &scope);
        item.provider_switch = note.or(item.provider_switch.take());
    }
    follow_plan(item, spec, tiers);
    item.updated_at = archon_core::now();
    store.update_item(item)?;
    Ok(Reserve::Go)
}

/// The weights and policy `item` routes by in `repo`; `None` when weights
/// do not route it, or its config does not read (logged: nothing moves).
fn routing(
    ctx: &DaemonContext,
    item: &QueueItem,
    repo: Option<&Repo>,
) -> Option<(ProviderWeights, ModelPolicy)> {
    assert_eq!(
        item.repo.as_ref(),
        repo.map(|r| &r.id),
        "the item's own repository"
    );
    let env = match RepoEnv::new(repo.cloned()) {
        Ok(env) => env,
        Err(error) => {
            tracing::warn!(item = %item.id, %error, "repository config unreadable; stages keep their provider");
            return None;
        }
    };
    let weights = env.provider_weights(&ctx.config)?;
    Some((weights, env.policy(&ctx.config)))
}

/// The first of `plans` of `item` in `spec` that waits for quota, routed as
/// the item is; the planner gives such a run no slot.
pub(super) fn first_hold(
    item: &QueueItem,
    (spec, plans): (&PipelineSpec, &[SpawnPlan]),
    (weights, policy): (&ProviderWeights, &ModelPolicy),
    (catalog, view): (&ModelCatalog, &QuotaView),
) -> Option<QuotaHold> {
    assert!(!plans.is_empty(), "a run with pending spawns");
    assert!(!catalog.is_empty(), "tiers resolve against a catalog");
    let quota = (Some(weights), Some(view));
    let held = knobs::item_provider(quota, catalog, policy, ItemRoute::new(item));
    let route = ItemRoute {
        id: &item.id,
        held: held.as_deref(),
    }
    .by(quota.0, quota.1)?;
    plans
        .iter()
        .find_map(|p| plan_hold(p, spec, (catalog, policy), route))
}

/// Note on each item the planner held a pending spawn of that it waits.
pub(crate) async fn note_waits(
    ctx: &DaemonContext,
    spawns: &[PendingSpawn],
) -> Result<(), ArchonError> {
    let held: Vec<(&QueueItemId, &QuotaHold)> = (spawns.iter())
        .filter_map(|s| Some((&s.item, s.quota_hold.as_ref()?)))
        .collect();
    if held.is_empty() {
        return Ok(());
    }
    let mut store = ctx.store.lock().await;
    for (id, hold) in held {
        let mut item = store.item(id)?;
        assert_eq!(&item.id, id, "the store returns the item asked for");
        record_wait(&mut store, &mut item, hold)?;
    }
    Ok(())
}

/// `plan`'s hold when it starts by tier alone and every provider meeting its
/// floor is out of quota; a kept agent's resume waits likewise.
fn plan_hold(
    plan: &SpawnPlan,
    spec: &PipelineSpec,
    (catalog, policy): (&ModelCatalog, &ModelPolicy),
    route: ProviderRoute<'_>,
) -> Option<QuotaHold> {
    let tier = plan.choice.tier.as_ref().filter(|t| t.provider.is_none())?;
    route.quota?;
    let floor = spec.tier_floor(&plan.stage_id, tier.requested);
    match catalog.route((tier.requested, floor), route, policy) {
        Routed::Exhausted { until } => Some(QuotaHold {
            stage: plan.stage_id.clone(),
            until,
        }),
        Routed::To(_) | Routed::Nowhere => None,
    }
}

/// Note on `item` that `hold`'s stage waits for quota, once per stage.
fn record_wait(
    store: &mut Store,
    item: &mut QueueItem,
    hold: &QuotaHold,
) -> Result<(), ArchonError> {
    let waits = format!("stage {} waits for provider quota", hold.stage);
    if item
        .provider_switch
        .as_deref()
        .is_some_and(|n| n.contains(&waits))
    {
        return Ok(());
    }
    let now = minute(archon_core::now());
    let reset = reset_text(hold.until);
    item.provider_switch = Some(format!(
        "{now}: {waits}: every provider meeting its floor is out ({reset})"
    ));
    tracing::info!(item = %item.id, stage = %hold.stage, until = ?hold.until, "a stage waits for provider quota");
    item.updated_at = archon_core::now();
    store.update_item(item)
}

/// Move each fresh tier-only plan that sits on a provider out of quota, or
/// off the item's `held` one, as `tiers` resolve it; the first move made.
fn move_plans(
    plans: &mut [SpawnPlan],
    (spec, tiers): (&PipelineSpec, TierScope<'_>),
    (held, view): (Option<&str>, Option<&QuotaView>),
) -> Option<(String, String, String)> {
    let mut first = None;
    for plan in plans.iter_mut() {
        let Some(tier) = plan.choice.tier.as_ref().filter(|t| t.provider.is_none()) else {
            continue;
        };
        let on = plan.choice.model.provider.as_str();
        let spent = view
            .and_then(|v| v.get(on))
            .is_some_and(Headroom::exhausted);
        if plan.resume.is_some() || (!spent && held.is_none_or(|h| h == on)) {
            continue;
        }
        let floor = spec.tier_floor(&plan.stage_id, tier.requested);
        let (model, tier) = match tiers.resolve((tier.requested, floor), None) {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(stage = %plan.stage_id, %error, "no provider to move a stage to; it keeps its own");
                continue;
            }
        };
        if model.provider == on {
            continue;
        }
        tracing::info!(stage = %plan.stage_id, from = %plan.choice.model, to = %model, "moved an unstarted stage to the item's provider");
        let from = std::mem::replace(&mut plan.choice.model, model);
        debug_assert!(tier.resolved >= floor, "never below the floor");
        plan.choice.tier = Some(tier);
        let to = plan.choice.model.provider.clone();
        first.get_or_insert((plan.stage_id.clone(), from.provider, to));
    }
    first
}

/// Resolve `item`'s stored tier-only choices again, so its later stages and
/// a recovered run follow the provider it moved to.
fn follow_plan(item: &mut QueueItem, spec: &PipelineSpec, tiers: TierScope<'_>) {
    let before = item.model_plan.clone();
    let resolved = item.model_plan.resolve_tiers(|key, tier, provider| {
        tiers.resolve((tier, spec.tier_floor(key, tier)), provider)
    });
    if let Err(error) = resolved {
        tracing::warn!(item = %item.id, %error, "the stored plan keeps its models");
        item.model_plan = before;
        return;
    }
    debug_assert_eq!(
        item.model_plan.0.keys().collect::<Vec<_>>(),
        before.0.keys().collect::<Vec<_>>(),
        "only models change"
    );
}
