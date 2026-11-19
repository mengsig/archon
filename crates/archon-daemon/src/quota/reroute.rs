//! A running item's stages, as they are reserved: they leave a provider out
//! of quota, follow the item's new provider, or wait while every one is out.

use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QueueItemId};
use archon_core::model::{ModelPlan, ModelRef};
use archon_core::model_tier::ModelCatalog;
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::plan_origin::TierMove;
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
/// storing the item when its provider, plan or note changed. A catalog repair
/// that `replaced` the stored plan just before is noted with these moves.
pub(crate) fn reroute_spawns(
    ctx: &DaemonContext,
    store: &mut Store,
    (item, spec): (&mut QueueItem, &PipelineSpec),
    plans: &mut [SpawnPlan],
    (repo, replaced): (Option<&Repo>, Option<&ModelPlan>),
) -> Result<Reserve, ArchonError> {
    assert!(!plans.is_empty(), "only pending spawns are routed");
    assert!(
        (plans.iter()).all(|p| spec.file.stages.iter().any(|s| s.id == p.stage_id)),
        "the plans are stages of the run's pipeline"
    );
    let view = super::view(ctx);
    let Some(routing) = quota_routing(ctx, store, item, repo, view.as_ref()) else {
        return note_repair(ctx, store, item, replaced, view.as_ref()).map(|()| Reserve::Go);
    };
    let (weights, policy, catalog) = &routing;
    if let Some(spent) = view.as_ref().filter(|v| v.any_exhausted())
        && let Some(hold) = first_hold(item, (spec, plans), (weights, policy), (catalog, spent))
    {
        note_repair(ctx, store, item, replaced, view.as_ref())?;
        record_wait(store, item, &hold)?;
        return Ok(Reserve::Wait);
    }
    let read = (&routing, replaced, view.as_ref());
    let unmoved = route_plans(ctx, store, (item, spec), plans, read)?;
    // These plans passed the quota hold above; a replacement skips it, so never ends a wait.
    if unmoved {
        note_started(store, item, plans)?;
    }
    Ok(Reserve::Go)
}

/// Route a cold replacement `plan` of `item`'s run by quota as a reserved
/// stage is routed, storing the item when it moved. It never waits for
/// quota: a replacement is reserved at once, as in 1.6.3. Only a wake keeps
/// the slot's agent (`takeover` replaces a lost one), so findings the plan
/// carries for that agent go to a fresh one, routed as any fresh stage is.
pub(crate) fn route_replacement(
    ctx: &DaemonContext,
    store: &mut Store,
    (item, spec): (&mut QueueItem, &PipelineSpec),
    plan: &mut SpawnPlan,
    repo: Option<&Repo>,
) -> Result<(), ArchonError> {
    assert!(
        spec.file.stages.iter().any(|s| s.id == plan.stage_id),
        "the plan is a stage of the run's pipeline"
    );
    let view = super::view(ctx);
    let Some(routing) = quota_routing(ctx, store, item, repo, view.as_ref()) else {
        return Ok(());
    };
    let mut fresh = SpawnPlan {
        resume: None,
        ..plan.clone()
    };
    // A pending attempt still waits behind this note: the replacement skips that hold.
    let waiting = (item.provider_switch.clone()).filter(|note| note.contains(WAITS_NOTE));
    let plans = std::slice::from_mut(&mut fresh);
    route_plans(
        ctx,
        store,
        (item, spec),
        plans,
        (&routing, None, view.as_ref()),
    )?;
    assert_eq!(
        fresh.choice.effort, plan.choice.effort,
        "routing moves no effort"
    );
    plan.choice = fresh.choice;
    let Some(waiting) = waiting.filter(|note| item.provider_switch.as_ref() != Some(note)) else {
        return Ok(());
    };
    item.provider_switch = Some(waiting);
    store.update_item(item)
}

