//! Working out what a steer does to a live run, with nothing stored: the
//! blueprint its driver is rebuilt from, the edits to record, the stage runs
//! to end, and what each live stage run's agents are owed.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::catalog::StageKind;
use archon_core::cost::format_usd;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model::ModelChoice;
use archon_core::run::{PipelineRun, RunState, StageRun};
use archon_core::steer::{RunEdit, Steer, Then};
use archon_pipeline::spec::StageSpec;
use archon_pipeline::{PipelineSpec, reshape};

use super::{Asked, shape};
use crate::context::DaemonContext;
use crate::executor::recover::{self, Blueprint};
use crate::knobs;

/// The template variable every stage prompt reads the brief from.
const BRIEF_VARIABLE: &str = "task.brief";

/// What a steer puts on one live stage run: its stored request and agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Tune {
    pub choice: Option<ModelChoice>,
    pub extend_minutes: Option<u32>,
    /// Dollars added to (or taken from) what the agent may still spend.
    pub budget_delta: Option<f64>,
}

/// One steer of a live run, worked out and checked.
#[derive(Debug, Clone)]
pub(super) struct Change {
    /// What the run's driver is built from once the steer is stored.
    pub blueprint: Blueprint,
    pub edits: Vec<RunEdit>,
    /// The stages changed, so the item keeps them for its later runs.
    pub reshaped: bool,
    /// Live stage runs of an aborted group.
    pub cancel: Vec<StageRunId>,
    /// Every live stage run whose agents the steer reaches.
    pub live: BTreeMap<StageRunId, Tune>,
    pub note: Option<String>,
    /// For the item's history, one entry per thing changed.
    pub what: Vec<String>,
    /// The driver reads something the steer changed.
    pub rebuild: bool,
}

/// Where the run is while it is steered.
struct At<'a> {
    run: &'a PipelineRun,
    stages: &'a [StageRun],
    /// The stage `model`, `effort` and `abort` are about.
    target: &'a str,
    /// A delivery of the run is queued, running or recorded.
    delivering: bool,
}

impl At<'_> {
    fn live(&self) -> impl Iterator<Item = &StageRun> {
        self.stages
            .iter()
            .filter(|stage| !stage.state.is_terminal())
    }
}

fn no_stage(spec: &PipelineSpec, stage: &str) -> ArchonError {
    let ids: Vec<&str> = spec.file.stages.iter().map(|s| s.id.as_str()).collect();
    ArchonError::Invalid(format!(
        "the run has no stage {stage:?}; its stages are {}",
        ids.join(", ")
    ))
}

/// A steer whose result no driver can run is the steer's mistake.
fn not_runnable(error: ArchonError) -> ArchonError {
    match error {
        ArchonError::Pipeline(why) | ArchonError::Invalid(why) => {
            ArchonError::Invalid(format!("the run could not continue with this steer: {why}"))
        }
        other => other,
    }
}

/// Work out `asked` for `run`, whose stage runs are `stages`, and check it
/// by building the driver it would leave. `delivering`: see [`At`].
pub(super) async fn plan(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    run: &PipelineRun,
    stages: &[StageRun],
    per_item_cap: f64,
    delivering: bool,
) -> Result<Change, ArchonError> {
    assert!(stages.iter().all(|stage| stage.run == run.id));
    let live_stage = run.state.live_stage().expect("a live run is at a stage");
    let target = asked.steer.stage.as_deref().unwrap_or(live_stage);
    let at = At {
        run,
        stages,
        target,
        delivering,
    };
    let mut change = Change {
        blueprint: recover::blueprint(ctx, run).await?,
        edits: Vec::new(),
        reshaped: false,
        cancel: Vec::new(),
        live: BTreeMap::new(),
        note: None,
        what: Vec::new(),
        rebuild: false,
    };
    amend(&mut change, asked.steer, &at)?;
    replan(&mut change, asked, &at);
    reshape_run(ctx, asked, &mut change, &at).await?;
    crate::handlers::check_stage_timeouts(
        ctx,
        &change.blueprint.spec,
        &asked.steer.stage_timeouts,
    )?;
    if !asked.steer.stage_timeouts.is_empty() {
        change
            .blueprint
            .item
            .stage_timeouts
            .extend(asked.steer.stage_timeouts.clone());
        change
            .what
            .push("updated stage timeouts for future spawns".to_owned());
        change.rebuild = true;
    }
    retune(ctx, asked, &mut change, &at)?;
    rebudget(&mut change, asked.steer, &at, per_item_cap);
    extend(&mut change, asked.steer, &at)?;
    abort(ctx, asked, &mut change, &at)?;
    let dry = recover::driver_of(ctx, run, change.blueprint.clone()).await;
    dry.map_err(not_runnable)?;
    assert!(!change.what.is_empty(), "a checked steer changes something");
    Ok(change)
}

