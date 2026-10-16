//! A running item's unstarted stages leave a provider whose quota ran out:
//! each tier-only choice still on it moves to the item's next provider that
//! meets the stage's floor, and the item records why.

use archon_core::error::ArchonError;
use archon_core::model::ModelChoice;
use archon_core::policy::ModelPolicy;
use archon_core::provider_route::ProviderWeights;
use archon_core::queue::QueueItem;
use archon_core::quota::{Headroom, QuotaView};
use archon_core::repo::Repo;
use archon_pipeline::{PipelineSpec, SpawnPlan};
use archon_queue::Store;

use crate::context::DaemonContext;
use crate::knobs::{self, ItemRoute, TierScope};
use crate::resolve::RepoEnv;

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
    let reset = spent.resets_at.map_or("names no reset".to_owned(), |at| {
        format!("resets {}", at.format("%Y-%m-%d %H:%M UTC"))
    });
    let reserve = match spent.reserve {
        0 => String::new(),
        percent => format!(" above its {percent}% reserve"),
    };
    let now = archon_core::now().format("%Y-%m-%d %H:%M UTC");
    let note = format!(
        "{now}: {scope} moved from {from} to {to}: {from} has no quota{reserve} (it {reset})"
    );
    debug_assert!(!note.contains('\n'), "one line");
    Some(note)
}

/// A stage moved off a provider out of quota, and the choice it moved to.
struct Moved {
    stage: String,
    fan: u32,
    from: String,
    choice: ModelChoice,
}

/// Move `plans` whose tier-only choice sits on a provider out of quota to
/// the item's next provider meeting each stage's floor in `spec`, and store
/// the item with its new provider, plan and switch. A kept agent resumes
/// where it is; a stage that finds no provider with quota starts where it was.
pub(crate) fn reroute_spawns(
    ctx: &DaemonContext,
    store: &mut Store,
    (item, spec): (&mut QueueItem, &PipelineSpec),
    plans: &mut [SpawnPlan],
    repo: Option<&Repo>,
) -> Result<(), ArchonError> {
    let Some(view) = super::view(ctx).filter(QuotaView::any_exhausted) else {
        return Ok(());
    };
    if !plans.iter().any(|plan| stranded(plan, &view)) {
        return Ok(());
    }
    let Some((weights, policy)) = routing(ctx, item, repo) else {
        return Ok(());
    };
    let (catalog, id) = (store.model_catalog(), item.id.clone());
    let quota = (Some(&weights), Some(&view));
    let held = knobs::item_provider(quota, &catalog, &policy, ItemRoute::new(item));
    let route = ItemRoute {
        id: &id,
        held: held.as_deref(),
    };
    let tiers = TierScope::new(&ctx.config, &policy, &catalog).routed(route.by(quota.0, quota.1));
    let mut moved = Vec::new();
    for plan in plans.iter_mut().filter(|plan| stranded(plan, &view)) {
        let from = plan.choice.model.provider.clone();
        if move_plan(plan, spec, tiers) {
            let (stage, fan) = (plan.stage_id.clone(), plan.fan_index);
            let choice = plan.choice.clone();
            moved.push(Moved {
                stage,
                fan,
                from,
                choice,
            });
        }
    }
    if moved.is_empty() {
        return Ok(());
    }
    item.routed_provider = held;
    record(store, item, &view, moved)
}

/// Whether `plan` starts a fresh agent by tier alone on a provider `view`
/// has out of quota: the only spawns that move.
fn stranded(plan: &SpawnPlan, view: &QuotaView) -> bool {
    let tier_only = plan
        .choice
        .tier
        .as_ref()
        .is_some_and(|t| t.provider.is_none());
    let spent = view
        .get(&plan.choice.model.provider)
        .is_some_and(Headroom::exhausted);
    plan.resume.is_none() && tier_only && spent
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

/// Store `item` with the choices `moved` took and why the first one moved.
fn record(
    store: &mut Store,
    item: &mut QueueItem,
    view: &QuotaView,
    moved: Vec<Moved>,
) -> Result<(), ArchonError> {
    let first = moved.first().expect("only a move is recorded");
    let to = Some(first.choice.model.provider.as_str());
    let scope = format!("stage {}", first.stage);
    item.provider_switch = switch_note((Some(&first.from), to), Some(view), &scope);
    debug_assert!(
        item.provider_switch.is_some(),
        "it moved off a spent provider"
    );
    for Moved {
        stage, fan, choice, ..
    } in moved
    {
        replace_choice(item, &stage, fan, choice);
    }
    item.updated_at = archon_core::now();
    store.update_item(item)
}

/// `plan`'s tier resolved again by `tiers` at its stage's floor; whether it
/// moved to another provider.
fn move_plan(plan: &mut SpawnPlan, spec: &PipelineSpec, tiers: TierScope<'_>) -> bool {
    let asked = plan
        .choice
        .tier
        .as_ref()
        .expect("only tier-only choices move")
        .requested;
    let floor = spec.tier_floor(&plan.stage_id, asked);
    match tiers.resolve((asked, floor), None) {
        Ok((model, tier)) if model.provider != plan.choice.model.provider => {
            tracing::info!(stage = %plan.stage_id, from = %plan.choice.model, to = %model, "moved an unstarted stage off a provider out of quota");
            plan.choice.model = model;
            plan.choice.tier = Some(tier);
            debug_assert!(
                plan.choice
                    .tier
                    .as_ref()
                    .is_some_and(|t| t.resolved >= floor)
            );
            true
        }
        // Every provider meeting the floor is out too: it starts where it was.
        Ok(_) => false,
        Err(error) => {
            tracing::warn!(stage = %plan.stage_id, %error, "no provider to move a stage to; it keeps its own");
            false
        }
    }
}

/// The item's stored plan with `stage`'s slot `fan` set to `choice`, so
/// `get_item` and a recovered run see the model it moved to.
fn replace_choice(item: &mut QueueItem, stage: &str, fan: u32, choice: ModelChoice) {
    assert!(choice.tier.is_some(), "only tier-only choices move");
    let slot = item
        .model_plan
        .0
        .get_mut(stage)
        .and_then(|choices| choices.get_mut(fan as usize))
        .filter(|held| held.tier.is_some());
    if let Some(held) = slot {
        *held = choice;
    }
}