/// Move `plans` off spent providers or onto the item's held one, follow the
/// stored plan with them, and note every move with any catalog repair that
/// `replaced` the stored plan; with nothing moved, only that repair is noted
/// and `true` is returned.
fn route_plans(
    ctx: &DaemonContext,
    store: &mut Store,
    (item, spec): (&mut QueueItem, &PipelineSpec),
    plans: &mut [SpawnPlan],
    ((weights, policy, catalog), replaced, view): (
        &(ProviderWeights, ModelPolicy, ModelCatalog),
        Option<&ModelPlan>,
        Option<&QuotaView>,
    ),
) -> Result<bool, ArchonError> {
    let quota = (Some(weights), view);
    let held = knobs::item_provider(quota, catalog, policy, ItemRoute::new(item));
    let id = item.id.clone();
    let route = ItemRoute {
        id: &id,
        held: held.as_deref(),
    }
    .by(quota.0, quota.1);
    let route = route.expect("weights route the item");
    let tiers = TierScope::new(&ctx.config, policy, catalog).routed(Some(route));
    let moved = move_plans(plans, (spec, tiers), (held.as_deref(), view));
    if moved.is_empty() && item.routed_provider == held {
        note_repair(ctx, store, item, replaced, view)?;
        return Ok(true);
    }
    item.routed_provider = held.clone();
    let stored = item.model_plan.plan().clone();
    follow_plan(item, spec, tiers);
    let before = notice_baseline(replaced, &stored);
    let mut moves = repair_moves((before, &stored), item.model_plan.plan(), catalog);
    moves.extend(moved);
    record_moves(ctx, store, item, (&moves, before), view)?;
    item.updated_at = archon_core::now();
    store.update_item(item)?;
    Ok(false)
}

/// Replace `item`'s note that one of `plans`' stages waits for quota, now
/// that it goes on unmoved: a model an earlier move gave it has quota again.
fn note_started(
    store: &mut Store,
    item: &mut QueueItem,
    plans: &[SpawnPlan],
) -> Result<(), ArchonError> {
    let Some(note) = item.provider_switch.as_deref() else {
        return Ok(());
    };
    let waits = |plan: &&SpawnPlan| {
        note.contains(&format!("stage {} waits for provider quota", plan.stage_id))
    };
    let Some(plan) = plans.iter().find(waits) else {
        return Ok(());
    };
    let now = minute(archon_core::now());
    let started = format!(
        "{now}: stage {} no longer waits for provider quota; it runs on {}",
        plan.stage_id, plan.choice.model
    );
    assert!(!started.contains('\n'), "one line");
    item.provider_switch = Some(started);
    item.updated_at = archon_core::now();
    store.update_item(item)
}

/// The weights, policy and catalog `item`'s stages are routed by quota with;
/// `None` while nothing is spent and nothing moved, or nothing routes them.
fn quota_routing(
    ctx: &DaemonContext,
    store: &Store,
    item: &QueueItem,
    repo: Option<&Repo>,
    view: Option<&QuotaView>,
) -> Option<(ProviderWeights, ModelPolicy, ModelCatalog)> {
    let spent = view.is_some_and(QuotaView::any_exhausted);
    if !spent && item.provider_switch.is_none() {
        return None;
    }
    let (weights, policy) = routing(ctx, item, repo)?;
    let catalog = store.model_catalog();
    if catalog.is_empty() {
        return None;
    }
    Some((weights, policy, catalog))
}

/// Note on `item`, and in its feed, the moves a catalog repair made from the
/// plan it `replaced`, storing the item; nothing without such a repair.
fn note_repair(
    ctx: &DaemonContext,
    store: &mut Store,
    item: &mut QueueItem,
    replaced: Option<&ModelPlan>,
    view: Option<&QuotaView>,
) -> Result<(), ArchonError> {
    let Some(replaced) = replaced else {
        return Ok(());
    };
    let now = item.model_plan.plan().clone();
    let before = notice_baseline(Some(replaced), &now);
    let dropped = repair_moves((before, &now), &now, &store.model_catalog());
    if dropped.is_empty() {
        return Ok(());
    }
    record_moves(ctx, store, item, (&dropped, before), view)?;
    assert!(item.provider_switch.is_some(), "a move is noted");
    item.updated_at = archon_core::now();
    store.update_item(item)
}