/// The note amends the brief every later prompt reads, and reaches the
/// agents working now.
fn amend(change: &mut Change, steer: &Steer, at: &At<'_>) -> Result<(), ArchonError> {
    let Some(note) = &steer.note else {
        return Ok(());
    };
    let blueprint = &mut change.blueprint;
    blueprint.item.amend_brief(note, archon_core::now())?;
    let brief = blueprint.vars.get_mut(BRIEF_VARIABLE);
    *brief.expect("run variables hold the brief") = blueprint.item.brief.clone();
    for stage in at.live() {
        change.live.entry(stage.id.clone()).or_default();
    }
    change.note = Some(note.trim().to_owned());
    change.what.push("amended the brief".to_owned());
    change.rebuild = true;
    Ok(())
}

/// A whole new model plan (`update_item`): for the agents started from now,
/// and on each live agent whose slot it gives another model or effort.
fn replan(change: &mut Change, asked: &Asked<'_>, at: &At<'_>) {
    let Some((plan, reasons)) = &asked.plan else {
        return;
    };
    for stage in at.live() {
        // A plan that leaves a stage out is refused when the driver is built.
        let Some(choices) = plan.choices_for(&stage.stage_id).filter(|c| !c.is_empty()) else {
            continue;
        };
        let choice = &choices[stage.fan_index as usize % choices.len()];
        if choice != stage.current_choice() {
            change.live.entry(stage.id.clone()).or_default().choice = Some(choice.clone());
        }
    }
    let item = &mut change.blueprint.item;
    item.model_plan = plan.clone();
    item.model_reasons = reasons.clone();
    change.what.push("replaced the model plan".to_owned());
    change.rebuild = true;
}

/// The stages a steer removed before they started: the run passes them over.
fn removed(blueprint: &Blueprint) -> BTreeSet<String> {
    let standing = blueprint
        .aborts
        .iter()
        .filter(|(_, attempt, _)| *attempt == 0);
    standing.map(|(stage, _, _)| stage.clone()).collect()
}

/// Whether the first `prefix` stages of `had` and `has` are the same ones.
fn same_start(had: &PipelineSpec, has: &PipelineSpec, prefix: usize) -> bool {
    let ids = |spec: &PipelineSpec| -> Option<Vec<String>> {
        let started = spec.file.stages.get(..prefix)?;
        Some(started.iter().map(|stage| stage.id.clone()).collect())
    };
    ids(had).is_some() && ids(had) == ids(has)
}

