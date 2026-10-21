//! Crash and restart recovery (ARCHITECTURE §16): drivers are rebuilt from
//! persisted stage runs with the run's original template variables.

use std::collections::BTreeMap;
use std::sync::Arc;

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageRun};
use archon_core::steer::{RunEdit, Then};
use archon_pipeline::autonomy::OperatorGates;
use archon_pipeline::{PipelineSpec, RunDriver};
use archon_queue::Store;

use super::{FollowUp, RunTable, finish, frozen, settle, ship, team};
use crate::context::DaemonContext;

const RUN_VARS_KEY_PREFIX: &str = "run_vars:";

fn run_vars_key(run: &PipelineRunId) -> String {
    format!("{RUN_VARS_KEY_PREFIX}{run}")
}

/// Stored at start so a rebuilt driver renders prompts exactly as before.
pub(super) fn save_run_vars(
    store: &mut Store,
    run: &PipelineRunId,
    vars: &BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    assert!(vars.contains_key("workspace"), "run variables are complete");
    if let Some(base) = vars.get("base_branch") {
        store.kv_set(&format!("run_base:{run}"), base)?;
    }
    store.kv_set(&run_vars_key(run), &serde_json::to_string(vars)?)
}

/// Whether `run` delivers with auto-merge on, as its item's `merge` knob now says.
pub(crate) fn set_run_auto_merge(
    store: &mut Store,
    run: &PipelineRunId,
    on: bool,
) -> Result<(), ArchonError> {
    let mut vars = load_run_vars(store, run)?;
    assert!(
        vars.contains_key("ship.auto_merge"),
        "a run's variables name its auto-merge"
    );
    vars.insert("ship.auto_merge".into(), on.to_string());
    save_run_vars(store, run, &vars)
}

pub(super) fn load_run_vars(
    store: &Store,
    run: &PipelineRunId,
) -> Result<BTreeMap<String, String>, ArchonError> {
    let raw = store
        .kv_get(&run_vars_key(run))?
        .ok_or_else(|| ArchonError::NotFound(format!("template variables of run {run}")))?;
    let vars: BTreeMap<String, String> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("template variables of run {run}: {e}")))?;
    if vars.is_empty() {
        return Err(ArchonError::Corrupt(format!(
            "template variables of run {run} are empty"
        )));
    }
    Ok(vars)
}