/// [`catalog_moves`] of a repair that may not have run: none when `replaced`
/// is the plan it would have repaired.
fn repair_moves(
    (replaced, repaired): (&ModelPlan, &ModelPlan),
    now: &ModelPlan,
    catalog: &ModelCatalog,
) -> Vec<Moved> {
    if replaced == repaired {
        return Vec::new();
    }
    catalog_moves((replaced, repaired), now, catalog)
}

/// The plan a model notice compares with: the one a catalog repair just
/// `replaced`, while it still has the `stored` plan's shape.
pub(crate) fn notice_baseline<'a>(
    replaced: Option<&'a ModelPlan>,
    stored: &'a ModelPlan,
) -> &'a ModelPlan {
    let same_shape = |plan: &&ModelPlan| {
        let entries = plan.0.iter().zip(&stored.0);
        plan.0.len() == stored.0.len()
            && entries
                .into_iter()
                .all(|(was, now)| was.0 == now.0 && was.1.len() == now.1.len())
    };
    replaced.filter(same_shape).unwrap_or(stored)
}

/// One stage slot a reroute moved from model `from` to model `to`; `dropped`
/// says why when the model catalog no longer offers `from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Moved {
    stage: String,
    from: ModelRef,
    to: ModelRef,
    dropped: Option<String>,
}

impl Moved {
    fn same_swap(&self, other: &Moved) -> bool {
        self.stage == other.stage && self.from == other.from && self.to == other.to
    }
}

/// The moves from the plan a catalog repair `replaced` to the plan as it is
/// `now`, in the slots that repair changed (to `repaired`): the catalog
/// dropped their model, whatever tier or quota moved them further.
pub(crate) fn catalog_moves(
    (replaced, repaired): (&ModelPlan, &ModelPlan),
    now: &ModelPlan,
    catalog: &ModelCatalog,
) -> Vec<Moved> {
    assert!(
        catalog.not_ready_reason().is_none(),
        "only a ready catalog repairs a plan"
    );
    let by_repair = plan_moves(replaced, repaired);
    let mut moves = plan_moves(replaced, now);
    moves.retain(|m| (by_repair.iter()).any(|r| r.stage == m.stage && r.from == m.from));
    for moved in &mut moves {
        moved.dropped = Some(dropped_reason(catalog, &moved.from));
    }
    assert!(
        moves.iter().all(|m| m.dropped.is_some() && m.from != m.to),
        "each says why its model was dropped"
    );
    moves
}

/// Why `catalog` no longer offers `model` to a stage the daemon chose it for.
fn dropped_reason(catalog: &ModelCatalog, model: &ModelRef) -> String {
    if catalog.is_retired(model) {
        return format!("{model} is retired");
    }
    if let Some(successor) = catalog.superseded_by(model) {
        return format!("{model} is superseded by {successor}");
    }
    format!("the model catalog no longer lists {model}")
}

/// Note on `item`, and post to its feed, every model its `pending` spawns
/// moved to and every choice of its stored plan that differs from `before`;
/// nothing when no model moved. A move among `pending` keeps its own reason
/// over the same swap read from the plan. The caller stores `item`.
pub(crate) fn record_moves(
    ctx: &DaemonContext,
    store: &mut Store,
    item: &mut QueueItem,
    (pending, before): (&[Moved], &ModelPlan),
    view: Option<&QuotaView>,
) -> Result<(), ArchonError> {
    let moves = distinct_moves(pending, &plan_moves(before, &item.model_plan));
    let Some(first) = moves.first() else {
        return Ok(());
    };
    let reason = move_reason(first, item.routed_provider.as_deref(), view);
    let swaps = swap_list(&moves);
    announce_move(ctx, store, item, &reason, &swaps)?;
    let note = format!("{}: {reason}; {swaps}", minute(archon_core::now()));
    debug_assert!(!note.contains('\n'), "one line");
    assert!(
        moves.iter().all(|m| note.contains(&m.stage)),
        "the note names every stage that moved"
    );
    item.provider_switch = Some(note);
    Ok(())
}

