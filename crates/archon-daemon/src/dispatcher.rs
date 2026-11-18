//! The dispatch loop (ARCHITECTURE §7.3). Wakes on
//! `ctx.dispatch` or every `dispatcher.tick_secs`, builds a snapshot, calls
//! `archon_queue::plan_dispatch`, and executes the plan. No dispatch while
//! Aether is disconnected or start-up recovery runs. Each pass first checks
//! free disk space, and while that holds dispatch, collects what finished
//! runs left first; then probes each engine held for a failed login.

mod disk_hold;
mod engine_hold;
mod snapshot;

pub(crate) use disk_hold::{check as check_disk, read as read_disk};
pub(crate) use snapshot::invalid_pin;

use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::queue::{Caps, QueueState};
use archon_queue::DispatchPlan;
use archon_queue::dispatch::{GangWaiter, Hold, ModelHoldKind, NeverFits};

use crate::context::DaemonContext;
use crate::executor::{self, FollowUp};
use crate::readers::ReadOn;
use crate::resolve;
use crate::triggers::{self, TriggerEvent};

/// The longest a read waits for the run table, which an admission pass holds
/// across its writes; past it the read goes on without pending spawns.
const READ_TABLE_WAIT: Duration = Duration::from_millis(100);

/// The read-only view uses the dispatcher's complete snapshot and lock order,
/// on a reader connection. It does not check disk, collect worktrees, reserve
/// slots, or dispatch work.
pub(crate) async fn read_snapshot(
    ctx: &DaemonContext,
) -> Result<archon_queue::DispatchSnapshot, ArchonError> {
    let caps = resolve::caps(ctx).await;
    let catalog = catalog_for_view(ctx).await?;
    let envs = snapshot::Envs::prefetch(ctx, ReadOn::Reader).await?;
    // Recovery holds the run table while it rebuilds the drivers; no spawn is
    // pending before they are, so an empty table reads the same without waiting.
    let recovered = ctx.recovery.is_recovered();
    let table = if recovered {
        tokio::time::timeout(READ_TABLE_WAIT, ctx.runs.lock())
            .await
            .ok()
    } else {
        None
    };
    let Some(table) = table else {
        if recovered {
            tracing::debug!("the run table is busy; the view leaves out pending spawns");
        }
        let unseen = executor::RunTable::default();
        assert!(unseen.pending.is_empty(), "an empty table owes no spawn");
        return snapshot::build(ctx, ReadOn::Reader, &unseen, caps, envs, &catalog).await;
    };
    snapshot::build(ctx, ReadOn::Reader, &table, caps, envs, &catalog).await
}

async fn catalog_for_view(
    ctx: &DaemonContext,
) -> Result<archon_core::model_tier::ModelCatalog, ArchonError> {
    use archon_core::model_tier::ModelCatalog;
    let store = ctx.read_store().await?;
    if let Some(catalog) = crate::learned::cached_catalog(ctx, &store)? {
        return Ok(catalog);
    }
    if !ctx.aether.is_connected() {
        return Ok(ModelCatalog::unavailable("Aether disconnected".to_owned()));
    }
    let catalog = store.model_catalog();
    if let Some(reason) = catalog.unavailable_reason() {
        return Ok(ModelCatalog::unavailable(reason.to_owned()));
    }
    if store.catalog_generation() == Some(ctx.aether.generation()?) && catalog.is_empty() {
        return Ok(catalog);
    }
    Ok(ModelCatalog::unavailable(
        "waiting for a fresh catalog read".to_owned(),
    ))
}

/// Remembers the budget hold last announced, so each is posted once.
const BUDGET_HOLD_KEY: &str = "dispatch_budget_hold";

/// Remembers the teams over their caps last announced, one per line.
const NEVER_FITS_KEY: &str = "dispatch_never_fits";

/// Admission passes per `dispatch_once`; each pass that starts items is
/// followed by another so their first stages are admitted promptly.
const MAX_PASSES: usize = 8;

pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let tick = Duration::from_secs(ctx.config.dispatcher.tick_secs);
    assert!(!tick.is_zero(), "config validation refuses tick_secs = 0");
    let mut catalog_outage = CatalogOutage::default();
    loop {
        let catalog = crate::learned::catalog(&ctx).await?;
        if catalog_outage.observe(Instant::now(), catalog.not_ready_reason().is_some()) {
            let reason = catalog
                .not_ready_reason()
                .expect("only an unavailable catalog is announced");
            let text = format!(
                "Model catalog has been unavailable for 30 minutes: {reason}. Work remains queued until it recovers."
            );
            executor::notify(&ctx, NotificationKind::Blocker, &text, None, None).await?;
        }
        if ctx.aether.is_connected() {
            let pass = crate::executor::coordination::guarded(
                "a dispatch pass",
                dispatch_with_catalog(&ctx, &catalog),
            );
            match pass.await {
                Ok(_) => {}
                Err(error) if error.is_store() => return Err(error),
                Err(error) => tracing::error!(%error, "dispatch pass failed"),
            }
        }
        tokio::select! {
            () = ctx.dispatch.notified() => {}
            () = tokio::time::sleep(tick) => {}
        }
    }
}

/// One notice per continuous catalog outage, after 30 minutes.
#[derive(Default)]
struct CatalogOutage {
    since: Option<Instant>,
    announced: bool,
}

impl CatalogOutage {
    fn observe(&mut self, now: Instant, unavailable: bool) -> bool {
        if !unavailable {
            self.since = None;
            self.announced = false;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if self.announced || now.saturating_duration_since(since) < Duration::from_secs(30 * 60) {
            return false;
        }
        self.announced = true;
        true
    }
}

/// One planning pass; returns the plan it executed (for tests and Status).
/// A failing stage or item never holds back the rest of the pass: items
/// admitted as `dispatching` are always started. Its first error is
/// returned at the end; a store error ends the pass at once. Nothing is
/// planned until start-up recovery is done.
pub async fn dispatch_once(ctx: &DaemonContext) -> Result<DispatchPlan, ArchonError> {
    if !ctx.recovery.is_recovered() {
        return Ok(DispatchPlan::default());
    }
    let catalog = crate::learned::catalog(ctx).await?;
    dispatch_with_catalog(ctx, &catalog).await
}

async fn dispatch_with_catalog(
    ctx: &DaemonContext,
    catalog: &archon_core::model_tier::ModelCatalog,
) -> Result<DispatchPlan, ArchonError> {
    if !ctx.recovery.is_recovered() {
        let phase = ctx.recovery.phase();
        tracing::debug!(?phase, "dispatch held until recovery is done");
        return Ok(DispatchPlan::default());
    }
    let mut first_error = None;
    keep_error(
        &mut first_error,
        crate::clarifications::expire(ctx, archon_core::now()).await,
    )?;
    disk_hold::check(ctx).await?;
    if crate::gc::before_dispatch(ctx).await? {
        disk_hold::check(ctx).await?;
    }
    engine_hold::check(ctx).await?;
    keep_error(&mut first_error, executor::sweep_live_stages(ctx).await)?;
    let mut merged = DispatchPlan::default();
    let mut last = None;
    for _ in 0..MAX_PASSES {
        let admission = admit(ctx, catalog).await?;
        let started = admission.plan.start.clone();
        merged.spawn.extend(admission.plan.spawn.iter().cloned());
        merged.start.extend(started.iter().cloned());
        merged.block.extend(admission.plan.block.iter().cloned());
        merged.held_by = admission.plan.held_by.clone();
        merged.never_fits = admission.plan.never_fits.clone();
        let spawned = executor::run_follow_up(ctx, admission.follow).await;
        keep_error(&mut first_error, spawned)?;
        keep_error(&mut first_error, executor::start_items(ctx, &started).await)?;
        last = Some(admission.budget);
        if started.is_empty() {
            break;
        }
    }
    let budget = last.expect("MAX_PASSES > 0");
    announce_budget_hold(ctx, merged.held_by.as_ref(), &budget).await?;
    announce_never_fits(ctx, &merged.never_fits).await?;
    first_error.map_or(Ok(merged), Err)
}

/// Keep the first non-store error of a pass (later ones are logged); a
/// store error is returned at once, since the daemon stops on it.
fn keep_error(
    first: &mut Option<ArchonError>,
    result: Result<(), ArchonError>,
) -> Result<(), ArchonError> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.is_store() => Err(error),
        Err(error) if first.is_none() => {
            *first = Some(error);
            Ok(())
        }
        Err(error) => {
            tracing::error!(%error, "dispatch step failed");
            Ok(())
        }
    }
}