/// The branch the run forked from: the retained `run_base:` key, which outlives
/// the pruned variables, then the variables' `base_branch`. `None` when neither remains.
pub(super) fn forked_base(
    store: &Store,
    run: &PipelineRunId,
) -> Result<Option<String>, ArchonError> {
    if let Some(base) = store.kv_get(&format!("run_base:{run}"))? {
        return Ok(Some(base));
    }
    match load_run_vars(store, run) {
        Ok(vars) => Ok(vars.get("base_branch").cloned()),
        Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Delete the variables of finished runs, which only rebuild a live run's
/// driver, and of runs never inserted. Returns how many went.
pub(crate) fn prune_finished_run_vars(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(RUN_VARS_KEY_PREFIX)? {
        let raw = key
            .strip_prefix(RUN_VARS_KEY_PREFIX)
            .expect("ranged by prefix");
        let run = PipelineRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("run variables key {key}: {e}")))?;
        let found = match store.run(&run) {
            Ok(found) => Some(found),
            Err(ArchonError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        let finished = match &found {
            Some(found) => {
                assert_eq!(
                    found.state.is_terminal(),
                    found.finished_at.is_some(),
                    "a run has a finish time exactly when it finished"
                );
                found.state.is_terminal()
            }
            None => true,
        };
        if finished {
            if found.is_some() && store.kv_get(&format!("run_base:{run}"))?.is_none() {
                let vars = load_run_vars(store, &run)?;
                if let Some(base) = vars.get("base_branch") {
                    store.kv_set(&format!("run_base:{run}"), base)?;
                }
            }
            if found.is_none() {
                store.kv_delete(&format!("run_base:{run}"))?;
            }
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Requeue `dispatching` items that never got a run, close questions left
/// open on withdrawn gates and reopen questions no gate waits on, repair an
/// item left `dispatching` with a live run, and rebuild every live run's
/// driver.
/// Aether work (re-spawning `spawning` stage runs, stopping agents of
/// finished runs) is left to the dispatcher, which waits for a connection.
pub(super) async fn recover(ctx: &DaemonContext) -> Result<FollowUp, ArchonError> {
    let mut table = ctx.runs.lock().await;
    assert!(
        table.drivers.is_empty(),
        "recovery runs once, before dispatch"
    );
    let now = archon_core::now();
    reset_quarantine(ctx).await?;
    let (runs, answers) = {
        let mut store = ctx.store.lock().await;
        let requeued = store.requeue_orphaned_dispatching(now)?;
        if !requeued.is_empty() {
            tracing::info!(
                count = requeued.len(),
                "requeued dispatching items without a run"
            );
        }
        let mut answers = store.close_withdrawn_questions(now)?;
        answers.extend(store.close_decided_questions(now)?);
        if !answers.is_empty() {
            tracing::info!(
                count = answers.len(),
                "closed questions of withdrawn or decided gates"
            );
        }
        let (runs, unreadable) = store.active_runs_lenient()?;
        for bad in &unreadable {
            set_aside_unreadable_run(ctx, &mut store, bad, now)?;
        }
        let mut kept = Vec::with_capacity(runs.len());
        for run in runs {
            // A crash between insert_run and the item's transition.
            match repair_item(&mut store, &run, now) {
                Ok(()) => kept.push(run),
                Err(error) if error.is_store() => return Err(error),
                Err(error) => quarantine_run(ctx, &run, &error),
            }
        }
        (kept, answers)
    };
    crate::feed::publish(ctx, answers);
    crate::reports::close_stale_questions(ctx).await?;
    ctx.recovery.count(runs.len());
    let mut follow = FollowUp::poke();
    // One run at a time, each under the store's lock only briefly, so
    // `status` and other reads are answered between them.
    for run in runs {
        let stages = ctx.store.lock().await.stage_runs_strict(&run.id);
        let followed = match stages {
            Ok(stages) => resume_isolated(ctx, &mut table, run, &stages).await?,
            Err(error) => fail_unreadable(ctx, &mut table, run, error).await?,
        };
        follow.merge(followed);
        ctx.recovery.advance();
    }
    // Aether calls wait for the dispatcher; local cleanup, team merges and
    // the questions of gates opened just before a crash run now.
    Ok(FollowUp {
        cleanup: follow.cleanup,
        discard: follow.discard,
        gates: follow.gates,
        merge: follow.merge,
        poke: true,
        ..FollowUp::default()
    })
}

/// Forget the quarantine list, which recovery rebuilds from what is still
/// unusable, except runs that failed and wait for the operator's retry or cancel.
async fn reset_quarantine(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let data_dir = &ctx.paths.data_dir;
    let listed = match archon_core::quarantine::list(data_dir) {
        Ok(listed) => listed,
        Err(error) => {
            tracing::error!(%error, "the quarantine list could not be read; recovery goes on");
            Vec::new()
        }
    };
    let store = ctx.store.lock().await;
    let mut waiting = Vec::new();
    for record in listed {
        if record.kind == "run" && store.run_item_failed(&record.id)? {
            waiting.push(record);
        }
    }
    drop(store);
    if let Err(error) = archon_core::quarantine::clear_all(data_dir) {
        tracing::error!(%error, "the quarantine list could not be reset; recovery goes on");
    }
    for record in &waiting {
        if let Err(error) = archon_core::quarantine::write(data_dir, record) {
            tracing::error!(%error, run = %record.id, "a quarantine record was not kept");
        }
    }
    Ok(())
}

/// A live run whose stored record does not decode fails its item and is
/// retired, so the item can be retried; recovery goes on with the others.
fn set_aside_unreadable_run(
    ctx: &DaemonContext,
    store: &mut Store,
    bad: &archon_queue::store::UnreadableRun,
    now: archon_core::Timestamp,
) -> Result<(), ArchonError> {
    let item = archon_core::id::QueueItemId::parse(&bad.item)?;
    let message = format!(
        "the run record is unreadable ({}). `archon queue retry {item}` runs it again; \
         `archon queue cancel {item}` gives it up",
        bad.error
    );
    let record = archon_core::quarantine::Quarantined::new(
        "run",
        &bad.run,
        &format!("item {item}: {message}"),
        &format!("archon queue retry {item}"),
        &format!("archon queue cancel {item}"),
    );
    tracing::error!(run = %bad.run, %item, error = %bad.error, "run record unreadable; its item fails");
    store.retire_unreadable_run(&bad.run)?;
    let failed = QueueState::Failed {
        run: None,
        error: message,
    };
    store.transition_item(&item, failed, now)?;
    if let Err(write) = archon_core::quarantine::write(&ctx.paths.data_dir, &record) {
        tracing::error!(%write, "the quarantine record was not written");
    }
    Ok(())
}

/// A `dispatching` item whose run exists moves on to `running`.
fn repair_item(
    store: &mut Store,
    run: &PipelineRun,
    now: archon_core::Timestamp,
) -> Result<(), ArchonError> {
    if store.item(&run.item)?.state == QueueState::Dispatching {
        let running = QueueState::Running {
            run: run.id.clone(),
        };
        store.transition_item(&run.item, running, now)?;
    }
    Ok(())
}

/// Set `run` aside with why and the commands that fix or cancel its item.
fn quarantine_run(ctx: &DaemonContext, run: &PipelineRun, error: &ArchonError) {
    let item = run.item.to_string();
    let record = archon_core::quarantine::Quarantined::new(
        "run",
        run.id.as_str(),
        &format!("item {item}: {error}"),
        &format!("archon queue retry {item}"),
        &format!("archon queue cancel {item}"),
    );
    tracing::error!(run = %run.id, %item, %error, "run set aside; archond starts without it");
    if let Err(write) = archon_core::quarantine::write(&ctx.paths.data_dir, &record) {
        tracing::error!(%write, "the quarantine record was not written");
    }
}

fn unresumable_message(run: &PipelineRun, error: &ArchonError) -> String {
    format!(
        "cannot resume the run: {error}. `archon queue retry {item}` runs it again once \
         fixed; `archon queue cancel {item}` gives it up",
        item = run.item
    )
}

/// [`resume_run`] for one run whose stored state may be unusable: a failure
/// or panic on it fails that run's item with a blocker and sets the run aside,
/// and recovery goes on with the others. Only a store failure stops recovery.
async fn resume_isolated(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    stages: &[StageRun],
) -> Result<FollowUp, ArchonError> {
    let subject = format!("run {}", run.id);
    let resumed =
        super::coordination::guarded(&subject, resume_run(ctx, table, run.clone(), stages));
    let error = match resumed.await {
        Err(error) if !error.is_store() => error,
        other => return other,
    };
    quarantine_run(ctx, &run, &error);
    let stage = run.state.live_stage().unwrap_or("recovery").to_owned();
    let message = unresumable_message(&run, &error);
    let failed = finish::fail_run(ctx, table, run, stage, message, true);
    super::coordination::guarded(&subject, failed).await
}

/// A run whose stage rows cannot be read fails with the way out, so the
/// healthy runs still recover.
async fn fail_unreadable(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    quarantine_run(ctx, &run, &error);
    let dropped = ctx
        .store
        .lock()
        .await
        .drop_unreadable_stages(&run.id, &ctx.paths.data_dir.join("quarantine-rows"))?;
    for (id, data) in &dropped {
        tracing::error!(run = %run.id, stage_run = %id, bytes = data.len(), "unreadable stage row removed; saved under quarantine-rows");
    }
    let stage = run.state.live_stage().unwrap_or("recovery").to_owned();
    let message = unresumable_message(&run, &error);
    let failed = finish::fail_run(ctx, table, run.clone(), stage, message, true);
    super::coordination::guarded(&format!("run {}", run.id), failed).await
}

/// Rebuild `run`'s driver and apply what it still owes. A run whose
/// pipeline no longer loads fails with a `Blocker`.
pub(super) async fn resume_run(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    stages: &[StageRun],
) -> Result<FollowUp, ArchonError> {
    assert!(!run.state.is_terminal(), "only live runs are resumed");
    assert!(stages.iter().all(|s| s.run == run.id));
    let mut driver = match build_driver(ctx, &run).await {
        Ok(driver) => driver,
        Err(error) => {
            let stage = run.state.live_stage().unwrap_or("recovery").to_owned();
            quarantine_run(ctx, &run, &error);
            let message = unresumable_message(&run, &error);
            return finish::fail_run(ctx, table, run, stage, message, true).await;
        }
    };
    let actions = driver.resume(&run, stages);
    table.pending.remove(&run.id);
    table.merges.remove(&run.id);
    table.ships.remove(&run.id);
    table.gates.forget(&run.id);
    table.drivers.insert(run.id.clone(), driver);
    settle::apply(ctx, table, run, actions).await
}

/// What a run's driver is built from besides its gates and merges: as
/// stored, or as a steer would have it before it is stored.
#[derive(Debug, Clone)]
pub(super) struct Blueprint {
    pub item: QueueItem,
    /// The frozen pipeline after the recorded shape edits.
    pub spec: PipelineSpec,
    pub vars: BTreeMap<String, String>,
    /// Steered aborts still in force, in the order they were recorded.
    pub aborts: Vec<(String, u32, Then)>,
}

/// The run's blueprint as stored.
pub(super) async fn blueprint(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<Blueprint, ArchonError> {
    let (item, vars, recorded) = {
        let store = ctx.store.lock().await;
        let vars = load_run_vars(&store, &run.id)?;
        (store.item(&run.item)?, vars, store.run_edits(&run.id)?)
    };
    assert_eq!(item.id, run.item, "a run's blueprint is its item's");
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    let spec = frozen::thaw_edited(&ctx.paths.data_dir, &run.id, &run.pipeline, &edits).await?;
    let mut aborts = Vec::new();
    for edit in edits {
        match edit {
            RunEdit::AbortStage {
                stage,
                attempt,
                then,
            } => aborts.push((stage, attempt, then)),
            // The stages a removal was about went with the pipeline switched away.
            RunEdit::SwitchPipeline { .. } => aborts.retain(|(_, attempt, _)| *attempt != 0),
            RunEdit::InsertStage { .. } | RunEdit::AssignRoles { .. } => {}
        }
    }
    aborts.retain(|(stage, _, _)| spec.stage(stage).is_some());
    Ok(Blueprint {
        item,
        spec,
        vars,
        aborts,
    })
}

/// The driver `blueprint` describes, with every decision, merge outcome and
/// delivery recorded so far. Errors when its parts do not fit together.
pub(super) async fn driver_of(
    ctx: &DaemonContext,
    run: &PipelineRun,
    blueprint: Blueprint,
) -> Result<RunDriver, ArchonError> {
    let Blueprint {
        item,
        spec,
        vars,
        aborts,
    } = blueprint;
    assert_eq!(spec.name(), run.pipeline, "a run keeps its snapshot's name");
    let gates = ctx.store.lock().await.gates_for_run(&run.id)?;
    let operator_gates = OperatorGates::for_run(&spec, &item.autonomy);
    let experiments = crate::experiments::item_dir(&ctx.paths, &item.id);
    let mut driver = RunDriver::new(Arc::new(spec), item.model_plan, vars)?
        .with_gates(operator_gates)?
        .with_experiments(experiments);
    for gate in gates {
        assert_eq!(gate.run, run.id, "gates_for_run returns the run's gates");
        if let Some(decision) = gate.decision {
            driver.record_decision(&gate.stage_id, gate.attempt, decision)?;
        }
    }
    for (stage, attempt, then) in aborts {
        driver.record_abort(&stage, attempt, then)?;
    }
    let store = ctx.store.lock().await;
    team::replay_merges(&store, &run.id, &mut driver)?;
    if driver.ships_by_daemon() {
        ship::replay(&store, &run.id, &mut driver)?;
    }
    Ok(driver)
}

/// The driver as it was: the frozen pipeline with its recorded edits, the
/// item's gates, and every decision, merge outcome and abort recorded so far.
async fn build_driver(ctx: &DaemonContext, run: &PipelineRun) -> Result<RunDriver, ArchonError> {
    let blueprint = blueprint(ctx, run).await?;
    driver_of(ctx, run, blueprint).await
}