/// `pending` then each of `followed` not among them: one entry per distinct
/// swap, so a fan-out slot that moved differently keeps its own.
fn distinct_moves(pending: &[Moved], followed: &[Moved]) -> Vec<Moved> {
    let mut moves = pending.to_vec();
    for moved in followed {
        if !moves.iter().any(|m| m.same_swap(moved)) {
            moves.push(moved.clone());
        }
    }
    debug_assert!(
        (followed.iter()).all(|f| moves.iter().any(|m| m.same_swap(f))),
        "none dropped"
    );
    assert!(
        moves.iter().all(|m| m.from != m.to),
        "a move changes the model"
    );
    moves
}

/// Why `first` of a reroute's moves moved: the catalog dropped its model, it
/// left a provider out of quota, moved onto the item's `held` provider, or
/// its tier resolves elsewhere now.
fn move_reason(first: &Moved, held: Option<&str>, view: Option<&QuotaView>) -> String {
    let (from, to) = (first.from.provider.as_str(), first.to.provider.as_str());
    let scope = format!("stage {}", first.stage);
    if let Some(dropped) = &first.dropped {
        let reason = format!(
            "{scope} moved from {} to {}: {dropped}",
            first.from, first.to
        );
        debug_assert!(!reason.contains('\n'), "one line");
        return reason;
    }
    if let Some(note) = switch_note((Some(from), Some(to)), view, &scope) {
        let (_, reason) = note
            .split_once(": ")
            .expect("a note starts with its minute");
        return reason.to_owned();
    }
    let why = match held == Some(to) && from != to {
        true => format!("it follows the item's provider {to}"),
        false => format!("its tier resolves to {} now", first.to),
    };
    let reason = match from == to {
        true => format!("{scope} moved from {} to {}: {why}", first.from, first.to),
        false => format!("{scope} moved from {from} to {to}: {why}"),
    };
    debug_assert!(!reason.contains('\n'), "one line");
    reason
}

/// Every model `moves` swapped, by stage, old and new: one clause each. A
/// later move the catalog made says so, as the reason before the list tells
/// of the first move alone.
fn swap_list(moves: &[Moved]) -> String {
    assert!(!moves.is_empty(), "a list is made only for a move");
    let swaps: Vec<String> = moves
        .iter()
        .enumerate()
        .map(|(index, m)| {
            let swap = format!(
                "stage {} now runs on {} instead of {}",
                m.stage, m.to, m.from
            );
            match m.dropped.as_ref().filter(|_| index > 0) {
                Some(dropped) => format!("{swap} ({dropped})"),
                None => swap,
            }
        })
        .collect();
    assert_eq!(swaps.len(), moves.len(), "one clause a move");
    swaps.join("; ")
}

/// Post the reroute's `reason` and every model it swapped to `item`'s feed,
/// so no stage runs on another model unannounced.
fn announce_move(
    ctx: &DaemonContext,
    store: &mut Store,
    item: &QueueItem,
    reason: &str,
    swaps: &str,
) -> Result<(), ArchonError> {
    assert!(!reason.is_empty(), "the notice says why");
    assert!(!swaps.is_empty(), "the notice says what moved");
    let entry = FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind: NotificationKind::Progress,
        text: format!("Item {} changed models: {reason}; {swaps}", item.id),
        item: Some(item.id.clone()),
        run: None,
        at: archon_core::now(),
    });
    let posted = store.append_feed(entry, archon_core::now())?;
    crate::feed::publish(ctx, vec![posted]);
    Ok(())
}

