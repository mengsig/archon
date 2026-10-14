//! Steering a live run without cancelling it (ARCHITECTURE §6.6). A steer is
//! worked out against the run as stored and checked by building the driver
//! it would leave; then it is stored together with what it owes the live
//! agents, and the run's driver is rebuilt from the store. That is the path
//! a restart takes, so a steered run replays the same after one.

mod audit;
mod owed;
mod plan;
mod shape;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QueueItemId, StageRunId};
use archon_core::model::{ModelPlan, ModelReasons};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_core::steer::{Steer, SteerRecord, SteeredBy};
use archon_queue::Store;

use super::spawn::StoredSpawn;
use super::{FollowUp, RunTable, load_spawn, recover, run_follow_up, settle, siblings, spawn};
use crate::context::DaemonContext;
use crate::resolve::RepoEnv;

pub(crate) use audit::{notes_since, record, records};
pub(crate) use owed::{deliver, owed};
pub(crate) use shape::{Current, Sources, forget as forget_shape, keep as keep_shape};
pub(crate) use shape::{correct_shaped, reshape, shaped};

use owed::Delivery;
use plan::{Change, Tune};

/// The least Aether is told an agent may still spend: zero would lift its cap.
const MIN_GRANT_USD: f64 = 0.01;
/// How much of a note the item's feed repeats.
const NOTE_PREVIEW_CHARS: usize = 240;

/// A steer with who asked and what was read for it before any lock.
#[derive(Debug, Clone)]
pub(crate) struct Asked<'a> {
    pub by: SteeredBy,
    pub env: &'a RepoEnv,
    pub sources: &'a Sources,
    pub steer: &'a Steer,
    /// A whole new, checked model plan (`update_item` on a running item).
    pub plan: Option<(ModelPlan, Option<ModelReasons>)>,
}

/// What an agent is told when a steer amended its task's brief.
pub(crate) fn note_message(note: &str) -> String {
    assert!(!note.trim().is_empty(), "a note says something");
    format!(
        "Archon steer: the brief of your task was amended. Take the amendment into account and \
         go on with your work; it does not restart your task.\n\n{}",
        note.trim()
    )
}

/// Steer the live run of item `id`. `None` when the item has no live run
/// (any more), so nothing changed.
pub(crate) async fn steer_run(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    id: &QueueItemId,
) -> Result<Option<QueueItem>, ArchonError> {
    let per_item_cap = crate::resolve::caps(ctx).await.budget_usd_per_item;
    let (follow, item) = {
        let mut table = ctx.runs.lock().await;
        let Some((run, stages)) = live_run(ctx, id).await? else {
            return Ok(None);
        };
        if table.merging.contains(&run.id) {
            return Err(ArchonError::Unavailable(format!(
                "the roles of {id} are being merged right now; steer it again in a moment"
            )));
        }
        let delivering = super::ship::delivering(&table, &*ctx.store.lock().await, &run.id)?;
        let change = plan::plan(ctx, asked, &run, &stages, per_item_cap, delivering).await?;
        let follow = apply(ctx, &mut table, run, change, asked.by).await?;
        (follow, ctx.store.lock().await.item(id)?)
    };
    assert_eq!(&item.id, id, "the store returns the item asked for");
    // Everything owed is stored, so the sweep repeats what fails here.
    if let Err(error) = run_follow_up(ctx, follow).await {
        tracing::warn!(item = %id, %error, "steer stored; the sweep repeats its follow-up work");
    }
    Ok(Some(item))
}