/// Spend and caps seen by the last admission, for budget blockers.
#[derive(Debug, Clone)]
struct BudgetView {
    caps: Caps,
    today_usd: f64,
    total_usd: f64,
}

struct Admission {
    plan: DispatchPlan,
    follow: FollowUp,
    budget: BudgetView,
}

/// Snapshot, plan and reserve under the run table, so no settle or other
/// pass changes capacity between the plan and its reservations. Repository
/// files are read before the table is taken.
async fn admit(
    ctx: &DaemonContext,
    catalog: &archon_core::model_tier::ModelCatalog,
) -> Result<Admission, ArchonError> {
    let caps = resolve::caps(ctx).await;
    let envs = snapshot::Envs::prefetch(ctx, ReadOn::Writer).await?;
    if envs.weighted(&ctx.config)
        && let Err(error) = crate::quota::refresh(ctx).await
    {
        tracing::warn!(%error, "provider quota refresh failed; using the last reading");
    }
    ctx.host.check(ctx.config.limits.host_limits());
    let mut table = ctx.runs.lock().await;
    let snapshot =
        snapshot::build(ctx, ReadOn::Writer, &table, caps.clone(), envs, catalog).await?;
    let mut plan = archon_queue::plan_dispatch(&snapshot);
    let budget = BudgetView {
        caps,
        today_usd: snapshot.spend_today_usd,
        total_usd: snapshot.spend_total_usd,
    };
    block_items(ctx, &plan.block).await?;
    plan.block
        .extend(block_invalid_models(ctx, &snapshot, catalog).await?);
    crate::quota::note_waits(ctx, &snapshot.pending_spawns).await?;
    note_model_waits(ctx, &snapshot.pending_spawns).await?;
    let mut follow = FollowUp::default();
    for (run, slots) in &plan.spawn {
        follow.merge(executor::reserve(ctx, &mut table, run, *slots, catalog).await?);
    }
    plan.start = mark_dispatching(ctx, &plan.start).await?;
    drop(table);
    Ok(Admission {
        plan,
        follow,
        budget,
    })
}

/// Tell the operator once why a live run cannot start its next stage.
async fn note_model_waits(
    ctx: &DaemonContext,
    spawns: &[archon_queue::dispatch::PendingSpawn],
) -> Result<(), ArchonError> {
    for spawn in spawns {
        let key = format!("dispatch_model_wait/{}", spawn.run);
        let message = spawn.model_hold.as_ref()
            .filter(|hold| hold.kind != ModelHoldKind::CatalogUnavailable)
            .map(|hold| format!("Run {} waits for a valid model: {}. Re-plan the item before retrying the stage.", spawn.run, hold.reason))
            .unwrap_or_default();
        let previous = ctx.store.lock().await.kv_get(&key)?.unwrap_or_default();
        if message == previous {
            continue;
        }
        if !message.is_empty() {
            executor::notify(
                ctx,
                NotificationKind::Blocker,
                &message,
                Some(&spawn.item),
                Some(&spawn.run),
            )
            .await?;
        }
        ctx.store.lock().await.kv_set(&key, &message)?;
    }
    Ok(())
}

/// Recheck invalid immutable pins before persisting an actionable block.
async fn block_invalid_models(
    ctx: &DaemonContext,
    snapshot: &archon_queue::DispatchSnapshot,
    catalog: &archon_core::model_tier::ModelCatalog,
) -> Result<Vec<QueueItemId>, ArchonError> {
    let mut blocked = Vec::new();
    for candidate in &snapshot.queued {
        if !candidate
            .model_hold
            .as_ref()
            .is_some_and(|hold| hold.kind == ModelHoldKind::InvalidPin)
        {
            continue;
        }
        let item = {
            let mut store = ctx.store.lock().await;
            let item = store.item(&candidate.item)?;
            if item.state != QueueState::Queued
                || crate::learned::cached_catalog(ctx, &store)?.as_ref() != Some(catalog)
            {
                continue;
            }
            let Some(reason) = snapshot::invalid_pin(&item, catalog) else {
                continue;
            };
            let reason = format!("{reason}; re-plan this item and retry");
            store.transition_item(
                &item.id,
                QueueState::model_pin_block(&reason),
                archon_core::now(),
            )?
        };
        let QueueState::Blocked { reason } = &item.state else {
            unreachable!("just blocked")
        };
        executor::notify(
            ctx,
            NotificationKind::Blocker,
            &format!("Blocked: {}. {reason}", item.title),
            Some(&item.id),
            None,
        )
        .await?;
        let news = TriggerEvent::ItemBlocked {
            item: item.id.clone(),
            reason: reason.clone(),
        };
        triggers::push(ctx, news).await?;
        blocked.push(item.id);
    }
    Ok(blocked)
}