/// Every choice whose model differs from `before` to `after`, keyed alike.
fn plan_moves(before: &ModelPlan, after: &ModelPlan) -> Vec<Moved> {
    assert!(
        before.0.keys().eq(after.0.keys()),
        "a reroute changes models, not stages"
    );
    let pairs = before.0.iter().zip(after.0.values());
    let mut moved = Vec::new();
    for ((stage, was), now) in pairs {
        assert_eq!(was.len(), now.len(), "a reroute keeps each stage's choices");
        let changed = was
            .iter()
            .zip(now)
            .filter(|(was, now)| was.model != now.model);
        moved.extend(changed.map(|(was, now)| Moved {
            stage: stage.clone(),
            from: was.model.clone(),
            to: now.model.clone(),
            dropped: None,
        }));
    }
    moved
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
    Some((weights, env.resolved_policy(ctx)))
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
/// What follows a stage in the note [`record_wait`] writes, and only there.
const WAITS_NOTE: &str = "waits for provider quota: ";

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
    let note = format!("{now}: {waits}: every provider meeting its floor is out ({reset})");
    assert!(note.contains(WAITS_NOTE), "a wait note reads as one");
    item.provider_switch = Some(note);
    tracing::info!(item = %item.id, stage = %hold.stage, until = ?hold.until, "a stage waits for provider quota");
    item.updated_at = archon_core::now();
    store.update_item(item)
}

/// Move each fresh tier-only plan that sits on a provider out of quota, or
/// off the item's `held` one, as `tiers` resolve it; every move made. An
/// exact pin is never moved.
fn move_plans(
    plans: &mut [SpawnPlan],
    (spec, tiers): (&PipelineSpec, TierScope<'_>),
    (held, view): (Option<&str>, Option<&QuotaView>),
) -> Vec<Moved> {
    let mut moved = Vec::new();
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
        moved.push(Moved {
            stage: plan.stage_id.clone(),
            from,
            to: plan.choice.model.clone(),
            dropped: None,
        });
    }
    moved
}

/// Resolve `item`'s stored tiered choices again, so its later stages and a
/// recovered run follow the provider it moved to. A choice that no longer
/// resolves keeps its model alone; the rest still follow.
fn follow_plan(item: &mut QueueItem, spec: &PipelineSpec, tiers: TierScope<'_>) {
    let before = plan_shape(&item.model_plan);
    let id = item.id.clone();
    let follow = |key: &str, asked: &archon_core::model_tier::TierChoice| {
        let floor = spec.tier_floor(key, asked.requested);
        match tiers.resolve((asked.requested, floor), asked.provider.as_deref()) {
            Ok(resolved) => Ok(Some(resolved)),
            Err(error) => {
                tracing::warn!(item = %id, stage = %key, %error, "a stored choice keeps its model");
                Ok(None)
            }
        }
    };
    let note = format!(
        "moved to follow provider {}",
        item.routed_provider.as_deref().unwrap_or("of the item")
    );
    let result = item.model_plan.retier(
        &id,
        (archon_core::now(), Some(&note), TierMove::InRun),
        follow,
    );
    result.expect("following a provider refuses nothing");
    assert_eq!(plan_shape(&item.model_plan), before, "only models change");
}

