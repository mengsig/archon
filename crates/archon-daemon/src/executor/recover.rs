//! Crash and restart recovery (ARCHITECTURE §16): drivers are rebuilt from
//! persisted stage runs with the run's original template variables.

use std::collections::BTreeMap;
use std::sync::Arc;

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, StageRun};
use archon_pipeline::RunDriver;
use archon_pipeline::autonomy::OperatorGates;
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
    store.kv_set(&run_vars_key(run), &serde_json::to_string(vars)?)
}

pub(super) fn load_run_vars(
    store: &Store,
    run: &PipelineRunId,
) -> Result<BTreeMap<String, String>, ArchonError> {
    let raw = store
        .kv_get(&run_vars_key(run))?
        .ok_or_else(|| ArchonError::NotFound(format!("template variables of run {run}")))?;
    let vars: BTreeMap<String, String> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("template variables of run {run}: {e}")))?;
    assert!(!vars.is_empty(), "stored run variables are never empty");
    Ok(vars)
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
            .map_err(|e| ArchonError::Store(format!("run variables key {key}: {e}")))?;
        let finished = match store.run(&run) {
            Ok(found) => {
                assert_eq!(
                    found.state.is_terminal(),
                    found.finished_at.is_some(),
                    "a run has a finish time exactly when it finished"
                );
                found.state.is_terminal()
            }
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if finished {
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
    let (runs, answers) = {
        let mut store = ctx.store.lock().await;
        let requeued = store.requeue_orphaned_dispatching(now)?;
        if !requeued.is_empty() {
            tracing::info!(
                count = requeued.len(),
                "requeued dispatching items without a run"
            );
        }
        let answers = store.close_withdrawn_questions(now)?;
        if !answers.is_empty() {
            tracing::info!(count = answers.len(), "closed questions of withdrawn gates");
        }
        let runs = store.active_runs()?;
        for run in &runs {
            // A crash between insert_run and the item's transition.
            if store.item(&run.item)?.state == QueueState::Dispatching {
                let running = QueueState::Running {
                    run: run.id.clone(),
                };
                store.transition_item(&run.item, running, now)?;
            }
        }
        (runs, answers)
    };
    crate::feed::publish(ctx, answers);
    crate::reports::close_stale_questions(ctx).await?;
    let mut follow = FollowUp::poke();
    for run in runs {
        let stages = ctx.store.lock().await.stage_runs(&run.id)?;
        follow.merge(resume_run(ctx, &mut table, run, &stages).await?);
    }
    // Aether calls wait for the dispatcher; local cleanup, team merges and
    // the questions of gates opened just before a crash run now.
    Ok(FollowUp {
        cleanup: follow.cleanup,
        gates: follow.gates,
        merge: follow.merge,
        poke: true,
        ..FollowUp::default()
    })
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
            let stage = run
                .state
                .live_stage()
                .expect("checked live above")
                .to_owned();
            let message = format!("cannot resume the run: {error}");
            return finish::fail_run(ctx, table, run, stage, message, true).await;
        }
    };
    let actions = driver.resume(&run, stages);
    table.pending.remove(&run.id);
    table.merges.remove(&run.id);
    table.ships.remove(&run.id);
    table.drivers.insert(run.id.clone(), driver);
    settle::apply(ctx, table, run, actions).await
}

/// The driver as it was: the frozen pipeline, the item's gates, and every
/// operator decision, team merge outcome and delivery recorded so far.
async fn build_driver(ctx: &DaemonContext, run: &PipelineRun) -> Result<RunDriver, ArchonError> {
    let (item, vars, gates) = {
        let store = ctx.store.lock().await;
        let gates = store.gates_for_run(&run.id)?;
        (
            store.item(&run.item)?,
            load_run_vars(&store, &run.id)?,
            gates,
        )
    };
    let spec = frozen::thaw(&ctx.paths.data_dir, &run.id, &run.pipeline).await?;
    let operator_gates = OperatorGates::for_run(&spec, &item.autonomy);
    let mut driver =
        RunDriver::new(Arc::new(spec), item.model_plan, vars)?.with_gates(operator_gates)?;
    for gate in gates {
        assert_eq!(gate.run, run.id, "gates_for_run returns the run's gates");
        if let Some(decision) = gate.decision {
            driver.record_decision(&gate.stage_id, gate.attempt, decision)?;
        }
    }
    let store = ctx.store.lock().await;
    team::replay_merges(&store, &run.id, &mut driver)?;
    if driver.ships_by_daemon() {
        ship::replay(&store, &run.id, &mut driver)?;
    }
    Ok(driver)
}
