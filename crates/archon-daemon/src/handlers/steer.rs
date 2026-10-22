//! `steer_item`: change a queued or running item without cancelling it
//! (ARCHITECTURE §6.6). A running item is steered through its live run; one
//! that waits has what it runs next changed; a failed one a shape steer
//! revives on its branch.

use archon_core::catalog::StageKind;
use archon_core::cost::format_usd;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelReasons};
use archon_core::protocol::{ClientKind, ItemPatch, Reply};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::{RunEdit, Steer, SteerRecord, SteeredBy};
use archon_pipeline::PipelineSpec;

use super::items;
use super::revise::{self, Ceiling};
use crate::context::DaemonContext;
use crate::executor::steer::{self as live, Asked, Current, Sources};
use crate::executor::{frozen, revived};
use crate::knobs;
use crate::resolve::RepoEnv;

/// An item can start or end between reading its state and taking the run
/// table; a steer follows it this often before it gives up.
const STATE_CHANGES: usize = 3;

/// The orchestrator steers through the MCP bridge, the operator otherwise.
fn steered_by(client: ClientKind) -> SteeredBy {
    match client {
        ClientKind::ArchonMcp => SteeredBy::Orchestrator,
        _ => SteeredBy::Operator,
    }
}

/// Replies `Reply::Item` with the item as steered (`ItemBudgetsOff` when
/// it set a budget while budgets are off). Its `autonomy` is checked first
/// and applied after the rest, as `update_item` would.
pub(super) async fn steer(
    ctx: &DaemonContext,
    client: ClientKind,
    id: &QueueItemId,
    mut steer: Steer,
) -> Result<Reply, ArchonError> {
    steer.check()?;
    let Some(knobs) = steer.autonomy.take() else {
        return steer_run_or_waiting(ctx, client, id, steer).await;
    };
    let patch = ItemPatch {
        autonomy: Some(knobs),
        ..ItemPatch::default()
    };
    if steer == Steer::default() {
        return items::update(ctx, id, patch).await;
    }
    let item = ctx.store.lock().await.item(id)?;
    revise::prepare(ctx, &item, &knobs, Ceiling::Enforced).await?;
    let steered = steer_run_or_waiting(ctx, client, id, steer).await?;
    let revised = items::update(ctx, id, patch).await?;
    Ok(match (steered, revised) {
        (Reply::ItemBudgetsOff(_), Reply::Item(item)) => Reply::ItemBudgetsOff(item),
        (_, revised) => revised,
    })
}

async fn steer_run_or_waiting(
    ctx: &DaemonContext,
    client: ClientKind,
    id: &QueueItemId,
    steer: Steer,
) -> Result<Reply, ArchonError> {
    let item = ctx.store.lock().await.item(id)?;
    let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
    let sources = Sources::read(&env, &item, &steer)?;
    let asked = Asked {
        by: steered_by(client),
        env: &env,
        sources: &sources,
        steer: &steer,
        plan: None,
    };
    let item = steer_as(ctx, &asked, id).await?;
    Ok(super::items::item_reply(ctx, item, steer.budget_usd.is_some()).await)
}

/// Steer the item through its live run or as it waits, whichever it is by
/// the time the run table is held.
async fn steer_as(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    id: &QueueItemId,
) -> Result<QueueItem, ArchonError> {
    for _ in 0..STATE_CHANGES {
        let state = ctx.store.lock().await.item(id)?.state;
        let steered = match state {
            QueueState::Running { .. } => live::steer_run(ctx, asked, id).await?,
            _ => steer_waiting(ctx, asked, id).await?,
        };
        if let Some(item) = steered {
            assert_eq!(&item.id, id, "the item that was steered");
            ctx.dispatch.notify_one();
            return Ok(item);
        }
    }
    Err(ArchonError::Unavailable(format!(
        "item {id} kept changing state while it was steered; steer it again"
    )))
}

/// `update_item`: a running item's `brief_append` and `model_plan` are
/// steered into its run; everything else is patched as on any item.
pub(super) async fn update(
    ctx: &DaemonContext,
    client: ClientKind,
    id: &QueueItemId,
    mut patch: ItemPatch,
) -> Result<Reply, ArchonError> {
    let item = ctx.store.lock().await.item(id)?;
    let steers = patch.brief_append.is_some() || patch.model_plan.is_some();
    let QueueState::Running { run } = &item.state else {
        return items::update(ctx, id, patch).await;
    };
    if !steers {
        return items::update(ctx, id, patch).await;
    }
    if let Some(knobs) = &patch.autonomy {
        revise::prepare(ctx, &item, knobs, Ceiling::Enforced).await?;
    }
    let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
    let steer = Steer {
        note: patch.brief_append.take(),
        ..Steer::default()
    };
    if steer.note.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return Err(ArchonError::Invalid(
            "brief_append is empty; give the text to add to the brief".into(),
        ));
    }
    let asked = Asked {
        by: steered_by(client),
        env: &env,
        sources: &Sources::default(),
        steer: &steer,
        plan: running_plan(ctx, &env, run, &mut patch).await?,
    };
    assert!(patch.brief_append.is_none() && patch.model_plan.is_none());
    if live::steer_run(ctx, &asked, id).await?.is_none() {
        return Err(ArchonError::Unavailable(format!(
            "item {id} ended its run while it was updated; send the update again"
        )));
    }
    items::update(ctx, id, patch).await
}