/// An inserted stage or a switched pipeline, for the stages not yet started.
async fn reshape_run(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    change: &mut Change,
    at: &At<'_>,
) -> Result<(), ArchonError> {
    let blueprint = &change.blueprint;
    let ran = at.stages.iter().map(|stage| stage.stage_id.as_str());
    let started = ran.chain(at.run.state.live_stage());
    let prefix = reshape::started_prefix(&blueprint.spec, started);
    let current = shape::Current {
        spec: &blueprint.spec,
        prefix,
        removed: removed(blueprint),
    };
    let reshaped = {
        let store = ctx.store.lock().await;
        shape::reshape(ctx, asked.env, &store, &blueprint.item, current, asked)?
    };
    let Some(reshaped) = reshaped else {
        return Ok(());
    };
    assert!(
        same_start(&blueprint.spec, &reshaped.spec, prefix),
        "the stages that started keep their place"
    );
    refuse_while_delivering(at)?;
    let gone = match reshaped.edit {
        RunEdit::SwitchPipeline { .. } => BTreeSet::new(),
        _ => removed(blueprint),
    };
    refuse_unshipped(&blueprint.spec, &reshaped.spec, &gone)?;
    let planned = |stage: &StageSpec| reshaped.plan.choices_for(&stage.id).is_some();
    assert!(
        reshaped.spec.file.stages.iter().all(planned),
        "the plan has a choice for every stage of the new shape"
    );
    let mut stages = reshaped.spec.file.stages.iter();
    if let Some(stage) = stages.find(|stage| stage.kind == StageKind::Team)
        && at.run.team_root.is_none()
    {
        return Err(ArchonError::Invalid(format!(
            "stage {:?} is a team stage, and this run was started without team worktrees; \
             cancel the item and retry it on that pipeline instead",
            stage.id
        )));
    }
    let blueprint = &mut change.blueprint;
    if matches!(reshaped.edit, RunEdit::SwitchPipeline { .. }) {
        blueprint.aborts.retain(|(_, attempt, _)| *attempt != 0);
    }
    blueprint.spec = reshaped.spec;
    blueprint.item.model_plan = reshaped.plan;
    blueprint.item.autonomy = reshaped.autonomy;
    change.edits.push(reshaped.edit);
    change.reshaped = true;
    change.what.push(reshaped.what);
    change.rebuild = true;
    Ok(())
}

/// Refuse to change the stages of a run whose delivery is queued, running or
/// recorded: it would ship the commit the stages it replaces left.
fn refuse_while_delivering(at: &At<'_>) -> Result<(), ArchonError> {
    if !at.delivering {
        return Ok(());
    }
    assert!(!at.run.state.is_terminal(), "only a live run is steered");
    Err(ArchonError::InvalidTransition(format!(
        "run {} is delivering its change already, so its stages can no longer change; steer it \
         with a note, or retry the item with the new stages once it finished",
        at.run.id
    )))
}

/// Refuse a steer that leaves a run of `had`, which archond delivers, with
/// no stage that ships in `left` but those in `gone`: it would ship nothing.
fn refuse_unshipped(
    had: &PipelineSpec,
    left: &PipelineSpec,
    gone: &BTreeSet<String>,
) -> Result<(), ArchonError> {
    let Some(record) = had.ship_record_stage() else {
        return Ok(());
    };
    assert!(
        had.stage_index(&record.id).is_some(),
        "validated: a stage of the run"
    );
    assert!(
        !left.file.stages.is_empty(),
        "a run keeps at least one stage"
    );
    let stays = |id: &str| left.stage_index(id).is_some() && !gone.contains(id);
    let recorded = left
        .ship_record_stage()
        .is_some_and(|stage| stays(&stage.id));
    let mut shippers = left
        .file
        .stages
        .iter()
        .filter(|s| s.kind == StageKind::Ship);
    if recorded || shippers.any(|stage| stays(&stage.id)) {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "stage `{0}` writes the ship record this run is delivered from, and the steer leaves \
         no stage that writes one or ships, so the run would deliver nothing; keep `{0}`, or \
         bring a stage that writes a ship record",
        record.id
    )))
}

/// `choice` with the model and the effort the steer names.
fn retuned(choice: &ModelChoice, steer: &Steer) -> ModelChoice {
    ModelChoice {
        model: steer.model.clone().unwrap_or_else(|| choice.model.clone()),
        effort: steer.effort.unwrap_or(choice.effort),
        tier: None,
    }
}