/// `queued → dispatching` for admitted items; the snapshot counts them as
/// active runs from now on. Returns those actually moved. An item whose
/// worktree gc is collecting stays queued for a later pass.
async fn mark_dispatching(
    ctx: &DaemonContext,
    items: &[QueueItemId],
) -> Result<Vec<QueueItemId>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let now = archon_core::now();
    let mut moved = Vec::with_capacity(items.len());
    for id in items {
        let owner = store.stack_fix(id)?.map(|t| t.owner);
        if ctx.collecting.contains(id)
            || owner.as_ref().is_some_and(|id| ctx.collecting.contains(id))
        {
            tracing::info!(item = %id, "admitted item waits for worktree gc to finish");
            continue;
        }
        if let Some(why) = executor::held_by_stack(&store, id)? {
            tracing::info!(item = %id, %why, "admitted item waits for its stack");
            continue;
        }
        match store.transition_item(id, QueueState::Dispatching, now) {
            Ok(item) => {
                assert_eq!(item.state, QueueState::Dispatching);
                moved.push(item.id);
            }
            Err(ArchonError::InvalidTransition(why)) => {
                tracing::info!(item = %id, %why, "admitted item changed before dispatch");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(moved)
}

/// `queued → blocked` for items whose dependencies failed or were
/// cancelled, naming the dependency; a `Blocker` and a trigger each.
async fn block_items(ctx: &DaemonContext, items: &[QueueItemId]) -> Result<(), ArchonError> {
    for id in items {
        let blocked = {
            let mut store = ctx.store.lock().await;
            let item = store.item(id)?;
            let mut broken = Vec::new();
            for dep in &item.dependencies {
                let dep = store.item(dep)?;
                if matches!(
                    dep.state,
                    QueueState::Failed { .. } | QueueState::Cancelled { .. }
                ) {
                    broken.push(format!("{} ({:?}) {}", dep.id, dep.title, dep.state.kind()));
                }
            }
            assert!(!broken.is_empty(), "the planner blocks only broken items");
            let reason = format!("dependency {}", broken.join(", "));
            let next = QueueState::Blocked { reason };
            match store.transition_item(id, next, archon_core::now()) {
                Ok(item) => item,
                Err(ArchonError::InvalidTransition(_)) => continue,
                Err(e) => return Err(e),
            }
        };
        let QueueState::Blocked { reason } = &blocked.state else {
            unreachable!("just blocked");
        };
        let text = format!("Blocked: {}. {reason}", blocked.title);
        executor::notify(
            ctx,
            NotificationKind::Blocker,
            &text,
            Some(&blocked.id),
            None,
        )
        .await?;
        crate::push::blocked_by_dependency(ctx, &blocked);
        let news = TriggerEvent::ItemBlocked {
            item: blocked.id.clone(),
            reason: reason.clone(),
        };
        triggers::push(ctx, news).await?;
    }
    Ok(())
}

/// Post a `Blocker` and a `budget` trigger once per hold: once per local
/// day for the daily budget, once per reach of the total budget.
async fn announce_budget_hold(
    ctx: &DaemonContext,
    hold: Option<&Hold>,
    budget: &BudgetView,
) -> Result<(), ArchonError> {
    let (marker, scope, spent, limit) = match hold {
        Some(Hold::DailyBudget) => {
            let day = chrono::Local::now().date_naive();
            let limit = budget.caps.budget_usd_daily;
            (format!("daily:{day}"), "daily", budget.today_usd, limit)
        }
        Some(Hold::TotalBudget) => {
            let limit = budget.caps.budget_usd_total;
            ("total".to_owned(), "total", budget.total_usd, limit)
        }
        _ => (String::new(), "", 0.0, 0.0),
    };
    {
        let mut store = ctx.store.lock().await;
        let last = store.kv_get(BUDGET_HOLD_KEY)?.unwrap_or_default();
        if last == marker {
            return Ok(());
        }
        store.kv_set(BUDGET_HOLD_KEY, &marker)?;
    }
    if marker.is_empty() {
        return Ok(());
    }
    assert!(limit > 0.0, "a budget hold needs a budget");
    let text = format!(
        "The {scope} budget of {} is reached ({} spent). No new work starts; running agents finish.",
        archon_core::cost::format_usd(limit),
        archon_core::cost::format_usd(spent)
    );
    executor::notify(ctx, NotificationKind::Blocker, &text, None, None).await?;
    crate::push::budget(ctx, scope, &archon_core::cost::format_usd(limit));
    let news = TriggerEvent::Budget {
        scope: scope.to_owned(),
        spent_usd: spent,
        limit_usd: limit,
    };
    triggers::push(ctx, news).await
}

/// Post a `Blocker` once for each team that wants more agents at once than
/// its caps now allow, naming its stage and the cap; it waits otherwise
/// without a word. A team announced before and still over is not repeated.
async fn announce_never_fits(ctx: &DaemonContext, teams: &[NeverFits]) -> Result<(), ArchonError> {
    let marker = |t: &NeverFits| format!("{:?}/{}/{}", t.waiter, t.wanted, t.cap);
    let current: Vec<String> = teams.iter().map(marker).collect();
    let joined = current.join("\n");
    let last = ctx
        .store
        .lock()
        .await
        .kv_get(NEVER_FITS_KEY)?
        .unwrap_or_default();
    if joined == last {
        return Ok(());
    }
    let announced: Vec<&str> = last.lines().collect();
    for (team, key) in teams.iter().zip(&current) {
        if !announced.contains(&key.as_str()) {
            post_never_fits(ctx, team).await?;
        }
    }
    // Recorded only once every notice is out, so a failed post is retried.
    ctx.store.lock().await.kv_set(NEVER_FITS_KEY, &joined)
}

/// Post the `Blocker` for one team over its cap.
async fn post_never_fits(ctx: &DaemonContext, team: &NeverFits) -> Result<(), ArchonError> {
    assert!(
        team.cap > 0 && team.wanted > team.cap,
        "only a team over its cap"
    );
    let (item, run, stage) = match &team.waiter {
        GangWaiter::Run(run) => {
            let run = ctx.store.lock().await.run(run)?;
            let stage = run.state.live_stage().unwrap_or("?").to_owned();
            (
                run.item.clone(),
                Some(run.id),
                format!("team stage `{stage}`"),
            )
        }
        GangWaiter::Item(item) => (item.clone(), None, "its first stage, a team,".to_owned()),
    };
    let text = format!(
        "Item {item}: {stage} needs {} agents at once, but the caps now allow {} \
         (max_parallel_agents, max_per_repo or max_per_pipeline). It waits until a cap \
         is raised to at least {}, or the item is cancelled.",
        team.wanted, team.cap, team.wanted
    );
    executor::notify(
        ctx,
        NotificationKind::Blocker,
        &text,
        Some(&item),
        run.as_ref(),
    )
    .await
}

#[cfg(test)]
mod catalog_outage_tests {
    use super::*;

    #[test]
    fn announces_once_after_thirty_minutes_and_resets_after_recovery() {
        let mut outage = CatalogOutage::default();
        let start = Instant::now();
        assert!(!outage.observe(start, true));
        assert!(!outage.observe(start + Duration::from_secs(1799), true));
        assert!(outage.observe(start + Duration::from_secs(1800), true));
        assert!(!outage.observe(start + Duration::from_secs(7200), true));
        assert!(!outage.observe(start + Duration::from_secs(7201), false));
        assert!(!outage.observe(start + Duration::from_secs(7202), true));
        assert!(outage.observe(start + Duration::from_secs(9002), true));
    }
}