/// The model plan `patch` gives a running item, taken out of the patch with
/// its reasons, and filled and checked for the stages its run has now.
async fn running_plan(
    ctx: &DaemonContext,
    env: &RepoEnv,
    run: &archon_core::id::PipelineRunId,
    patch: &mut ItemPatch,
) -> Result<Option<(ModelPlan, Option<ModelReasons>)>, ArchonError> {
    let Some(plan) = patch.model_plan.take() else {
        return Ok(None);
    };
    let (run, item) = {
        let store = ctx.store.lock().await;
        let run = store.run(run)?;
        let item = store.item(&run.item)?;
        (run, item)
    };
    let spec = frozen::thaw(ctx, &run.id, &run.pipeline).await?;
    let reasons = patch.model_reasons.take();
    let route = crate::knobs::ItemRoute::new(&item);
    let given = items::resolve_draft(ctx, (env, route), &spec, plan, reasons.as_ref()).await?;
    let plan = items::filled_plan(ctx, route, env, &spec, &spec, given).await?;
    items::check_plan(ctx, env, &spec, &plan)?;
    items::check_reasons(&spec, reasons.as_ref())?;
    assert!(plan.0.len() >= spec.file.stages.len(), "a choice per stage");
    Ok(Some((plan, reasons)))
}

/// Steer an item with no live run. `None` when it is running after all.
async fn steer_waiting(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    id: &QueueItemId,
) -> Result<Option<QueueItem>, ArchonError> {
    assert!(asked.plan.is_none(), "only a live run is replanned whole");
    // Under the run table, so the dispatcher cannot start the item meanwhile.
    let _table = ctx.runs.lock().await;
    let loaded = ctx.store.lock().await.item(id)?;
    if matches!(loaded.state, QueueState::Running { .. }) {
        return Ok(None);
    }
    check_steerable(&loaded, asked.steer)?;
    let (_, spec) = knobs::item_specs(asked.env, &loaded)?;
    let spec = live::shaped(ctx, &loaded, spec).await?;
    let changed = changed_item(ctx, asked, &loaded, &spec).await?;
    let stored = store_steer(ctx, asked, &loaded, changed).await?;
    assert_eq!(stored.id, loaded.id, "the item that was steered");
    Ok(Some(stored))
}

/// Refuse what an item with no live run cannot take.
fn check_steerable(item: &QueueItem, steer: &Steer) -> Result<(), ArchonError> {
    let running = matches!(item.state, QueueState::Running { .. });
    assert!(!running, "a running item is steered through its run");
    let id = &item.id;
    let refuse = |why: &str| Err(ArchonError::InvalidTransition(format!("item {id} {why}")));
    match &item.state {
        QueueState::Dispatching => return refuse("is starting; steer it again in a moment"),
        QueueState::Succeeded { .. } => {
            return refuse("already succeeded; retry it, then steer it");
        }
        QueueState::Cancelled { .. } => {
            return refuse(
                "was cancelled, so nothing was steered; `archon queue retry` brings it back",
            );
        }
        _ => {}
    }
    if steer.abort.is_some() {
        return refuse(
            "has no running stage to abort or remove yet; stages can be steered once it runs",
        );
    }
    if steer.extend_minutes.is_some() {
        return refuse("has no running stage to give more time");
    }
    Ok(())
}

/// A waiting item as a steer leaves it, with nothing stored yet.
struct Changed {
    item: QueueItem,
    /// What changed, for the item's history.
    what: Vec<String>,
    /// The shape edit and the stage list it yields.
    shape: Option<(RunEdit, PipelineSpec)>,
}

/// Work out the steer against what the item runs next: its brief, budget,
/// stages and plan. `spec` is the stage list it has now.
async fn changed_item(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    loaded: &QueueItem,
    spec: &PipelineSpec,
) -> Result<Changed, ArchonError> {
    let steer = asked.steer;
    let mut item = loaded.clone();
    let mut what = Vec::new();
    if let Some(note) = &steer.note {
        item.amend_brief(note, archon_core::now())?;
        what.push("amended the brief".to_owned());
    }
    if let Some(budget) = steer.budget_usd {
        item.budget_usd = Some(budget);
        what.push(format!("set the budget to {}", format_usd(budget)));
    }
    let current = Current {
        spec,
        prefix: 0,
        removed: Default::default(),
    };
    let reshaped = {
        let store = ctx.store.lock().await;
        let ran = !store.runs_for_item(&item.id)?.is_empty();
        let reshaped = live::reshape(ctx, asked.env, &store, &item, current, asked)?;
        if let Some(reshaped) = reshaped.as_ref().filter(|_| ran) {
            check_same_layout(&item, spec, &reshaped.spec)?;
        }
        reshaped
    };
    let shape = match reshaped {
        Some(reshaped) => {
            items::check_runs_here(ctx, asked.env, &reshaped.spec).await?;
            item.model_plan = reshaped.plan;
            item.autonomy = reshaped.autonomy;
            what.push(reshaped.what);
            Some((reshaped.edit, reshaped.spec))
        }
        None => None,
    };
    let stages = shape.as_ref().map_or(spec, |(_, stages)| stages);
    items::check_stage_timeouts(ctx, stages, &steer.stage_timeouts)?;
    if !steer.stage_timeouts.is_empty() {
        item.stage_timeouts.extend(steer.stage_timeouts.clone());
        what.push("updated stage timeouts for future spawns".to_owned());
    }
    what.extend(retune(steer, stages, &mut item)?);
    items::check_plan(ctx, asked.env, stages, &item.model_plan)?;
    assert!(!what.is_empty(), "a checked steer changes something");
    Ok(Changed { item, what, shape })
}