/// A new model or effort for the target stage: in the plan for its later
/// agents, and on the agents it runs now.
fn retune(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    change: &mut Change,
    at: &At<'_>,
) -> Result<(), ArchonError> {
    let steer = asked.steer;
    if steer.model.is_none() && steer.effort.is_none() {
        return Ok(());
    }
    let blueprint = &mut change.blueprint;
    if blueprint.spec.stage(at.target).is_none() {
        return Err(no_stage(&blueprint.spec, at.target));
    }
    if let (Some(effort), Some(default)) = (steer.effort, blueprint.spec.default_effort(at.target))
    {
        let reasons = blueprint.item.model_reasons.as_ref();
        knobs::check_effort_reason(at.target, effort, default, reasons)?;
    }
    let plan = &mut blueprint.item.model_plan;
    let choices = plan.0.get_mut(at.target).filter(|c| !c.is_empty());
    let Some(choices) = choices else {
        return Err(ArchonError::Invalid(format!(
            "the item's model plan has no choice for stage {:?} to change",
            at.target
        )));
    };
    for choice in choices.iter_mut() {
        *choice = retuned(choice, steer);
    }
    let now = choices[0].clone();
    let model_named = steer.model.as_ref().is_none_or(|model| *model == now.model);
    assert!(
        model_named && steer.effort.is_none_or(|e| e == now.effort),
        "the plan runs the steer"
    );
    asked.env.policy(&ctx.config).check_plan(plan)?;
    for stage in at.live().filter(|stage| stage.stage_id == at.target) {
        let choice = retuned(stage.current_choice(), steer);
        if &choice != stage.current_choice() {
            change.live.entry(stage.id.clone()).or_default().choice = Some(choice);
        }
    }
    change.what.push(format!(
        "stage `{}` now runs on {} at effort {}",
        at.target, now.model, now.effort
    ));
    change.rebuild = true;
    Ok(())
}

/// The item's new dollar cap; live agents with a cap of their own get the
/// difference, a team's roles each their share.
fn rebudget(change: &mut Change, steer: &Steer, at: &At<'_>, per_item_cap: f64) {
    let Some(budget) = steer.budget_usd else {
        return;
    };
    assert!(budget.is_finite() && budget > 0.0, "checked by Steer");
    let blueprint = &mut change.blueprint;
    let before = blueprint.item.budget_usd.unwrap_or(per_item_cap);
    blueprint.item.budget_usd = Some(budget);
    change
        .what
        .push(format!("set the budget to {}", format_usd(budget)));
    // An item that ran uncapped has agents with no cap to move.
    if before <= 0.0 {
        return;
    }
    for stage in at.live() {
        let spec = blueprint.spec.stage(&stage.stage_id);
        let sharing = match spec.filter(|spec| spec.kind == StageKind::Team) {
            Some(team) => f64::from(team.fan_out),
            None => 1.0,
        };
        let tune = change.live.entry(stage.id.clone()).or_default();
        tune.budget_delta = Some((budget - before) / sharing);
    }
}

/// More time for every stage run working now.
fn extend(change: &mut Change, steer: &Steer, at: &At<'_>) -> Result<(), ArchonError> {
    let Some(minutes) = steer.extend_minutes else {
        return Ok(());
    };
    assert!(minutes > 0, "checked by Steer");
    let mut extended = 0;
    for stage in at.live() {
        change
            .live
            .entry(stage.id.clone())
            .or_default()
            .extend_minutes = Some(minutes);
        extended += 1;
    }
    if extended == 0 {
        return Err(ArchonError::InvalidTransition(format!(
            "no stage of {} is working right now, so there is no deadline to extend",
            at.run.item
        )));
    }
    change.what.push(format!(
        "gave stage `{}` {minutes} more minute(s)",
        at.run.state.live_stage().expect("a live run is at a stage")
    ));
    Ok(())
}

