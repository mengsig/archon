//! The task graph and run plans (CONTRACTS.md §1.3, owner AR-G). The
//! `list_items` filters are applied by the store's item query.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::protocol::{
    QueueStateKind, Reply, RunPlan, TaskFilter, TaskGraph, WaitingReasonCode as Code, feature,
};
use archon_core::steer::RunEdit;
use archon_pipeline::PipelineSpec;
use archon_queue::Store;

use crate::context::DaemonContext;
use crate::dispatcher;
use crate::executor::frozen;

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

/// A live run's frozen pipeline with every edit recorded for it applied. A
/// finished run's snapshot is gone, so its plan is not kept.
pub(super) async fn run_plan(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<Reply, ArchonError> {
    let (stored, recorded) = {
        let store = ctx.read_store().await?;
        (store.run(run)?, store.run_edits(run)?)
    };
    assert_eq!(&stored.id, run, "the run asked for");
    if stored.state.is_terminal() {
        return Err(finished(run));
    }
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    let thawed = frozen::thaw_edited(&ctx.paths.data_dir, run, &stored.pipeline, &edits).await;
    let spec = match thawed {
        Ok(spec) => spec,
        // A run that finished since it was read has dropped its snapshot.
        Err(error) if ctx.read_store().await?.run(run)?.state.is_terminal() => {
            tracing::debug!(%run, %error, "the run finished while its plan was read");
            return Err(finished(run));
        }
        Err(error) => return Err(error),
    };
    let plan = RunPlan {
        run: run.clone(),
        pipeline: spec.name().to_owned(),
        revision: revision(&spec)?,
        stages: spec.summary().stages,
        edited: edits.iter().any(changes_stages),
    };
    assert!(!plan.stages.is_empty(), "a run keeps a stage");
    Ok(Reply::RunPlan(Box::new(plan)))
}

fn finished(run: &PipelineRunId) -> ArchonError {
    ArchonError::NotFound(format!(
        "the plan of run {run}: it finished, and archond keeps a run's frozen pipeline \
         only while it runs"
    ))
}

/// Whether a steer's edit changed the stages, not only how one runs.
fn changes_stages(edit: &RunEdit) -> bool {
    match edit {
        RunEdit::InsertStage { .. } | RunEdit::SwitchPipeline { .. } => true,
        RunEdit::AbortStage { .. } | RunEdit::AssignRoles { .. } => false,
    }
}

/// 64-bit FNV-1a, in hex, of the plan's manifest and prompts.
fn revision(spec: &PipelineSpec) -> Result<String, ArchonError> {
    let mut contents = spec.manifest()?;
    for (name, prompt) in &spec.prompts {
        contents.push_str(name);
        contents.push('\0');
        contents.push_str(prompt);
    }
    assert!(!contents.is_empty(), "a manifest is never empty");
    let hash = contents.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok(format!("{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_inserted_and_switched_stages_mark_a_plan_edited() {
        let insert = RunEdit::InsertStage {
            after: None,
            spec: serde_json::json!({}),
        };
        let switch = RunEdit::SwitchPipeline {
            pipeline: "quick".into(),
            stages: Vec::new(),
        };
        let roles = RunEdit::AssignRoles {
            stage: "team".into(),
            roles: vec!["a".into()],
            role_prompts: BTreeMap::new(),
        };
        assert!(changes_stages(&insert) && changes_stages(&switch));
        assert!(!changes_stages(&roles));
    }

    #[test]
    fn the_revision_is_fnv1a_of_the_manifest() {
        let catalog = archon_pipeline::catalog::Catalog::builtin();
        let spec = catalog.get("direct").expect("a built-in pipeline").clone();
        let revision = revision(&spec).unwrap();
        assert_eq!(revision.len(), 16, "{revision}");
        assert_eq!(revision, super::revision(&spec.clone()).unwrap(), "stable");
        let mut changed = spec;
        changed.prompts.insert("extra.md".into(), "more".into());
        assert_ne!(super::revision(&changed).unwrap(), revision);
    }
}