/// Each plan key with its number of choices.
fn plan_shape(plan: &ModelPlan) -> Vec<(String, usize)> {
    (plan.0.iter())
        .map(|(key, c)| (key.clone(), c.len()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(stage: &str, from: &str, to: &str) -> Moved {
        Moved {
            stage: stage.into(),
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            dropped: None,
        }
    }

    #[test]
    fn a_fan_out_slot_that_moved_differently_keeps_its_own_entry() {
        let pending = [moved("review", "a/x", "b/y")];
        let followed = [
            moved("review", "a/x", "b/y"),
            moved("review", "a/z", "b/w"),
            moved("ship", "a/x", "b/y"),
        ];
        let moves = distinct_moves(&pending, &followed);
        assert_eq!(
            moves.len(),
            3,
            "the same slot once, each other kept: {moves:?}"
        );
        assert_eq!(moves[1], moved("review", "a/z", "b/w"));
        assert_eq!(distinct_moves(&[], &followed[2..]), followed[2..]);
    }

    fn plan(choices: &[(&str, &[&str])]) -> ModelPlan {
        let choice = |model: &&str| {
            let effort = archon_core::model::Effort::Medium;
            archon_core::model::ModelChoice::pinned(model.parse().unwrap(), effort)
        };
        let entries =
            (choices.iter()).map(|(k, m)| ((*k).to_owned(), m.iter().map(choice).collect()));
        ModelPlan(entries.collect())
    }

    /// Only the slots the repair changed move for the catalog's reason, from
    /// the model it replaced to the one that runs, whatever came between.
    #[test]
    fn a_catalog_move_is_a_slot_the_repair_changed_with_why_its_model_was_dropped() {
        let listed = archon_core::model_tier::CatalogModel {
            model: "a/new".parse().unwrap(),
            tier: None,
            is_default: false,
        };
        let mut catalog = ModelCatalog::new(vec![listed]);
        catalog.add_retired("a/old".parse().unwrap(), None);
        catalog.add_superseded("a/aging".parse().unwrap(), "a/new".parse().unwrap());
        let stages: [&str; 4] = ["build", "check", "review", "ship"];
        let of = |models: [&[&str]; 4]| plan(&[0, 1, 2, 3].map(|i| (stages[i], models[i])));
        let replaced = of([&["a/old"], &["a/gone"], &["a/aging", "b/kept"], &["b/tier"]]);
        let repaired = of([&["a/mid"], &["a/next"], &["a/new", "b/kept"], &["b/tier"]]);
        let now = of([&["a/last"], &["a/next"], &["a/new", "b/kept"], &["b/moved"]]);

        let moves = catalog_moves((&replaced, &repaired), &now, &catalog);
        let told: Vec<_> = (moves.iter())
            .map(|m| {
                (
                    m.stage.as_str(),
                    m.to.to_string(),
                    m.dropped.clone().unwrap(),
                )
            })
            .collect();
        let expected = [
            ("build", "a/last", "a/old is retired"),
            (
                "check",
                "a/next",
                "the model catalog no longer lists a/gone",
            ),
            ("review", "a/new", "a/aging is superseded by a/new"),
        ];
        assert_eq!(
            told,
            expected.map(|(s, to, why)| (s, to.to_owned(), why.to_owned()))
        );
        assert!(catalog_moves((&repaired, &repaired), &now, &catalog).is_empty());

        let all = distinct_moves(&moves, &plan_moves(&replaced, &now));
        assert_eq!(all.len(), 4, "each swap once: {all:?}");
        assert_eq!((all[3].stage.as_str(), &all[3].dropped), ("ship", &None));
        let reason = move_reason(&all[0], Some("a"), None);
        assert_eq!(
            reason,
            "stage build moved from a/old to a/last: a/old is retired"
        );
        let swaps = swap_list(&all);
        assert!(swaps.starts_with("stage build now runs on a/last instead of a/old; "));
        assert!(swaps.contains("instead of a/gone (the model catalog no longer lists a/gone)"));
        assert!(swaps.ends_with("stage ship now runs on b/moved instead of b/tier"));
    }

    #[test]
    fn a_note_claims_the_item_s_provider_only_when_the_move_went_onto_it() {
        let ship = moved("ship", "a/x", "b/y");
        let followed = move_reason(&ship, Some("b"), None);
        assert!(
            followed.contains("stage ship moved from a to b"),
            "{followed}"
        );
        assert!(
            followed.contains("follows the item's provider b"),
            "{followed}"
        );
        for held in [None, Some("c")] {
            let note = move_reason(&ship, held, None);
            assert!(!note.contains("item's provider"), "{held:?}: {note}");
            assert!(note.contains("its tier resolves to b/y now"), "{note}");
        }
        let same = move_reason(&moved("ship", "a/x", "a/y"), Some("a"), None);
        assert!(same.contains("its tier resolves to a/y now"), "{same}");
        assert!(
            same.contains("stage ship moved from a/x to a/y"),
            "a move within a provider names both models: {same}"
        );
    }
}