/// The item's live run with its stage runs.
async fn live_run(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<Option<(PipelineRun, Vec<StageRun>)>, ArchonError> {
    let store = ctx.store.lock().await;
    let QueueState::Running { run } = store.item(id)?.state else {
        return Ok(None);
    };
    let run = store.run(&run)?;
    if run.state.is_terminal() {
        return Ok(None);
    }
    assert_eq!(&run.item, id, "an item runs its own run");
    let stages = store.stage_runs(&run.id)?;
    Ok(Some((run, stages)))
}

/// Store `change`, rebuild the run's driver from the store, and return the
/// Aether work owed. The caller holds the run table.
async fn apply(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    change: Change,
    by: SteeredBy,
) -> Result<FollowUp, ArchonError> {
    assert!(!run.state.is_terminal(), "only a live run is steered");
    assert!(!change.what.is_empty(), "a steer changes something");
    let now = archon_core::now();
    let what = change.what.join("; ");
    let mut follow = FollowUp::poke();
    {
        let mut store = ctx.store.lock().await;
        store_blueprint(&mut store, &run, &change, now)?;
        follow.merge(cancel_group(&mut store, table, &change, by, now)?);
        follow.steers = owe_live(&mut store, &change, by, now)?;
        let record = SteerRecord {
            at: now,
            by,
            what: what.clone(),
            note: change.note.clone(),
        };
        audit::record(&mut store, &run.item, Some(&run.id), record)?;
    }
    let text = history_line(&run.item, by, &what, change.note.as_deref());
    let noted = run.clone();
    if change.rebuild {
        let stages = ctx.store.lock().await.stage_runs(&run.id)?;
        follow.merge(recover::resume_run(ctx, table, run, &stages).await?);
    }
    settle::progress(ctx, &noted, text).await?;
    Ok(follow)
}

/// The line a steer leaves in the item's feed.
pub(crate) fn history_line(
    item: &QueueItemId,
    by: SteeredBy,
    what: &str,
    note: Option<&str>,
) -> String {
    assert!(!what.trim().is_empty(), "a steer changed something");
    match note {
        Some(note) => {
            let preview = super::clip(note.trim(), NOTE_PREVIEW_CHARS);
            format!("Steered {item} ({by}): {what}. Note: {preview}")
        }
        None => format!("Steered {item} ({by}): {what}."),
    }
}

/// The item, the run's variables and its edits, as the steer leaves them.
fn store_blueprint(
    store: &mut Store,
    run: &PipelineRun,
    change: &Change,
    now: Timestamp,
) -> Result<(), ArchonError> {
    let stored = store.item(&run.item)?;
    let mut item = change.blueprint.item.clone();
    assert_eq!(stored.id, item.id, "a steer changes its own item");
    assert_eq!(stored.state, item.state, "a steer leaves the item's state");
    if item != stored {
        item.updated_at = now.max(stored.updated_at);
        store.update_item(&item)?;
    }
    recover::save_run_vars(store, &run.id, &change.blueprint.vars)?;
    for edit in &change.edits {
        store.append_run_edit(&run.id, edit, now)?;
    }
    if change.reshaped {
        shape::keep(store, &run.item, &change.blueprint.spec)?;
    }
    Ok(())
}

/// Settle the aborted group's live stage runs as cancelled, each with the
/// stop its agent is owed: the driver then acts on the abort.
fn cancel_group(
    store: &mut Store,
    table: &mut RunTable,
    change: &Change,
    by: SteeredBy,
    now: Timestamp,
) -> Result<FollowUp, ArchonError> {
    let reason = format!("aborted by a steer ({by})");
    let mut follow = FollowUp::default();
    for id in &change.cancel {
        let stage = store.stage_run(id)?;
        assert!(!stage.state.is_terminal(), "planned under the run table");
        // A spawn still in flight has its agent stopped once it is recorded.
        if let Some(agent) = &stage.aether_agent {
            siblings::owe(store, id, agent, &reason)?;
            follow.sibling_stops.push(id.clone());
        }
        let outcome = StageOutcome::Cancelled {
            reason: Some(reason.clone()),
        };
        store.upsert_stage_run(&settle::settled_stage(stage, outcome, now))?;
        table.spawn_errors.remove(id);
    }
    let mut stops = follow.sibling_stops.iter();
    assert!(
        stops.all(|id| change.cancel.contains(id)),
        "only the aborted group is stopped"
    );
    Ok(follow)
}

/// Put the steer on the stored request of every live stage run it reaches
/// and owe its agents the rest. Returns the stage runs owed something.
fn owe_live(
    store: &mut Store,
    change: &Change,
    by: SteeredBy,
    now: Timestamp,
) -> Result<Vec<StageRunId>, ArchonError> {
    let mut owing = Vec::new();
    for (id, tune) in &change.live {
        if change.cancel.contains(id) {
            continue;
        }
        let mut stored = load_spawn(store, id)?;
        let lead = Delivery {
            key: NotificationId::new().as_str().to_owned(),
            choice: tune.choice.clone(),
            cost_budget_usd: regranted(&mut stored, tune),
            deadline: extended(&mut stored, tune, now),
            interrupt: tune.choice.is_some(),
            lead: lead_message(change, tune, by),
            helpers: None,
        };
        spawn::save_spawn(store, id, &stored)?;
        if !lead.is_empty() {
            owed::owe(store, id, lead)?;
        }
        if let Some(note) = &change.note {
            let helpers = Delivery {
                key: NotificationId::new().as_str().to_owned(),
                helpers: Some(note_message(note)),
                ..Delivery::default()
            };
            owed::owe(store, id, helpers)?;
        }
        owing.push(id.clone());
    }
    assert!(owing.len() <= change.live.len(), "one entry per stage run");
    Ok(owing)
}

/// The stage run's new deadline, on its stored request for the sweep's timer.
fn extended(stored: &mut StoredSpawn, tune: &Tune, now: Timestamp) -> Option<Timestamp> {
    let minutes = tune.extend_minutes?;
    let deadline = stored.request.deadline?;
    // A deadline that just passed is extended from now, not into the past.
    let later = deadline.max(now) + chrono::Duration::minutes(i64::from(minutes));
    assert!(later > deadline, "an extension moves the deadline out");
    stored.request.deadline = Some(later);
    Some(later)
}

/// What the stage run's agent may spend after the item's budget changed;
/// `Some` when Aether enforces the cap and must be told.
fn regranted(stored: &mut StoredSpawn, tune: &Tune) -> Option<f64> {
    let delta = tune.budget_delta?;
    let granted = stored.request.cost_budget_usd?;
    assert!(
        delta.is_finite() && granted.is_finite(),
        "budgets are amounts"
    );
    let now = (granted + delta).max(MIN_GRANT_USD);
    stored.request.cost_budget_usd = Some(now);
    (stored.request.aether_cost_budget == Some(true)).then_some(now)
}

/// What the stage's own agent is told: its new model, the note, or both.
fn lead_message(change: &Change, tune: &Tune, by: SteeredBy) -> Option<String> {
    let retuned = tune.choice.as_ref().map(|choice| {
        format!(
            "Archon steer ({by}): you now run on {} at effort {}. Go on with your task where \
             you left off; nothing you did is lost.",
            choice.model, choice.effort
        )
    });
    let note = change.note.as_deref().map(note_message);
    match (retuned, note) {
        (Some(retuned), Some(note)) => Some(format!("{retuned}\n\n{note}")),
        (retuned, note) => retuned.or(note),
    }
}
