//! The task graph and run plans as archond serves them (CONTRACTS.md §1.3):
//! a steered run's plan is the stages it runs, and the graph's waiting
//! reasons are the ones the summaries give.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, ProposalMode};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    EnqueueRequest, ItemQuery, Reply, RepoRef, Request, RunPlan, TaskFilter, TaskGraph,
    WaitingReasonCode,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::{InsertStage, Steer};
use common::{Harness, PIPELINE, REPO_NAME, completed, plan, verdict};

/// The pipeline the steer borrows stage `verify` from.
const EXTRA: &str = r#"[pipeline]
name = "graph-extra"
description = "a stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "verify"
kind = "work"
prompt = "verify.md"
timeout_minutes = 30
"#;

async fn harness() -> Harness {
    let h = Harness::new().await;
    h.add_pipeline("graph-extra", EXTRA);
    let prompt = h.repo.join(".archon/pipelines/graph-extra/verify.md");
    std::fs::write(prompt, "Verify {{task.title}}.\n\n{{task.brief}}\n").expect("prompt");
    h
}

async fn run_plan(h: &Harness, item: &QueueItemId) -> Result<RunPlan, ArchonError> {
    let run = h.run(item).await.id;
    match h.cli(Request::RunPlan { run: run.clone() }).await? {
        Reply::RunPlan(plan) => {
            assert_eq!(plan.run, run, "the plan of the run asked for");
            Ok(*plan)
        }
        other => panic!("run_plan replied {other:?}"),
    }
}

fn stage_ids(plan: &RunPlan) -> Vec<&str> {
    plan.stages.iter().map(|s| s.id.as_str()).collect()
}

/// The stages the item's run started, each once, in the order they started.
async fn executed(h: &Harness, item: &QueueItemId) -> Vec<String> {
    let mut ids: Vec<String> = h
        .stages(item)
        .await
        .into_iter()
        .map(|s| s.stage_id)
        .collect();
    ids.dedup();
    ids
}

async fn graph(h: &Harness) -> TaskGraph {
    let request = Request::TaskGraph {
        filter: TaskFilter::default(),
        limit: None,
    };
    match h.cli(request).await.expect("task_graph") {
        Reply::TaskGraph(graph) => *graph,
        other => panic!("task_graph replied {other:?}"),
    }
}

/// Each summary's waiting reason code, by item.
async fn summary_codes(h: &Harness) -> Vec<(QueueItemId, Option<WaitingReasonCode>)> {
    let query = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    let Ok(Reply::ItemSummaries(rows)) = h.cli(Request::ListItemSummaries(query)).await else {
        panic!("list_item_summaries failed");
    };
    let mut codes: Vec<_> = rows
        .into_iter()
        .map(|row| (row.item.id, row.waiting_reason.map(|r| r.code)))
        .collect();
    codes.sort_by(|a, b| a.0.cmp(&b.0));
    codes
}

#[tokio::test]
async fn a_steered_runs_plan_is_the_stages_it_ran() {
    let h = harness().await;
    let item = h.enqueue("verify before shipping").await;
    h.dispatch().await;
    let frozen = run_plan(&h, &item.id).await.expect("a live run's plan");
    assert_eq!(stage_ids(&frozen), ["implement", "review", "ship"]);
    assert!(!frozen.edited, "nothing steered it yet");
    assert_eq!(frozen.pipeline, PIPELINE);

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let steer = Steer {
        insert_stage: Some(InsertStage {
            stage: "verify".into(),
            from: Some("graph-extra".into()),
            after: None,
        }),
        ..Steer::default()
    };
    let request = Request::SteerItem {
        item: item.id.clone(),
        steer,
    };
    h.cli(request).await.expect("verify is inserted");
    let steered = run_plan(&h, &item.id).await.expect("the steered plan");
    assert!(steered.edited);
    assert_ne!(
        steered.revision, frozen.revision,
        "the edit changed the plan"
    );
    assert_eq!(
        stage_ids(&steered),
        ["implement", "review", "verify", "ship"]
    );

    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    h.settle_running(|_| completed("verified")).await;
    h.dispatch().await;
    let shipping: Vec<_> = h.running().await.into_iter().map(|s| s.stage_id).collect();
    assert_eq!(shipping, ["ship"]);
    let last = run_plan(&h, &item.id)
        .await
        .expect("the plan while it ships");
    assert_eq!(last, steered, "the plan holds while the run goes on");
    assert_eq!(executed(&h, &item.id).await, stage_ids(&last));
}

#[tokio::test]
async fn a_finished_runs_plan_is_not_kept() {
    let h = harness().await;
    let item = h.enqueue("ship it").await;
    h.dispatch().await;
    for _ in 0..6 {
        h.settle_running(|stage| match stage.stage_id.as_str() {
            "review" => verdict(true),
            _ => completed("done"),
        })
        .await;
        h.dispatch().await;
    }
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let gone = run_plan(&h, &item.id).await.expect_err("finished");
    assert!(
        matches!(&gone, ArchonError::NotFound(m) if m.contains("finished")),
        "{gone}"
    );
}

/// Enqueue with a proposal stage and leave its run at the proposal gate.
async fn at_proposal_gate(h: &Harness, title: &str) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            proposal: Some(ProposalMode::Lavish),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.expect("enqueued") else {
        panic!("enqueue replies with the item");
    };
    h.dispatch().await;
    let page = h.run(&item.id).await.workspace;
    let page = page.join(".archon/reports/proposal/index.html");
    std::fs::create_dir_all(page.parent().expect("a directory")).expect("page directory");
    std::fs::write(&page, "<html><body>page</body></html>").expect("page");
    h.settle_running(|_| completed("Approach: add a flag."))
        .await;
    h.dispatch().await;
    item
}

#[tokio::test]
async fn the_graph_gives_the_waiting_reasons_the_summaries_give() {
    let h = harness().await;
    let gated = at_proposal_gate(&h, "gated").await;
    let queued = h.enqueue("waiting its turn").await;
    let graph = graph(&h).await;
    let reason = |id: &QueueItemId| {
        let node = graph.nodes.iter().find(|n| &n.id == id).expect("a node");
        node.waiting_reason
    };
    assert_eq!(reason(&gated.id), Some(WaitingReasonCode::OperatorGate));
    assert!(
        reason(&queued.id).is_some(),
        "a queued item says why it waits"
    );
    let mut from_graph: Vec<_> = graph
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.waiting_reason))
        .collect();
    from_graph.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(from_graph, summary_codes(&h).await);
    let as_of = h.ctx.store.lock().await.last_change_seq().expect("seq");
    assert_eq!(graph.as_of, as_of, "nothing was written since");
}

#[tokio::test]
async fn list_items_serves_the_text_filter() {
    let h = harness().await;
    let wanted = h.enqueue("Fix the parser").await;
    h.enqueue("unrelated").await;
    let query = ItemQuery {
        text: Some("PARSER".into()),
        ..ItemQuery::default()
    };
    let Ok(Reply::Items(items)) = h.cli(Request::ListItems(query)).await else {
        panic!("list_items with text failed");
    };
    let ids: Vec<_> = items.into_iter().map(|i| i.id).collect();
    assert_eq!(ids, [wanted.id]);
}
