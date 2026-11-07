//! The task graph and run plans (CONTRACTS.md §1.3, owner AR-G). The
//! `list_items` filters are applied by the store's item query.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::protocol::{
    QueueStateKind, Reply, TaskFilter, TaskGraph, WaitingReasonCode as Code, feature,
};
use archon_core::steer::RunEdit;
use archon_queue::Store;

use crate::context::DaemonContext;
use crate::dispatcher;
use crate::executor::{frozen, plan_view};

/// The features `welcome` advertises for this file's requests.
pub(super) const FEATURES: &[&str] = &[
    feature::TASK_GRAPH,
    feature::RUN_PLAN,
    feature::ITEM_FILTERS,
];

/// Reads of a task graph before it asks the client to retry.
const GRAPH_ATTEMPTS: usize = 5;

/// The graph and its `as_of`, both read in one store snapshot. Queued items'
/// waiting reasons come from the dispatcher's view, read before that snapshot;
/// it is all read again when anything was written between the two.
pub(super) async fn task_graph(
    ctx: &DaemonContext,
    filter: TaskFilter,
    limit: Option<u32>,
) -> Result<Reply, ArchonError> {
    for _ in 0..GRAPH_ATTEMPTS {
        let before = ctx.store.lock().await.write_count();
        let queued = queued_reasons(ctx).await?;
        let graph = graph_in_snapshot(ctx, &filter, limit, &queued).await?;
        let after = ctx.store.lock().await.write_count();
        assert!(after >= before, "the writer's count only grows");
        if after == before {
            return Ok(Reply::TaskGraph(Box::new(graph)));
        }
    }
    Err(ArchonError::Unavailable(format!(
        "the store kept changing while {GRAPH_ATTEMPTS} task graphs were read; retry"
    )))
}

/// Each queued item's waiting reason, as `list_item_summaries` gives it.
async fn queued_reasons(ctx: &DaemonContext) -> Result<BTreeMap<QueueItemId, Code>, ArchonError> {
    let snapshot = dispatcher::read_snapshot(ctx).await?;
    let mut codes = archon_queue::dispatch::waiting_reasons(&snapshot);
    assert_eq!(
        codes.len(),
        snapshot.queued.len(),
        "one reason per queued item"
    );
    if !ctx.aether.is_connected() {
        for code in codes.values_mut().filter(|c| **c == Code::NextInLine) {
            *code = Code::AetherDisconnected;
        }
    }
    Ok(codes)
}

async fn graph_in_snapshot(
    ctx: &DaemonContext,
    filter: &TaskFilter,
    limit: Option<u32>,
    queued: &BTreeMap<QueueItemId, Code>,
) -> Result<TaskGraph, ArchonError> {
    let estimates = ctx.config.limits.count_estimated_cost;
    let store = ctx.read_store().await?;
    let mut graph = store.task_graph(filter, limit, estimates)?;
    for node in &mut graph.nodes {
        match node.state {
            // An item enqueued after the dispatcher's view has no reason yet.
            QueueStateKind::Queued => node.waiting_reason = queued.get(&node.id).copied(),
            QueueStateKind::Running => node.waiting_reason = operator_gate(&store, &node.id)?,
            _ => {}
        }
    }
    assert_eq!(
        store.last_change_seq()?,
        graph.as_of,
        "the snapshot holds while the graph is read"
    );
    Ok(graph)
}

/// `operator_gate` while a running item's run waits on an open gate.
fn operator_gate(store: &Store, item: &QueueItemId) -> Result<Option<Code>, ArchonError> {
    let item = store.item(item)?;
    let runs = store.runs_for_item(&item.id)?;
    let stages = store.stage_runs_for_item(&item.id)?;
    let waiting = super::item_views::waiting_on(store, &item, &runs, &stages)?;
    Ok(waiting.map(|_| Code::OperatorGate))
}

/// A live run's frozen pipeline with every edit recorded for it applied and
/// its settings resolved; a finished run's plan as it was kept.
pub(super) async fn run_plan(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<Reply, ArchonError> {
    let (stored, recorded, item) = {
        let store = ctx.read_store().await?;
        let stored = store.run(run)?;
        let item = store.item(&stored.item)?;
        (stored, store.run_edits(run)?, item)
    };
    assert_eq!(&stored.id, run, "the run asked for");
    if stored.state.is_terminal() {
        return finished_plan(ctx, run).await;
    }
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    let thawed = frozen::thaw_edited(&ctx.paths.data_dir, run, &stored.pipeline, &edits).await;
    let spec = match thawed {
        Ok(spec) => spec,
        // A run that finished since it was read has dropped its snapshot.
        Err(error) if ctx.read_store().await?.run(run)?.state.is_terminal() => {
            tracing::debug!(%run, %error, "the run finished while its plan was read");
            return finished_plan(ctx, run).await;
        }
        Err(error) => return Err(error),
    };
    let plan = plan_view::plan_of(run, &spec, &edits, &item)?;
    assert!(!plan.finished, "a live run's plan");
    Ok(Reply::RunPlan(Box::new(plan)))
}

/// The plan kept when `run` finished; none when it finished before archond
/// kept plans, or its plan could not be written (logged then).
async fn finished_plan(ctx: &DaemonContext, run: &PipelineRunId) -> Result<Reply, ArchonError> {
    match plan_view::kept(&ctx.paths.data_dir, run).await? {
        Some(plan) => Ok(Reply::RunPlan(Box::new(plan))),
        None => Err(ArchonError::NotFound(format!(
            "the plan of run {run}: no plan was kept when it finished"
        ))),
    }
}