/// The attempt of the target stage an abort ends: the running one, or 0 for
/// a stage that never started, which can only be removed.
fn aborted_attempt(at: &At<'_>) -> Result<u32, ArchonError> {
    let target = at.target;
    match &at.run.state {
        RunState::Running { stage_id, attempt } if stage_id == target => return Ok(*attempt),
        RunState::AwaitingOperator { stage_id, .. } if stage_id == target => {
            return Err(ArchonError::InvalidTransition(format!(
                "stage {target:?} passed and waits for the operator's decision; decide its \
                 gate instead of aborting it"
            )));
        }
        _ => {}
    }
    if at.stages.iter().any(|stage| stage.stage_id == target) {
        let live = at.run.state.live_stage().expect("a live run is at a stage");
        return Err(ArchonError::InvalidTransition(format!(
            "stage {target:?} already ran and is not running now, so it cannot be aborted or \
             removed; to run it again, abort the running stage {live:?} with goto {target:?}"
        )));
    }
    Ok(0)
}

/// The stages the run passes over when the stage at `index` is aborted.
fn passed_over(spec: &PipelineSpec, index: usize, then: &Then) -> Vec<String> {
    let stages = &spec.file.stages;
    let until = match then {
        Then::Retry => index,
        Then::Skip => index + 1,
        Then::Goto { to } => spec.stage_index(to).unwrap_or(index),
    };
    let passed = stages.get(index..until.max(index)).unwrap_or_default();
    passed.iter().map(|stage| stage.id.clone()).collect()
}

/// End the target stage's running attempt, or remove a stage that has not
/// started; the run then does what the steer says instead of judging it.
fn abort(
    ctx: &DaemonContext,
    asked: &Asked<'_>,
    change: &mut Change,
    at: &At<'_>,
) -> Result<(), ArchonError> {
    let Some(then) = asked.steer.abort.clone() else {
        return Ok(());
    };
    assert!(change.cancel.is_empty(), "a steer aborts one group");
    refuse_while_delivering(at)?;
    let blueprint = &mut change.blueprint;
    let Some(index) = blueprint.spec.stage_index(at.target) else {
        return Err(no_stage(&blueprint.spec, at.target));
    };
    let attempt = aborted_attempt(at)?;
    if attempt == 0 && then != Then::Skip {
        return Err(ArchonError::InvalidTransition(format!(
            "stage {:?} has not started, so there is nothing to abort; it can only be removed \
             (abort with skip)",
            at.target
        )));
    }
    let mut gone = removed(blueprint);
    gone.extend(passed_over(&blueprint.spec, index, &then));
    refuse_unshipped(&blueprint.spec, &blueprint.spec, &gone)?;
    let knobs = blueprint.item.autonomy;
    blueprint.item.autonomy =
        shape::within_ceiling(ctx, asked.env, &blueprint.spec, &gone, knobs, knobs)?;
    blueprint
        .aborts
        .push((at.target.to_owned(), attempt, then.clone()));
    change.edits.push(RunEdit::AbortStage {
        stage: at.target.to_owned(),
        attempt,
        then: then.clone(),
    });
    let group = at
        .live()
        .filter(|s| s.stage_id == at.target && s.attempt == attempt);
    change.cancel = group.map(|stage| stage.id.clone()).collect();
    assert!(
        attempt > 0 || change.cancel.is_empty(),
        "a stage that never started has no agents to stop"
    );
    let what = aborted(at.target, attempt, &then);
    let now = blueprint.item.autonomy.review;
    change
        .what
        .push(shape::noting_review(what, knobs.review, now));
    change.rebuild = true;
    Ok(())
}

/// For the item's history: what aborting `attempt` of stage `target` did.
fn aborted(target: &str, attempt: u32, then: &Then) -> String {
    if attempt == 0 {
        return format!("removed stage `{target}`");
    }
    let next = match then {
        Then::Retry => "retries it".to_owned(),
        Then::Skip => "skips it".to_owned(),
        Then::Goto { to } => format!("goes to stage `{to}`"),
    };
    format!("aborted stage `{target}` (attempt {attempt}); the run {next}")
}
