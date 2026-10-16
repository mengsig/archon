//! The dispatch loop (ARCHITECTURE §7.3). Wakes on
//! `ctx.dispatch` or every `dispatcher.tick_secs`, builds a snapshot, calls
//! `archon_queue::plan_dispatch`, and executes the plan. No dispatch while
//! Aether is disconnected. Each pass first checks free disk space, and
//! while that holds dispatch, collects what finished runs left first.

mod disk_hold;
mod snapshot;

pub(crate) use disk_hold::{check as check_disk, read as read_disk};

use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::queue::{Caps, QueueState};
use archon_queue::DispatchPlan;
use archon_queue::dispatch::{GangWaiter, Hold, NeverFits};

use crate::context::DaemonContext;
use crate::executor::{self, FollowUp};
use crate::resolve;
use crate::triggers::{self, TriggerEvent};

/// The read-only view uses the dispatcher's complete snapshot and lock order.
/// It does not check disk, collect worktrees, reserve slots, or dispatch work.
pub(crate) async fn read_snapshot(
    ctx: &DaemonContext,
) -> Result<archon_queue::DispatchSnapshot, ArchonError> {
    let caps = resolve::caps(ctx).await;
    let envs = snapshot::Envs::prefetch(ctx).await?;
    let table = ctx.runs.lock().await;
    snapshot::build(ctx, &table, caps, envs).await
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
    loop {
        if ctx.aether.is_connected() {
            match dispatch_once(&ctx).await {
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

/// One planning pass; returns the plan it executed (for tests and Status).
/// A failing stage or item never holds back the rest of the pass: items
/// admitted as `dispatching` are always started. Its first error is
/// returned at the end; a store error ends the pass at once.
pub async fn dispatch_once(ctx: &DaemonContext) -> Result<DispatchPlan, ArchonError> {
    let mut first_error = None;
    disk_hold::check(ctx).await?;
    if crate::gc::before_dispatch(ctx).await? {
        disk_hold::check(ctx).await?;
    }
    keep_error(&mut first_error, executor::sweep_live_stages(ctx).await)?;
    let mut merged = DispatchPlan::default();
    let mut last = None;
    for _ in 0..MAX_PASSES {
        let admission = admit(ctx).await?;
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
async fn admit(ctx: &DaemonContext) -> Result<Admission, ArchonError> {
    let caps = resolve::caps(ctx).await;
    let envs = snapshot::Envs::prefetch(ctx).await?;
    if envs.weighted(&ctx.config)
        && let Err(error) = crate::quota::refresh(ctx).await
    {
        tracing::warn!(%error, "provider quota refresh failed; using the last reading");
    }
    let mut table = ctx.runs.lock().await;
    let snapshot = snapshot::build(ctx, &table, caps.clone(), envs).await?;
    let plan = archon_queue::plan_dispatch(&snapshot);
    let budget = BudgetView {
        caps,
        today_usd: snapshot.spend_today_usd,
        total_usd: snapshot.spend_total_usd,
    };
    block_items(ctx, &plan.block).await?;
    crate::quota::note_waits(ctx, &snapshot.pending_spawns).await?;
    let mut follow = FollowUp::default();
    for (run, slots) in &plan.spawn {
        follow.merge(executor::reserve(ctx, &mut table, run, *slots).await?);
    }
    let mut plan = plan;
    plan.start = mark_dispatching(ctx, &plan.start).await?;
    drop(table);
    Ok(Admission {
        plan,
        follow,
        budget,
    })
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