/// An item that already ran has its worktree laid out for a team or for a
/// single agent; its stages cannot change which of the two it needs.
fn check_same_layout(
    item: &QueueItem,
    had: &PipelineSpec,
    has: &PipelineSpec,
) -> Result<(), ArchonError> {
    let team = |spec: &PipelineSpec| spec.file.stages.iter().any(|s| s.kind == StageKind::Team);
    if team(had) == team(has) {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "item {} already ran, and a team stage needs a worktree laid out differently from the \
         one it has; enqueue a new item for these stages",
        item.id
    )))
}

/// Store the steer and its record, and queue a failed item whose stages
/// changed again on its branch. The caller holds the run table.
async fn store_steer(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    loaded: &QueueItem,
    changed: Changed,
) -> Result<QueueItem, ArchonError> {
    let Changed {
        item,
        mut what,
        shape,
    } = changed;
    assert_eq!(loaded.state, item.state, "the state changes by transition");
    assert_eq!(
        loaded.id, item.id,
        "a steer changes the item it was asked for"
    );
    let now = archon_core::now();
    let mut store = ctx.store.lock().await;
    let mut item = item;
    let mut revive = false;
    // The failed run a shape steer revives; a steer is no correction of it.
    let mut revived_run = None;
    if let Some((edit, stages)) = &shape {
        let switched = match edit {
            RunEdit::SwitchPipeline { pipeline, .. } => Some(pipeline.as_str()),
            _ => None,
        };
        match asked.sources.switches_to(&item.pipeline) {
            true => live::forget_shape(&mut store, &item.id)?,
            false => live::keep_shape(&mut store, &item.id, stages, switched)?,
        }
        if let QueueState::Failed { run, .. } = &loaded.state {
            revive = true;
            if let Some(run) = run {
                item = revived(&store.run(run)?, edit, item)?;
            }
            revived_run = run.clone();
            what.push("queued it again on its branch".to_owned());
        }
    }
    let item = match revive {
        true => items::queue_again(&mut store, loaded, item)?,
        false => {
            item.updated_at = now.max(loaded.updated_at);
            store.update_item(&item)?;
            item
        }
    };
    let what = what.join("; ");
    let record = SteerRecord {
        at: now,
        by: asked.by,
        what: what.clone(),
        note: asked.steer.note.as_ref().map(|note| note.trim().to_owned()),
    };
    live::record(&mut store, &item.id, revived_run.as_ref(), record)?;
    drop(store);
    let text = live::history_line(&item.id, asked.by, &what, asked.steer.note.as_deref());
    items::post_note(ctx, &item.id, &text).await;
    Ok(item)
}

/// A new model or effort for one stage of a waiting item, which names the
/// stage: none is running.
fn retune(
    steer: &Steer,
    spec: &PipelineSpec,
    item: &mut QueueItem,
) -> Result<Option<String>, ArchonError> {
    if steer.model.is_none() && steer.effort.is_none() {
        return Ok(None);
    }
    let Some(stage) = steer.stage.as_deref() else {
        return Err(ArchonError::Invalid(format!(
            "item {} has no running stage; name the stage whose model or effort changes",
            item.id
        )));
    };
    let known = spec.stage(stage).is_some();
    if let (Some(effort), Some(default)) = (steer.effort, spec.default_effort(stage)) {
        knobs::check_effort_reason(stage, effort, default, item.model_reasons.as_ref())?;
    }
    let Some(choices) = item.model_plan.0.get_mut(stage).filter(|_| known) else {
        let ids: Vec<&str> = spec.file.stages.iter().map(|s| s.id.as_str()).collect();
        return Err(ArchonError::Invalid(format!(
            "item {} has no stage {stage:?}; its stages are {}",
            item.id,
            ids.join(", ")
        )));
    };
    assert!(!choices.is_empty(), "a plan gives every stage a choice");
    for choice in choices.iter_mut() {
        choice.model = steer.model.clone().unwrap_or_else(|| choice.model.clone());
        choice.effort = steer.effort.unwrap_or(choice.effort);
    }
    Ok(Some(format!(
        "stage `{stage}` now runs on {} at effort {}",
        choices[0].model, choices[0].effort
    )))
}
