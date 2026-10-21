//! The dispatcher and executor against a scripted backend: a pipeline
//! through its gates, caps under concurrent settles, pause, cancel, model
//! policy refusal at spawn, and infrastructure retries.

use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use archon_aether::AetherEvent;
use archon_aether::events::EventKind;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::model::Effort;
use archon_core::notify::NotificationKind;
use archon_core::ports::{DescendantState, NetworkPolicy};
use archon_core::protocol::{PauseTarget, Request};
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, StageState};
use archon_daemon::{dispatcher, executor, pump};
use common::delivery::{
    FakeGh, add_origin, origin_of, pass_by, pushed, reviewed_by, shipped_by, verdict_on,
};
use common::{
    Harness, MODEL, REPO_NAME, REVIEW_MODEL, choice, completed, expire_deadline, git, verdict,
};

/// What a well-behaved agent of each stage reports.
fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

/// The first spawn: the planned model and effort, the stage contract, and
/// the run's worktree.
fn assert_implementer(h: &Harness, run: &PipelineRun, title: &str) {
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "one implementer");
    let implement = &requests[0];
    assert_eq!(implement.choice, choice(MODEL, Effort::Medium));
    assert_eq!(implement.workspace, run.workspace);
    assert!(
        run.workspace.join("README.md").exists(),
        "the worktree is checked out"
    );
    assert_eq!(implement.repository.as_deref(), Some(REPO_NAME));
    assert!(implement.objective.contains(title));
    assert!(
        !implement.instructions.is_empty(),
        "the stage contract is passed"
    );
    assert_eq!(implement.labels["archon.stage"], "implement");
    assert_eq!(implement.labels["archon.attempt"], "1");
    assert_eq!(implement.labels["archon.fan"], "0");
    assert_eq!(
        implement.labels["archon.stage_run"],
        implement.idempotency_key
    );
    assert_eq!(
        implement.name,
        format!("#{} implement · {title}", short_item(&run.item))
    );
    assert!(implement.leaf && implement.deadline.is_some());
    assert_run_node(h, run, title);
    assert_eq!(implement.parent, Some(h.backend.node_ids()[0].clone()));
}

/// The run's node: one passive agent, keyed by the run, scoped to its
/// worktree and named after the item, that every stage agent sits under.
fn assert_run_node(h: &Harness, run: &PipelineRun, title: &str) {
    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 1, "one node per run: {nodes:?}");
    let node = &nodes[0];
    assert_eq!(node.idempotency_key, format!("{}:node:1", run.id));
    assert_eq!(node.role, format!("archon-run:{}", run.pipeline));
    assert_eq!(node.name, title);
    assert_eq!(node.workspace, run.workspace);
    assert_eq!(node.repository.as_deref(), Some(REPO_NAME));
    assert_eq!(node.labels["archon.run"], run.id.as_str());
    assert_eq!(node.labels["archon.item"], run.item.as_str());
    assert_eq!(node.model, choice(MODEL, Effort::Medium).model);
    assert_eq!(
        node.attach_run, None,
        "the node starts the run's Aether run"
    );
    let parents: Vec<_> = h
        .backend
        .requests()
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert!(parents.iter().all(|p| p.is_some()), "{parents:?}");
}

/// Both reviewers run with their planned choices; the first fails.
async fn one_reviewer_fails(h: &Harness) {
    let reviewers = h.running().await;
    let choices: Vec<_> = reviewers
        .iter()
        .map(|s| (s.fan_index, s.choice.clone()))
        .collect();
    let expected = [
        (0, choice(REVIEW_MODEL, Effort::Medium)),
        (1, choice(MODEL, Effort::Low)),
    ];
    assert_eq!(choices, expected);
    h.settle_running(|s| match s.fan_index {
        0 => StageOutcome::Completed {
            summary: "missing tests for the greeting\nVERDICT: fail".into(),
            structured: None,
        },
        _ => verdict(true),
    })
    .await;
}

#[tokio::test]
async fn review_failure_loops_back_then_ships() {
    let h = Harness::new().await;
    let item = h.enqueue("add a greeting").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(
        h.state(&item.id).await,
        QueueState::Running {
            run: run.id.clone()
        }
    );
    assert_eq!(
        run.workspace,
        h.ctx.paths.worktrees.join(REPO_NAME).join(item.id.as_str())
    );
    assert_implementer(&h, &run, "add a greeting");

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    one_reviewer_fails(&h).await;
    h.dispatch().await;
    let again = h.running().await;
    assert_eq!(again.len(), 1);
    assert_eq!(
        (again[0].stage_id.as_str(), again[0].attempt),
        ("implement", 2)
    );
    let first = &h.stages(&item.id).await[0];
    assert_eq!(again[0].aether_agent, first.aether_agent, "the same agent");
    assert_eq!(again[0].aether_task, None, "resumed, not spawned");
    assert_eq!(h.backend.requests().len(), 3, "no second implementer");
    let findings = h.backend.resumed_with(&again[0]);
    assert!(
        findings.contains("missing tests"),
        "review feedback reaches the fix: {findings}"
    );
    // Reviewers are issued in stage-run id order, which is random.
    let reviewers: BTreeSet<String> = h.backend.requests()[1..3]
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(
        reviewers,
        BTreeSet::from([
            format!("#{} review 1/2 · add a greeting", short_item(&item.id)),
            format!("#{} review 2/2 · add a greeting", short_item(&item.id))
        ])
    );

    for expected in ["implement", "review", "ship"] {
        let settled = h.settle_running(passing).await;
        assert!(
            settled.iter().all(|s| s.stage_id == expected),
            "{expected}: {settled:?}"
        );
        h.dispatch().await;
    }
    let QueueState::Succeeded { run: done, .. } = h.state(&item.id).await else {
        panic!("item did not succeed: {:?}", h.state(&item.id).await);
    };
    assert_eq!(done, run.id);
    assert!(h.live_stages().await.is_empty());
    assert!(
        !run.workspace.exists(),
        "a shipped PR's worktree is removed"
    );
    let branch = format!("refs/heads/{}", run.branch);
    git(&h.repo, &["show-ref", "--verify", &branch]);
    let spawns = h.backend.requests().len();
    assert_eq!(h.backend.created(), spawns, "no spawn was repeated");
    assert!(!h.notices(NotificationKind::Outcome).await.is_empty());
    let node = h.backend.node_ids();
    assert_eq!(node.len(), 1, "every stage of the run shared one node");
    let parents: Vec<_> = h
        .backend
        .requests()
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert!(
        parents.iter().all(|p| p.as_ref() == Some(&node[0])),
        "{parents:?}"
    );
    assert_eq!(
        h.backend.cancelled(),
        node,
        "only the node is cancelled, once the run is done"
    );
    assert_eq!(
        h.kv(&format!("run_node:{}", run.id)).await,
        None,
        "a retired node is forgotten"
    );
}

/// Implement and both reviews pass; the ship stage is running.
async fn drive_to_ship(h: &Harness) {
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(passing).await;
    h.dispatch().await;
    let ship = h.running().await;
    assert_eq!(ship.len(), 1);
    assert_eq!(ship[0].stage_id, "ship");
}

/// The replacement node's create is answered late: the reviewer that sent
/// it hears nothing, and the other reviewer, which runs another model,
/// replays the stored request rather than one of its own.
#[tokio::test]
async fn a_lost_node_reply_is_replayed_with_the_stored_request() {
    let h = Harness::new().await;
    let item = h.enqueue("lost node").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let first = h.backend.node_ids()[0].clone();
    h.settle_running(|_| completed("implemented")).await;
    h.backend.kill(&first);
    h.backend.lose_next_node_reply();
    h.dispatch().await;
    assert_eq!(
        h.running().await.len(),
        1,
        "one reviewer waits for its node"
    );
    h.dispatch().await;
    let reviewers = h.running().await;
    assert_eq!(reviewers.len(), 2);
    let models: BTreeSet<_> = reviewers.iter().map(|s| s.choice.model.clone()).collect();
    assert_eq!(models.len(), 2, "the reviewers run different models");

    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 3, "generation 1, then 2 lost and 2 replayed");
    assert_eq!(nodes[2].idempotency_key, format!("{}:node:2", run.id));
    assert_eq!(nodes[1], nodes[2], "the stored request, byte for byte");
    assert_eq!(h.backend.nodes_created(), 2);
    let node = h.backend.node_ids()[1].clone();
    let parents: BTreeSet<_> = h.backend.requests()[1..]
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert_eq!(parents, BTreeSet::from([Some(node)]));
}

/// Two dispatch passes race to replace a lost node: one generation, one
/// request, and both reviewers under the same replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_dispatches_agree_on_one_replacement_node() {
    let h = Harness::new().await;
    let item = h.enqueue("racing replacement").await;
    h.dispatch().await;
    let first = h.backend.node_ids()[0].clone();
    h.settle_running(|_| completed("implemented")).await;
    h.backend.kill(&first);
    let passes: Vec<_> = (0..2)
        .map(|_| {
            let ctx = h.ctx.clone();
            tokio::spawn(async move { dispatcher::dispatch_once(&ctx).await.map(|_| ()) })
        })
        .collect();
    for pass in passes {
        pass.await.expect("no panic").expect("dispatch pass");
    }
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2);
    assert_eq!(h.backend.nodes_created(), 2, "one replacement");
    let run = h.run(&item.id).await;
    let second: Vec<_> = h
        .backend
        .node_requests()
        .into_iter()
        .filter(|r| r.idempotency_key == format!("{}:node:2", run.id))
        .collect();
    assert!(!second.is_empty());
    assert!(
        second.iter().all(|r| *r == second[0]),
        "every generation-2 create sends the same request: {second:?}"
    );
    let node = h.backend.node_ids()[1].clone();
    let parents: BTreeSet<_> = h.backend.requests()[1..]
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert_eq!(parents, BTreeSet::from([Some(node)]));
}

#[tokio::test]
async fn an_unanswered_node_cancel_is_retried_by_the_sweep() {
    let h = Harness::new().await;
    let item = h.enqueue("slow cancel").await;
    drive_to_ship(&h).await;
    let run = h.run(&item.id).await;
    let key = format!("run_node:{}", run.id);
    h.backend
        .fail_next_cancel(ArchonError::Unavailable("aether away".into()));
    h.settle_running(passing).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(
        h.backend.cancelled().is_empty(),
        "the cancel went unanswered"
    );
    assert!(h.kv(&key).await.is_some(), "the node is remembered");

    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    assert_eq!(h.kv(&key).await, None, "retired and forgotten");
    h.dispatch().await;
    assert_eq!(h.backend.cancelled().len(), 1, "retired once");
}

/// A cancel that fails outright is reported, and the sweep still retires
/// the node later.
#[tokio::test]
async fn a_failed_node_cancel_is_reported_then_retried() {
    let h = Harness::new().await;
    let item = h.enqueue("broken cancel").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    h.backend
        .fail_next_cancel(ArchonError::Aether("internal: boom".into()));
    let agent = ship.aether_agent.clone().expect("agent");
    let settled = executor::stage_settled(&h.ctx, &agent, passing(&ship)).await;
    assert!(
        matches!(settled, Err(ArchonError::Aether(_))),
        "the failure surfaces: {settled:?}"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
}

/// Spawns waiting for a node Aether has not answered for are cancelled
/// with their run without creating an agent; the pending node create is
/// replayed under its key so a node Aether did create is cancelled too.
#[tokio::test]
async fn unsent_stages_of_a_finished_run_are_cancelled_without_a_create() {
    let h = Harness::new().await;
    let item = h.enqueue("cancel before create").await;
    h.dispatch().await;
    let first = h.backend.node_ids()[0].clone();
    h.settle_running(|_| completed("implemented")).await;
    h.backend.kill(&first);
    for _ in 0..2 {
        h.backend
            .fail_next_node(ArchonError::Unavailable("aether away".into()));
    }
    h.dispatch().await;
    let waiting = h.live_stages().await;
    assert_eq!(waiting.len(), 2);
    assert!(waiting.iter().all(|s| s.state == StageState::Spawning));
    let sent = h.backend.requests().len();

    executor::cancel_item(&h.ctx, &item.id, "changed my mind")
        .await
        .expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let stages = h.stages(&item.id).await;
    assert!(
        stages[1..].iter().all(|s| s.state == StageState::Cancelled),
        "{stages:?}"
    );
    assert_eq!(
        h.backend.requests().len(),
        sent,
        "no stage agent was created"
    );
    let nodes = h.backend.node_requests();
    assert_eq!(
        nodes.len(),
        4,
        "generation 1; 2 twice unanswered; 2 replayed to retire"
    );
    assert_eq!(nodes[3], nodes[1]);
    assert_eq!(h.backend.cancelled(), vec![h.backend.node_ids()[1].clone()]);
    let run = h.run(&item.id).await;
    assert_eq!(h.kv(&format!("run_node:{}", run.id)).await, None);
}

/// Aether refusing a node's key moves the node to the next generation
/// once; a second refusal fails the stage.
#[tokio::test]
async fn a_refused_node_key_is_left_for_the_next_generation() {
    let h = Harness::new().await;
    h.backend.fail_next_node(ArchonError::NotFound(
        "aether: agent agt_x was created under this idempotency_key and is no longer in the store"
            .into(),
    ));
    let item = h.enqueue("stale key").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let keys: Vec<String> = h
        .backend
        .node_requests()
        .into_iter()
        .map(|r| r.idempotency_key)
        .collect();
    assert_eq!(
        keys,
        [format!("{}:node:1", run.id), format!("{}:node:2", run.id)]
    );
    assert_eq!(h.running().await.len(), 1);
    assert_eq!(
        h.backend.requests()[0].parent,
        Some(h.backend.node_ids()[0].clone())
    );

    let h = Harness::new().await;
    for _ in 0..2 {
        h.backend.fail_next_node(ArchonError::Invalid(
            "aether: a different request needs a new key".into(),
        ));
    }
    let item = h.enqueue("refused twice").await;
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("different request"), "{error}");
    assert!(
        h.backend.requests().is_empty(),
        "no stage agent without a node"
    );
    assert_eq!(h.backend.node_requests().len(), 2);
}

/// Settles and dispatch passes race on a multi-threaded runtime while a
/// watcher checks that live stage runs never exceed the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caps_hold_under_concurrent_settles() {
    const CAP: usize = 2;
    let mut config = Config::default();
    config.limits.max_parallel_agents = CAP as u32;
    let h = Harness::with_config(config).await;
    let mut items = Vec::new();
    for n in 0..4 {
        items.push(h.enqueue(&format!("item {n}")).await.id);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = tokio::spawn(watch_cap(h.ctx.clone(), stop.clone(), CAP));
    h.dispatch().await;
    for _ in 0..60 {
        let running = h.running().await;
        if running.is_empty() {
            break;
        }
        let mut tasks = Vec::new();
        for stage in running {
            let ctx = h.ctx.clone();
            tasks.push(tokio::spawn(async move {
                let agent = stage.aether_agent.clone().expect("agent");
                executor::stage_settled(&ctx, &agent, passing(&stage)).await?;
                dispatcher::dispatch_once(&ctx).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("no panic").expect("settle and dispatch");
        }
        h.dispatch().await;
    }
    stop.store(true, Ordering::SeqCst);
    let peak = watcher.await.expect("watcher");
    assert_eq!(peak, CAP, "the cap was used, never exceeded");
    for id in &items {
        assert!(
            matches!(h.state(id).await, QueueState::Succeeded { .. }),
            "{id} finished"
        );
    }
    // 4 items × (implement + 2 reviewers + ship), each spawned exactly once.
    assert_eq!(h.backend.created(), 16);
    assert_eq!(h.backend.requests().len(), 16);
}

/// The highest number of live stage runs seen until `stop`.
async fn watch_cap(
    ctx: Arc<archon_daemon::context::DaemonContext>,
    stop: Arc<AtomicBool>,
    cap: usize,
) -> usize {
    let mut peak = 0;
    while !stop.load(Ordering::SeqCst) {
        let live = ctx
            .store
            .lock()
            .await
            .live_stage_runs()
            .expect("live")
            .len();
        assert!(live <= cap, "{live} live stage runs over the cap of {cap}");
        peak = peak.max(live);
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    peak
}

#[tokio::test]
async fn global_pause_holds_starts_and_spawns_until_resume() {
    let h = Harness::new().await;
    let global = PauseTarget::Global { hard: false };
    h.cli(Request::Pause(global.clone())).await.expect("pause");
    let item = h.enqueue("paused work").await;
    h.dispatch().await;
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert!(h.backend.requests().is_empty());

    h.cli(Request::Resume(global.clone()))
        .await
        .expect("resume");
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);

    h.cli(Request::Pause(global.clone()))
        .await
        .expect("pause again");
    h.settle_running(passing).await;
    h.dispatch().await;
    assert!(
        h.live_stages().await.is_empty(),
        "no reviewer spawns while paused"
    );
    assert_eq!(h.backend.requests().len(), 1);

    h.cli(Request::Resume(global)).await.expect("resume again");
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "both reviewers start");
}

#[tokio::test]
async fn pipeline_pause_holds_new_items_but_not_running_runs() {
    let h = Harness::new().await;
    let first = h.enqueue("first").await;
    h.dispatch().await;
    let target = PauseTarget::Pipeline {
        name: common::PIPELINE.into(),
    };
    h.cli(Request::Pause(target.clone())).await.expect("pause");
    let second = h.enqueue("second").await;
    h.settle_running(passing).await;
    h.dispatch().await;
    assert_eq!(h.state(&second.id).await, QueueState::Queued);
    let reviewers = h.running().await;
    let first_run = h.run(&first.id).await.id;
    assert_eq!(reviewers.len(), 2);
    assert!(reviewers.iter().all(|s| s.run == first_run));

    h.cli(Request::Resume(target)).await.expect("resume");
    h.dispatch().await;
    assert!(matches!(
        h.state(&second.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn cancel_stops_agents_and_ignores_late_events() {
    let h = Harness::new().await;
    let item = h.enqueue("to cancel").await;
    h.dispatch().await;
    h.settle_running(passing).await;
    h.dispatch().await;
    let reviewers = h.running().await;
    assert_eq!(reviewers.len(), 2);

    let reply = h
        .cli(Request::CancelItem {
            item: item.id.clone(),
            reason: Some("not needed".into()),
        })
        .await
        .expect("cancel");
    assert!(matches!(reply, archon_core::protocol::Reply::Item(_)));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let cancelled = h.backend.cancelled();
    for reviewer in &reviewers {
        let agent = reviewer.aether_agent.as_ref().expect("agent");
        assert!(cancelled.contains(agent), "{agent} was stopped");
    }
    let node = &h.backend.node_ids()[0];
    assert_eq!(
        cancelled.last(),
        Some(node),
        "the node goes after its stages"
    );
    assert!(h.live_stages().await.is_empty());

    // A reviewer's completion racing the cancel changes nothing.
    let late = reviewers[0].aether_agent.clone().expect("agent");
    executor::stage_settled(&h.ctx, &late, verdict(true))
        .await
        .expect("late settle");
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert_eq!(h.backend.requests().len(), 3, "nothing new was spawned");
    assert!(
        h.run(&item.id).await.workspace.exists(),
        "a cancelled run keeps its worktree"
    );
}

#[tokio::test]
async fn model_denied_after_enqueue_is_refused_at_spawn() {
    let h = Harness::new().await;
    let item = h.enqueue("policy changes mid-run").await;
    h.dispatch().await;
    h.repo_config(&format!("[models]\ndeny = [\"{REVIEW_MODEL}\"]\n"));
    h.settle_running(passing).await;
    h.dispatch().await;

    let requests = h.backend.requests();
    assert!(
        requests
            .iter()
            .all(|r| r.choice.model.to_string() != REVIEW_MODEL),
        "a denied model never reaches the backend: {requests:?}"
    );
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the run fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("refused at spawn"), "{error}");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert!(
        blockers.iter().any(|b| b.contains("review")),
        "{blockers:?}"
    );
    assert!(
        h.live_stages().await.is_empty(),
        "the allowed reviewer was stopped too"
    );
}

#[tokio::test]
async fn infra_failure_respawns_the_slot_once() {
    let h = Harness::new().await;
    let item = h.enqueue("flaky engine").await;
    h.dispatch().await;
    let infra = || StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    };
    let first = h.settle_running(|_| infra()).await;
    h.dispatch().await;
    let retry = h.running().await;
    assert_eq!(retry.len(), 1, "one replacement");
    assert_eq!(
        (retry[0].stage_id.as_str(), retry[0].attempt),
        ("implement", 1)
    );
    assert_ne!(retry[0].id, first[0].id);
    let requests = h.backend.requests();
    let keys: Vec<&str> = requests
        .iter()
        .map(|r| r.idempotency_key.as_str())
        .collect();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1], "the replacement is a new agent");
    assert_eq!(
        requests[1].name,
        format!("#{} implement · flaky engine", short_item(&item.id))
    );
    assert_eq!(
        requests[1].parent, requests[0].parent,
        "same run, same node"
    );

    // `infra_retries = 1`: the second infrastructure failure is final.
    h.settle_running(|_| StageOutcome::Failed {
        error: "Retry exhausted the provider quota.".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_eq!(h.backend.requests().len(), 2);
    let notices = h.notices(NotificationKind::Outcome).await;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("Retry exhausted the provider quota."),
        "{}",
        notices[0]
    );
    assert!(
        !notices[0].contains("engine died"),
        "the replaced failure is not the final cause"
    );
}

#[tokio::test]
async fn spawn_unavailable_retries_under_the_same_key() {
    let h = Harness::new().await;
    h.backend
        .fail_next(ArchonError::Unavailable("aetherd busy".into()));
    let item = h.enqueue("retry create").await;
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1);
    assert_eq!(
        stages[0].state,
        StageState::Spawning,
        "still owed a create_agent"
    );

    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "the re-issued request is identical"
    );
    assert_eq!(h.running().await.len(), 1);
    assert_eq!(h.backend.created(), 1);
}

/// A fleet event of the new agent can be waiting for the spawn's record
/// (`pump::fleet`): issuing the spawn signals it instead of leaving a poll.
#[tokio::test]
async fn an_issued_spawn_signals_whoever_waits_for_its_record() {
    let h = Harness::new().await;
    h.enqueue("signal the fleet").await;
    let recorded = h.ctx.spawn_recorded.notified();
    tokio::pin!(recorded);
    recorded.as_mut().enable();
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "the spawn is recorded");
    let signalled = tokio::time::timeout(Duration::ZERO, recorded).await;
    assert!(signalled.is_ok(), "nothing signalled the record");
}

/// The operator's week-long e2e stages: the agent's deadline is a week
/// after its start, not a day.
#[tokio::test]
async fn a_week_long_timeout_gives_the_agent_a_week_long_deadline() {
    let h = Harness::new().await;
    let path = h.repo.join(".archon/pipelines").join(common::PIPELINE);
    let toml = std::fs::read_to_string(path.join("pipeline.toml")).expect("pipeline");
    let implement = "prompt = \"implement.md\"\ntimeout_minutes = 30\n";
    assert!(toml.contains(implement), "implement is the first stage");
    let week = "prompt = \"implement.md\"\ntimeout_minutes = 10080\n";
    h.add_pipeline(common::PIPELINE, &toml.replacen(implement, week, 1));
    h.enqueue("long e2e").await;
    h.dispatch().await;

    let stage = h.running().await.remove(0);
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1);
    let week = chrono::Duration::days(7);
    assert_eq!(requests[0].deadline, Some(stage.started_at + week));
}

/// An item's `stage_timeouts` set after a first run reach the agent's
/// deadline when `retry_item` starts the next one.
#[tokio::test]
async fn an_item_timeout_set_before_a_retry_is_the_retried_agents_deadline() {
    use archon_core::protocol::ItemPatch;

    let h = Harness::new().await;
    let item = h.enqueue("slow change").await;
    h.dispatch().await;
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .expect("cancel");
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            stage_timeouts: [("implement".to_owned(), 150)].into(),
            ..Default::default()
        },
    })
    .await
    .expect("update");
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    })
    .await
    .expect("retry");
    h.dispatch().await;

    let stage = h.running().await.remove(0);
    let retried = h.backend.requests().pop().expect("the retried spawn");
    let wanted = stage.started_at + chrono::Duration::minutes(150);
    assert_eq!(retried.deadline, Some(wanted));
}

/// Aether's `agent.failed` for `stage`'s agent, stamped `at`.
fn agent_failed(stage: &StageRun, at: Timestamp, error: &str) -> AetherEvent {
    AetherEvent {
        seq: 1,
        at,
        agent: stage.aether_agent.clone(),
        task: None,
        parent: None,
        run: None,
        kind: EventKind::AgentFailed {
            error: error.into(),
        },
    }
}

/// Aether fails an agent at its deadline; that is the stage's timeout, not
/// an infrastructure failure for a fresh agent to redo.
#[tokio::test]
async fn an_agent_failed_at_its_deadline_times_out_without_a_respawn() {
    let h = Harness::new().await;
    let item = h.enqueue("ran out of time").await;
    h.dispatch().await;
    let implement = h.running().await.remove(0);
    let deadline = expire_deadline(&h, &implement).await;
    let error = format!("deadline of {}", deadline.to_rfc3339());
    pump::handle_fleet_event(&h.ctx, &agent_failed(&implement, deadline, &error))
        .await
        .expect("handled");
    h.dispatch().await;

    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "no replacement: {stages:?}");
    assert_eq!(stages[0].state, StageState::TimedOut);
    assert_eq!(h.backend.requests().len(), 1);
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
}

/// An engine crash before the deadline that Archon only hears of after it
/// (a reconnect replays the stream) is still an infrastructure failure.
#[tokio::test]
async fn a_crash_before_the_deadline_heard_after_it_is_still_retried() {
    let h = Harness::new().await;
    let item = h.enqueue("crashed early").await;
    h.dispatch().await;
    let implement = h.running().await.remove(0);
    let deadline = expire_deadline(&h, &implement).await;
    let crashed = deadline - chrono::Duration::minutes(1);
    pump::handle_fleet_event(&h.ctx, &agent_failed(&implement, crashed, "engine died"))
        .await
        .expect("handled");
    h.dispatch().await;

    let stages = h.stages(&item.id).await;
    let failed = stages.iter().find(|s| s.id == implement.id).expect("first");
    let engine_died = StageState::Failed {
        error: "engine died".into(),
    };
    assert_eq!(failed.state, engine_died, "its error is kept");
    let retry = h.running().await;
    assert_eq!(retry.len(), 1, "one replacement: {stages:?}");
    assert_ne!(retry[0].id, implement.id);
    assert_eq!(h.backend.requests().len(), 2);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn a_stage_aether_forgot_settles_at_its_deadline() {
    let h = Harness::new().await;
    let item = h.enqueue("vanished agent").await;
    h.dispatch().await;
    let implement = h.running().await.remove(0);
    expire_deadline(&h, &implement).await;
    h.backend
        .fail_next_cancel(ArchonError::NotFound("aether: no such agent".into()));

    dispatcher::dispatch_once(&h.ctx)
        .await
        .expect("an agent Aether forgot is not an error");
    let settled = h.stages(&item.id).await.remove(0);
    assert_eq!(settled.state, StageState::TimedOut);
    assert!(h.live_stages().await.is_empty(), "its slot is free again");
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
}

#[tokio::test]
async fn a_failing_stage_does_not_hold_back_dispatch() {
    let h = Harness::new().await;
    let stuck = h.enqueue("cannot be stopped").await;
    h.dispatch().await;
    let implement = h.running().await.remove(0);
    expire_deadline(&h, &implement).await;
    h.backend
        .fail_next_cancel(ArchonError::Protocol("garbled reply".into()));
    h.backend
        .fail_next_outcome(ArchonError::Protocol("garbled reply".into()));
    let next = h.enqueue("unrelated work").await;

    let pass = dispatcher::dispatch_once(&h.ctx).await;
    assert!(matches!(pass, Err(ArchonError::Protocol(_))), "{pass:?}");
    assert!(
        matches!(h.state(&next.id).await, QueueState::Running { .. }),
        "the other item still started"
    );
    assert_eq!(h.running().await.len(), 2);

    h.dispatch().await;
    let timed_out = h.stages(&stuck.id).await.remove(0);
    assert_eq!(
        timed_out.state,
        StageState::TimedOut,
        "the next pass retries"
    );
}

/// A node Aether refuses to create fails the stage like a refused stage
/// agent would; an unanswered node create leaves the stage `spawning`.
#[tokio::test]
async fn run_node_errors_are_the_stages_errors() {
    let h = Harness::new().await;
    h.backend
        .fail_next_node(ArchonError::Unavailable("aetherd busy".into()));
    let item = h.enqueue("node trouble").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    assert!(
        h.backend.requests().is_empty(),
        "no stage agent without its node"
    );

    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);
    assert_eq!(
        h.backend.node_requests().len(),
        2,
        "the same node create, retried"
    );
    assert_eq!(h.backend.nodes_created(), 1);

    let other = h.enqueue("node refused").await;
    h.backend
        .fail_next_node(ArchonError::Aether("scope refused".into()));
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&other.id).await else {
        panic!("the stage fails: {:?}", h.state(&other.id).await);
    };
    assert!(error.contains("scope refused"), "{error}");
}

/// A node Aether lost mid-run is replaced under the next generation's key
/// before the next stage agent is created.
#[tokio::test]
async fn a_lost_run_node_is_replaced_for_the_next_stage() {
    let h = Harness::new().await;
    let item = h.enqueue("node lost").await;
    h.dispatch().await;
    let first = h.backend.node_ids()[0].clone();
    h.backend.kill(&first);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2);
    let run = h.run(&item.id).await;
    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[1].idempotency_key, format!("{}:node:2", run.id));
    let second = h.backend.node_ids()[1].clone();
    let parents: Vec<_> = h.backend.requests()[1..]
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert_eq!(parents, vec![Some(second.clone()), Some(second.clone())]);
    let stored = h.kv(&format!("run_node:{}", run.id)).await.expect("stored");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(stored["agent"], second.as_str());
    assert_eq!(stored["generation"], 2);
    assert_eq!(
        stored["request"]["idempotency_key"],
        format!("{}:node:2", run.id),
        "the request stays with the node: {stored}"
    );
}

/// Aether settles a task mid-turn; the agent completes when the turn
/// ends. Cancelling the node meanwhile would cascade into that agent, so
/// retirement waits for it and the sweep retires the node once it ended.
#[tokio::test]
async fn retirement_waits_for_a_stage_agent_still_ending_its_turn() {
    let h = Harness::new().await;
    let item = h.enqueue("slow ending").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    let agent = ship.aether_agent.clone().expect("agent");
    h.backend.hold_turn(&agent);
    h.settle_running(passing).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let run = h.run(&item.id).await;
    let key = format!("run_node:{}", run.id);
    assert!(h.backend.cancelled().is_empty(), "nothing is cancelled");
    assert!(h.kv(&key).await.is_some(), "the node is remembered");
    h.dispatch().await;
    assert!(h.backend.cancelled().is_empty(), "still ending its turn");

    h.backend.end_turn(&agent);
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    assert_eq!(h.kv(&key).await, None, "retired and forgotten");
}

/// Past the grace, cancellation ends a stuck turn. Retirement then waits
/// for confirmation that the node and its builders stopped.
#[tokio::test]
async fn retirement_cancels_after_the_grace_and_confirms_the_stop() {
    let h = Harness::new().await;
    let item = h.enqueue("stuck ending").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    let agent = ship.aether_agent.clone().expect("agent");
    h.backend.hold_turn(&agent);
    h.settle_running(passing).await;
    h.dispatch().await;
    assert!(h.backend.cancelled().is_empty(), "within the grace");

    let run = h.run(&item.id).await;
    common::age_settlements(&h, &run.id, 2).await;
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    assert_eq!(h.kv(&format!("run_node:{}", run.id)).await, None);
}

/// A long pipeline: `implement`, fourteen more work stages, `review`
/// fanned out sixteen ways, and `ship`: 32 agents over 17 stages.
const LONG_PIPELINE: &str = "long-run";

fn long_pipeline_toml() -> String {
    let mut toml = String::from(
        "[pipeline]\nname = \"long-run\"\ndescription = \"many stages\"\nversion = 1\n\
         workspace = \"worktree\"\nrequires_repo = true\n",
    );
    let work =
        std::iter::once("implement".to_owned()).chain((2..=15).map(|n| format!("step-{n:02}")));
    for id in work {
        toml.push_str(&format!(
            "\n[[stage]]\nid = \"{id}\"\nkind = \"work\"\nprompt = \"implement.md\"\n\
             timeout_minutes = 30\n"
        ));
    }
    toml.push_str(
        "\n[[stage]]\nid = \"review\"\nkind = \"test\"\nprompt = \"review.md\"\nfan_out = 16\n\
         gate = \"all_pass\"\ntimeout_minutes = 30\n\
         \n[[stage]]\nid = \"ship\"\nkind = \"ship\"\nprompt = \"ship.md\"\ntimeout_minutes = 90\n",
    );
    toml
}

/// One choice per work stage and sixteen reviewers for the long pipeline.
fn long_plan() -> archon_core::model::ModelPlan {
    let mut stages = std::collections::BTreeMap::new();
    stages.insert("implement".to_owned(), vec![choice(MODEL, Effort::Medium)]);
    for n in 2..=15 {
        stages.insert(format!("step-{n:02}"), vec![choice(MODEL, Effort::Low)]);
    }
    stages.insert(
        "review".to_owned(),
        vec![choice(REVIEW_MODEL, Effort::Medium); 16],
    );
    stages.insert("ship".to_owned(), vec![choice(MODEL, Effort::Low)]);
    archon_core::model::ModelPlan(stages)
}

/// Retirement asks Aether only about the agents that settled recently,
/// newest first: with the ship agent still ending its turn, one
/// `get_agent` per sweep, not one per historical agent; and once it ended,
/// all builders are checked once before discarding their build output.
#[tokio::test]
async fn retirement_checks_history_only_after_the_last_turn_ends() {
    let h = Harness::new().await;
    h.add_pipeline(LONG_PIPELINE, &long_pipeline_toml());
    let item = h
        .enqueue_in(LONG_PIPELINE, "long haul", long_plan())
        .await
        .expect("enqueue");
    h.dispatch().await;
    let mut settled = 0;
    loop {
        let running = h.running().await;
        if running.len() == 1 && running[0].stage_id == "ship" {
            break;
        }
        assert!(
            !running.is_empty(),
            "the run stalled after {settled} agents"
        );
        settled += h.settle_running(passing).await.len();
        h.dispatch().await;
    }
    assert_eq!(settled, 15 + 16, "every earlier agent settled");
    let ship = h.running().await.remove(0);
    let agent = ship.aether_agent.clone().expect("agent");
    h.backend.hold_turn(&agent);
    h.backend.take_live_checks();
    h.settle_running(passing).await;
    let run = h.run(&item.id).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.backend.take_live_checks(), vec![agent.clone()]);
    h.dispatch().await;
    assert_eq!(h.backend.take_live_checks(), vec![agent.clone()]);
    assert!(h.backend.cancelled().is_empty(), "still ending its turn");

    common::age_settlements_except(&h, &run.id, 2, &["ship"]).await;
    h.backend.end_turn(&agent);
    h.dispatch().await;
    let checked = h.backend.take_live_checks();
    assert_eq!(checked.first(), Some(&agent));
    for stage in h.ctx.store.lock().await.stage_runs(&run.id).unwrap() {
        assert!(
            checked.contains(&stage.aether_agent.unwrap()),
            "every builder confirmed stopped"
        );
    }
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    assert_eq!(h.kv(&format!("run_node:{}", run.id)).await, None);
    assert_eq!(h.backend.requests().len(), 32);
}

/// Aether's `[network] default` changes while a run is under way: the node
/// recorded `denied`, so every stage agent bound to it states `denied`,
/// which the fake would otherwise refuse as widening its parent. The
/// network is persisted with the request before it is sent.
#[tokio::test]
async fn stage_agents_state_the_network_their_node_recorded() {
    let h = Harness::new().await;
    h.backend.set_network_default(NetworkPolicy::Denied);
    let item = h.enqueue("offline first").await;
    h.dispatch().await;
    assert_eq!(h.backend.node_requests()[0].network, NetworkPolicy::Denied);
    let implement = &h.backend.requests()[0];
    assert_eq!(implement.network, Some(NetworkPolicy::Denied));
    let stored = h
        .kv(&format!("spawn_request:{}", implement.idempotency_key))
        .await
        .expect("stored spawn");
    assert!(stored.contains("\"network\":\"denied\""), "{stored}");

    h.backend.set_network_default(NetworkPolicy::Allowed);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "{:?}", h.stages(&item.id).await);
    let reviewers = &h.backend.requests()[1..];
    assert_eq!(reviewers.len(), 2);
    assert!(
        reviewers
            .iter()
            .all(|r| r.network == Some(NetworkPolicy::Denied)),
        "reviewers state the node's network: {reviewers:?}"
    );
}

/// The operator tightens Aether's default to `denied` mid-run: stage agents
/// bound afterwards state `denied` under the same `allowed` node, so none
/// is wider than the current default, and the stored request says so.
#[tokio::test]
async fn a_default_tightened_mid_run_binds_later_stages_to_it() {
    let h = Harness::new().await;
    let item = h.enqueue("online first").await;
    h.dispatch().await;
    assert_eq!(h.backend.node_requests()[0].network, NetworkPolicy::Allowed);
    assert_eq!(
        h.backend.requests()[0].network,
        Some(NetworkPolicy::Allowed)
    );

    h.backend.set_network_default(NetworkPolicy::Denied);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "{:?}", h.stages(&item.id).await);
    let node = h.backend.node_ids()[0].clone();
    let reviewers = &h.backend.requests()[1..];
    assert_eq!(reviewers.len(), 2);
    for reviewer in reviewers {
        assert_eq!(reviewer.network, Some(NetworkPolicy::Denied));
        assert_eq!(reviewer.parent.as_ref(), Some(&node), "narrowing the node");
        let key = format!("spawn_request:{}", reviewer.idempotency_key);
        let stored = h.kv(&key).await.expect("stored spawn");
        assert!(stored.contains("\"network\":\"denied\""), "{stored}");
    }
    assert_eq!(h.backend.nodes_created(), 1);
}

/// A stage bound while Aether allowed the network lost its reply; the
/// default is tightened before the retry. The retry repeats the stored
/// frame, as Aether fingerprinted it under the key, and finds the agent
/// Aether already created rather than being refused as a different one.
#[tokio::test]
async fn a_bound_network_is_replayed_as_stored_after_the_default_changed() {
    let h = Harness::new().await;
    h.backend.lose_next_reply();
    let item = h.enqueue("lost reply").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);

    h.backend.set_network_default(NetworkPolicy::Denied);
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "replayed byte for byte");
    assert_eq!(requests[1].network, Some(NetworkPolicy::Allowed));
    assert_eq!(h.backend.created(), 1, "no second agent");
    assert_eq!(h.running().await.len(), 1);
}

/// The worktree `start_item` adds for `item`.
fn worktree_of(h: &Harness, item: &archon_core::queue::QueueItem) -> std::path::PathBuf {
    h.ctx.paths.worktrees.join(REPO_NAME).join(item.id.as_str())
}

/// The worktree is gone and git no longer lists it, but the start did
/// add it: its branch, which removal keeps, exists.
fn assert_worktree_removed(h: &Harness, tree: &std::path::Path) {
    assert!(!tree.exists(), "{} is left behind", tree.display());
    let listed = git(&h.repo, &["worktree", "list", "--porcelain"]);
    let name = tree.file_name().unwrap().to_str().unwrap();
    assert!(!listed.contains(name), "git still lists it: {listed}");
    let branches = git(&h.repo, &["branch", "--list", "archon/*"]);
    assert!(!branches.is_empty(), "the start never added a worktree");
}

/// Edit the test pipeline after enqueue: its new `docs` stage is in no
/// item's plan, which only the run's driver refuses.
fn add_a_stage_the_plan_misses(h: &Harness) {
    let path = h.repo.join(".archon/pipelines").join(common::PIPELINE);
    let toml = std::fs::read_to_string(path.join("pipeline.toml")).expect("pipeline");
    let ship = "[[stage]]\nid = \"ship\"";
    assert!(toml.contains(ship), "ship is the last stage");
    let docs = "[[stage]]\nid = \"docs\"\nkind = \"work\"\nprompt = \"implement.md\"\ntimeout_minutes = 30\n\n";
    h.add_pipeline(
        common::PIPELINE,
        &toml.replace(ship, &format!("{docs}{ship}")),
    );
}

/// A pipeline edited after enqueue so the item's plan misses a stage
/// is refused by the driver, after the worktree was added.
#[tokio::test]
async fn a_start_refused_after_its_worktree_was_added_removes_it() {
    let h = Harness::new().await;
    let item = h.enqueue("drifted plan").await;
    add_a_stage_the_plan_misses(&h);
    h.dispatch().await;
    let QueueState::Failed { run: None, error } = h.state(&item.id).await else {
        panic!(
            "expected an unstarted failure: {:?}",
            h.state(&item.id).await
        );
    };
    assert!(error.contains("docs"), "{error}");
    assert_worktree_removed(&h, &worktree_of(&h, &item));
    let notes = h.notices(NotificationKind::Blocker).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
}

/// An item cancelled while its start is in git work is left alone at
/// launch, and so is none of that work.
#[tokio::test]
async fn an_item_cancelled_while_starting_leaves_no_worktree() {
    let h = Harness::new().await;
    let item = h.enqueue("cancelled mid-start").await;
    let table = h.ctx.runs.lock().await;
    let (ctx, id) = (h.ctx.clone(), item.id.clone());
    let start = tokio::spawn(async move { executor::start_item(&ctx, &id).await });
    // The start queues behind the run table; the mutex is fair, so this
    // lock is next after its claim and before its launch.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(table);
    let table = h.ctx.runs.lock().await;
    assert_eq!(h.state(&item.id).await, QueueState::Dispatching, "claimed");
    let cancelled = QueueState::Cancelled {
        reason: Some("operator".into()),
    };
    h.ctx
        .store
        .lock()
        .await
        .transition_item(&item.id, cancelled, archon_core::now())
        .expect("cancel");
    drop(table);
    start.await.expect("start task").expect("start");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    assert!(runs.is_empty(), "{runs:?}");
    assert_worktree_removed(&h, &worktree_of(&h, &item));
}

/// A start that reused an earlier run's worktree leaves it in place
/// when it gives up.
#[tokio::test]
async fn an_abandoned_start_keeps_a_worktree_it_reused() {
    let h = Harness::new().await;
    let item = h.enqueue("reused tree").await;
    let tree = worktree_of(&h, &item);
    let manager = archon_daemon::worktree::WorktreeManager::new(
        h.ctx.paths.worktrees.clone(),
        h.ctx.config.worktrees.branch_prefix.clone(),
    );
    let earlier = manager
        .create(
            &h.repo,
            REPO_NAME,
            &item.id,
            &manager.branch_name(&item.id, &item.title, 0),
            Some("main"),
        )
        .expect("an earlier run's worktree");
    assert!(earlier.added && earlier.path == tree);
    add_a_stage_the_plan_misses(&h);
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("expected a failed start");
    };
    assert!(
        error.contains("docs"),
        "refused after the worktree: {error}"
    );
    assert!(tree.join("README.md").exists(), "the reused worktree stays");
}

#[tokio::test]
async fn pr_shipping_without_a_readable_frozen_spec_fails_and_keeps_its_workspace() {
    let h = Harness::new().await;
    let item = h.enqueue("ship without frozen spec").await;
    h.dispatch().await;
    drive_to_ship(&h).await;
    let run = h.run(&item.id).await;
    std::fs::remove_dir_all(
        h.ctx
            .paths
            .data_dir
            .join("pipeline-runs")
            .join(run.id.as_str()),
    )
    .unwrap();
    h.settle_running(passing).await;
    let finished = h.run(&item.id).await;
    assert!(matches!(finished.state, RunState::Failed { .. }));
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(!finished.shipped, "missing spec cannot prove shipping");
    assert!(
        run.workspace.exists(),
        "no shipping proof means normal retention"
    );
}

#[tokio::test]
async fn failure_notice_is_concise_and_stage_evidence_keeps_the_full_error() {
    let h = Harness::new().await;
    let item = h.enqueue("add a greeting").await;
    h.dispatch().await;
    let full = "Authentication failed. Check the configured API key.\n\nNested detail: agent slot diagnostics";
    h.settle_running(|_| StageOutcome::Failed {
        error: full.into(),
        infra: false,
    })
    .await;
    let notices = h.notices(NotificationKind::Outcome).await;
    assert_eq!(notices.len(), 1);
    let notice = &notices[0];
    assert!(
        notice.contains("Failed: add a greeting — implement: Authentication failed"),
        "{notice}"
    );
    assert!(
        notice.contains(&format!("archon queue show {} --full", item.id)),
        "{notice}"
    );
    assert!(
        !notice.contains("Nested detail") && !notice.contains("**") && !notice.contains("stage `")
    );
    let stages = h.stages(&item.id).await;
    assert!(matches!(&stages[0].state, StageState::Failed { error } if error == full));
    assert!(
        matches!(h.state(&item.id).await, QueueState::Failed { error, .. } if error.contains(full))
    );
}

#[tokio::test]
async fn failure_notices_handle_empty_errors_and_bound_unicode_causes() {
    for (error, expected) in [
        (String::new(), "No failure detail was recorded."),
        ("é".repeat(1000), "…"),
    ] {
        let h = Harness::new().await;
        let item = h.enqueue("bounded failure").await;
        h.dispatch().await;
        h.settle_running(|_| StageOutcome::Failed {
            error: error.clone(),
            infra: false,
        })
        .await;
        let notices = h.notices(NotificationKind::Outcome).await;
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(expected), "{}", notices[0]);
        assert!(notices[0].chars().count() < 400, "{}", notices[0]);
        assert!(notices[0].ends_with(&format!("archon queue show {} --full", item.id)));
    }
}

fn short_item(item: &archon_core::id::QueueItemId) -> String {
    item.as_str()[item.as_str().len() - 8..].to_ascii_lowercase()
}

#[tokio::test]
async fn operator_failures_publish_one_blocker_and_keep_the_finished_trigger() {
    use archon_daemon::triggers::{self, TriggerEvent};

    let h = Harness::new().await;
    let item = h.enqueue("operator action needed").await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "BLOCKED: the provider quota is exhausted. Raise the quota.".into(),
        infra: false,
    })
    .await;
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1);
    assert!(blockers[0].contains("archon queue show"));
    assert!(h.notices(NotificationKind::Outcome).await.is_empty());
    let held = triggers::held(&h.ctx).await.unwrap();
    assert!(held.iter().any(|event| matches!(event,
        TriggerEvent::ItemFinished { item: id, state, summary, .. }
            if id == &item.id && state == "failed" && summary.contains("Raise the quota."))));
}

#[tokio::test]
async fn subcent_item_overshoot_is_visible_once_when_the_next_stage_is_refused() {
    use archon_core::protocol::ItemPatch;
    use archon_queue::store::SpendRow;

    let h = Harness::new().await;
    let item = h.enqueue("tiny item budget").await;
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            budget_usd: Some(0.000001),
            ..ItemPatch::default()
        },
    })
    .await
    .unwrap();
    h.dispatch().await;
    let agent = h.running().await[0].aether_agent.clone().unwrap();
    h.ctx
        .store
        .lock()
        .await
        .record_spend(&SpendRow {
            agent,
            item: Some(item.id.clone()),
            cost_usd: 0.000002,
            estimated_cost_usd: 0.0,
            cost_basis: archon_core::cost::CostBasis::ProviderReported,
            cost_complete: None,
            input_tokens: 1,
            output_tokens: 1,
            at: archon_core::now(),
        })
        .unwrap();
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the next stage must be refused");
    };
    assert!(error.contains("(<$0.01 over)"), "{error}");
    let notices = h.notices(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("(<$0.01 over)"));
    assert!(h.notices(NotificationKind::Outcome).await.is_empty());
}

#[tokio::test]
async fn subcent_daily_cap_and_spend_stay_visible_in_the_hold() {
    use archon_queue::store::SpendRow;

    let mut config = Config::default();
    config.limits.budget_usd_daily = 0.001;
    let h = Harness::with_config(config).await;
    let item = h.enqueue("spend a little").await;
    h.dispatch().await;
    let agent = h.running().await[0].aether_agent.clone().unwrap();
    h.ctx
        .store
        .lock()
        .await
        .record_spend(&SpendRow {
            agent,
            item: Some(item.id),
            cost_usd: 0.002,
            estimated_cost_usd: 0.0,
            cost_basis: archon_core::cost::CostBasis::ProviderReported,
            cost_complete: None,
            input_tokens: 1,
            output_tokens: 1,
            at: archon_core::now(),
        })
        .unwrap();
    h.enqueue("held by daily cap").await;
    h.dispatch().await;
    let notices = h.notices(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("daily budget of <$0.01 is reached (<$0.01 spent)"),
        "{notices:?}"
    );
    h.dispatch().await;
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
}

#[tokio::test]
async fn pr_shipping_records_githubs_url_even_when_the_agent_guesses_another() {
    for reported in [
        Some("https://github.com/o/r/pull/1"),
        Some("https://github.com/o/r/pull/999"),
        None,
    ] {
        let h = Harness::new().await;
        let item = h.enqueue("verify shipping").await;
        drive_to_ship(&h).await;
        h.settle_running(|_| StageOutcome::Completed {
            summary: "Opened https://github.com/o/r/pull/999".into(),
            structured: reported.map(|url| serde_json::json!({"pr_url": url, "commit": "kept"})),
        })
        .await;
        assert!(matches!(
            h.state(&item.id).await,
            QueueState::Succeeded { .. }
        ));
        let stages = h.stages(&item.id).await;
        assert_eq!(
            archon_core::run::pr_url(stages.iter()),
            Some("https://github.com/o/r/pull/1")
        );
        if reported.is_some() {
            assert_eq!(
                stages.last().unwrap().structured.as_ref().unwrap()["commit"],
                "kept"
            );
        }
        let held = archon_daemon::triggers::held(&h.ctx).await.unwrap();
        assert!(held.iter().any(|event| matches!(event,
            archon_daemon::triggers::TriggerEvent::ItemFinished { item: id, pr_url: Some(url), .. }
                if id == &item.id && url == "https://github.com/o/r/pull/1")));
    }
}

/// A lookup held until the test releases it, with a bound if the test fails;
/// other calls, such as the risk label's, pass straight through.
struct SlowGh {
    binary: std::path::PathBuf,
}

impl SlowGh {
    fn install(h: &Harness) -> Self {
        let binary = h.ctx.config.github.binary.clone();
        let response = std::fs::read_to_string(&binary).unwrap();
        let barrier = r#"#!/bin/sh
if [ "$1 $2" = "pr list" ]; then
    printf 'lookup\n' >> "$0.started"
    attempt=0
    while [ ! -e "$0.release" ]; do
        attempt=$((attempt + 1))
        [ "$attempt" -lt 1000 ] || exit 4
        sleep 0.01
    done
    if [ -e "$0.missing" ]; then
        printf '[]'
        exit 0
    fi
fi
"#;
        std::fs::write(&binary, format!("{barrier}{response}")).unwrap();
        Self { binary }
    }

    fn marker(&self, suffix: &str) -> std::path::PathBuf {
        self.binary.with_file_name(format!(
            "{}.{suffix}",
            self.binary.file_name().unwrap().to_str().unwrap()
        ))
    }

    async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.marker("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fake gh started its lookup");
    }

    fn release(&self) {
        std::fs::write(self.marker("release"), "").unwrap();
    }
}

fn settle_ship(h: &Harness, ship: &StageRun) -> tokio::task::JoinHandle<Result<(), ArchonError>> {
    let ctx = h.ctx.clone();
    let agent = ship.aether_agent.clone().unwrap();
    let outcome = passing(ship);
    tokio::spawn(async move { executor::stage_settled(&ctx, &agent, outcome).await })
}

#[tokio::test]
async fn pending_pr_verification_allows_unrelated_dispatch_and_cancellation() {
    let h = Harness::new().await;
    let item = h.enqueue("slow shipping").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    let gh = SlowGh::install(&h);
    let pending = settle_ship(&h, &ship);
    gh.wait_started().await;

    let other = h.enqueue("unrelated work").await;
    let progressed = tokio::time::timeout(Duration::from_secs(2), async {
        h.dispatch().await;
        assert!(matches!(
            h.state(&other.id).await,
            QueueState::Running { .. }
        ));
        executor::cancel_item(&h.ctx, &other.id, "withdraw unrelated work")
            .await
            .unwrap();
    })
    .await;
    let still_pending = !pending.is_finished();
    gh.release();
    pending.await.unwrap().unwrap();

    progressed.expect("unrelated work proceeds while gh is held");
    assert!(
        still_pending,
        "the lookup was pending during unrelated work"
    );
    assert!(matches!(
        h.state(&other.id).await,
        QueueState::Cancelled { .. }
    ));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn cancellation_wins_pending_pr_verification_even_if_the_agent_stop_is_unanswered() {
    for stop_unanswered in [false, true] {
        let h = Harness::new().await;
        let item = h.enqueue("cancel slow shipping").await;
        drive_to_ship(&h).await;
        let ship = h.running().await.remove(0);
        let gh = SlowGh::install(&h);
        let pending = settle_ship(&h, &ship);
        gh.wait_started().await;
        if stop_unanswered {
            h.backend
                .fail_next_cancel(ArchonError::Unavailable("stop unanswered".into()));
        }
        let cancelled = tokio::time::timeout(
            Duration::from_secs(2),
            executor::cancel_item(&h.ctx, &item.id, "withdraw shipping"),
        )
        .await;
        gh.release();
        pending.await.unwrap().unwrap();
        cancelled
            .expect("cancel proceeds while gh is held")
            .unwrap();

        assert_eq!(
            h.run(&item.id).await.state,
            RunState::Cancelled {
                reason: Some("withdraw shipping".into()),
            }
        );
        let stages = h.stages(&item.id).await;
        assert_eq!(
            stages.last().unwrap().state,
            if stop_unanswered {
                StageState::Running
            } else {
                StageState::Cancelled
            }
        );
        assert_eq!(archon_core::run::pr_url(stages.iter()), None);
        assert!(!h.run(&item.id).await.shipped);
        h.dispatch().await;
        assert_eq!(
            h.stages(&item.id).await.last().unwrap().state,
            StageState::Cancelled
        );
        let held = archon_daemon::triggers::held(&h.ctx).await.unwrap();
        let finished: Vec<_> = held.iter().filter(|event| matches!(event,
            archon_daemon::triggers::TriggerEvent::ItemFinished { item: id, .. } if id == &item.id
        )).collect();
        assert_eq!(finished.len(), 1, "cancellation is announced once");
        assert!(matches!(
            finished[0],
            archon_daemon::triggers::TriggerEvent::ItemFinished { pr_url: None, .. }
        ));
    }
}

#[tokio::test]
async fn a_competing_settlement_wins_pending_pr_verification() {
    let h = Harness::new().await;
    let item = h.enqueue("ship fails during lookup").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    let gh = SlowGh::install(&h);
    let pending = settle_ship(&h, &ship);
    gh.wait_started().await;
    let failed = tokio::time::timeout(
        Duration::from_secs(2),
        executor::stage_settled(
            &h.ctx,
            ship.aether_agent.as_ref().unwrap(),
            StageOutcome::Failed {
                error: "shipping withdrawn".into(),
                infra: false,
            },
        ),
    )
    .await;
    gh.release();
    pending.await.unwrap().unwrap();
    failed
        .expect("another settle proceeds while gh is held")
        .unwrap();
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_eq!(
        h.stages(&item.id).await.last().unwrap().state,
        StageState::Failed {
            error: "shipping withdrawn".into(),
        }
    );
    assert_eq!(
        archon_core::run::pr_url(h.stages(&item.id).await.iter()),
        None
    );
}

#[tokio::test]
async fn restart_during_pr_verification_reconciles_and_verifies_again() {
    use archon_aether::AetherClient;
    use archon_aether::fake::{self, FakeAether, Scripted};

    for missing_on_recovery in [false, true] {
        let mut h = Harness::new().await;
        let item = h.enqueue("restart slow shipping").await;
        drive_to_ship(&h).await;
        let ship = h.running().await.remove(0);
        let gh = SlowGh::install(&h);
        let pending = settle_ship(&h, &ship);
        gh.wait_started().await;
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(
            h.stages(&item.id).await.last().unwrap().state,
            StageState::Running
        );
        h.restart();
        executor::recover(&h.ctx).await.unwrap();
        let recovered = h.running().await;
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].id, ship.id,
            "recovery keeps the original stage"
        );

        let aether = FakeAether::start(&h.dir.path().join("recovery.sock"))
            .await
            .unwrap();
        let task = fake::task(
            ship.aether_task.as_ref().unwrap().as_str(),
            Some(ship.aether_agent.as_ref().unwrap().as_str()),
            "run_0001",
            serde_json::json!({"state": "completed", "result": {
                "summary": "opened a PR", "artifacts": [],
                "structured": {"pr_url": "https://github.com/o/r/pull/999"},
            }}),
        );
        aether.set_default("get_task", Scripted::reply("task", task));
        // Reconciliation also checks the orchestrator; this test has none.
        aether.set_default(
            "create_agent",
            Scripted::refuse("unavailable", "no orchestrator"),
        );
        let client = AetherClient::connect(&aether.endpoint()).await.unwrap();
        h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
        if missing_on_recovery {
            std::fs::write(gh.marker("missing"), "").unwrap();
        }
        gh.release();
        pump::reconcile(&h.ctx).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(gh.marker("started"))
                .unwrap()
                .lines()
                .count(),
            2,
            "reconciliation repeats the interrupted lookup"
        );
        let stages = h.stages(&item.id).await;
        if missing_on_recovery {
            assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
            assert_eq!(archon_core::run::pr_url(stages.iter()), None);
        } else {
            assert!(matches!(
                h.state(&item.id).await,
                QueueState::Succeeded { .. }
            ));
            assert_eq!(
                archon_core::run::pr_url(stages.iter()),
                Some("https://github.com/o/r/pull/1")
            );
        }
        assert!(
            stages.last().unwrap().state.is_terminal(),
            "recovery settles the stage"
        );
    }
}

#[tokio::test]
async fn pr_verification_failures_do_not_retry_the_ship_agent_on_a_full_disk() {
    let h = Harness::new().await;
    let item = h.enqueue("GitHub lookup runs out of space").await;
    drive_to_ship(&h).await;
    h.set_free_bytes(0);
    std::fs::write(
        &h.ctx.config.github.binary,
        "#!/bin/sh\necho 'No space left on device' >&2\nexit 1\n",
    )
    .unwrap();
    h.settle_running(passing).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let stages = h.stages(&item.id).await;
    assert_eq!(
        stages.len(),
        4,
        "lookup failure does not respawn the ship agent"
    );
    assert!(matches!(
        stages.last().unwrap().state,
        StageState::Failed { .. }
    ));
    assert_eq!(archon_core::run::pr_url(stages.iter()), None);
}

#[tokio::test]
async fn pr_shipping_fails_for_missing_mismatched_closed_or_unreadable_prs() {
    for response in [
        "printf '[]'",
        "printf '[{},{}]'",
        r#"printf '[{"url":"https://github.com/o/r/pull/1","number":1,"headRefName":"other","state":"OPEN"}]'"#,
        r#"printf '[{"url":"https://github.com/o/r/pull/1","number":1,"headRefName":"%s","state":"CLOSED"}]' "$4""#,
        "printf 'bad json'",
        "echo 'authentication failed' >&2; exit 1",
        r#"printf '[{"url":"https://github.com/o/r/pull/99","number":1,"headRefName":"%s","state":"OPEN"}]' "$4""#,
    ] {
        let h = Harness::new().await;
        std::fs::write(
            &h.ctx.config.github.binary,
            format!("#!/bin/sh\n{response}\n"),
        )
        .unwrap();
        let item = h.enqueue("missing shipping").await;
        drive_to_ship(&h).await;
        let run = h.run(&item.id).await;
        h.settle_running(passing).await;
        let QueueState::Failed { error, .. } = h.state(&item.id).await else {
            panic!("invalid PR evidence must fail: {response}");
        };
        assert!(
            error.contains(&run.branch) && error.contains("Create or reopen"),
            "{error}"
        );
        assert!(matches!(
            h.stages(&item.id).await.last().unwrap().state,
            StageState::Failed { .. }
        ));
        assert!(
            run.workspace.exists(),
            "failed shipping retains the pushed branch's workspace"
        );
        assert_eq!(
            archon_core::run::pr_url(h.stages(&item.id).await.iter()),
            None
        );
        assert!(!h.run(&item.id).await.shipped);
    }
}

#[tokio::test]
async fn local_shipping_does_not_query_github() {
    use archon_core::protocol::{RepoPatch, RepoRef};
    use archon_core::repo::ShipMode;
    let h = Harness::new().await;
    std::fs::write(&h.ctx.config.github.binary, "#!/bin/sh\nexit 99\n").unwrap();
    let set_ship = |mode| Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            ship: Some(mode),
            ..RepoPatch::default()
        },
    };
    h.cli(set_ship(ShipMode::Local)).await.unwrap();
    let item = h.enqueue("unchanged shipping mode").await;
    drive_to_ship(&h).await;
    // The run keeps the mode it started with.
    common::git(
        &h.repo,
        &["remote", "add", "origin", "https://example.invalid/r.git"],
    );
    h.cli(set_ship(ShipMode::Pr)).await.unwrap();
    h.settle_running(|_| completed("committed locally")).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(
        archon_core::run::pr_url(h.stages(&item.id).await.iter()),
        None
    );
}

#[tokio::test]
async fn late_completion_of_a_cancelled_run_strips_unverified_pr_urls() {
    let h = Harness::new().await;
    let item = h.enqueue("late ship completion").await;
    drive_to_ship(&h).await;
    let ship = h.running().await.remove(0);
    h.backend
        .fail_next_cancel(ArchonError::Unavailable("stop unanswered".into()));
    executor::cancel_item(&h.ctx, &item.id, "withdrawn")
        .await
        .unwrap();
    assert_eq!(
        h.ctx.store.lock().await.stage_run(&ship.id).unwrap().state,
        StageState::Running
    );
    executor::stage_settled(
        &h.ctx,
        ship.aether_agent.as_ref().unwrap(),
        StageOutcome::Completed {
            summary: "unverified claim".into(),
            structured: Some(
                serde_json::json!({"pr_url": "https://unverified/pull/1", "other": "kept"}),
            ),
        },
    )
    .await
    .unwrap();
    let settled = h.ctx.store.lock().await.stage_run(&ship.id).unwrap();
    assert_eq!(
        settled.structured.unwrap(),
        serde_json::json!({"other": "kept"})
    );
    assert!(matches!(
        h.run(&item.id).await.state,
        RunState::Cancelled { .. }
    ));
    assert_eq!(
        archon_core::run::pr_url(h.stages(&item.id).await.iter()),
        None
    );
}

/// Aether's scheduling tools: the orchestrator's, hidden from every stage agent.
const SCHEDULE_TOOLS: [&str; 3] = ["create_schedule", "update_schedule", "list_schedules"];

/// What a guarded stage agent does not see, in a workspace it may write.
fn guarded_hidden() -> Vec<String> {
    let kept = archon_core::mcp_tools::STAGE_TOOLS.map(archon_core::mcp_tools::qualified);
    archon_core::mcp_tools::qualified_names()
        .into_iter()
        .filter(|name| !kept.contains(name))
        .chain(SCHEDULE_TOOLS.map(str::to_owned))
        .collect()
}

/// An item on `pipeline` whose plan gives each of `stages` `MODEL`, dispatched.
async fn planned_item(
    h: &Harness,
    pipeline: &str,
    stages: &[&str],
) -> archon_core::queue::QueueItem {
    let planned = |stage: &&str| ((*stage).to_owned(), vec![choice(MODEL, Effort::Medium)]);
    let plan = archon_core::model::ModelPlan(stages.iter().map(planned).collect());
    let item = h.enqueue_in(pipeline, "a led change", plan).await.unwrap();
    assert_eq!(
        item.autonomy.tools,
        archon_core::autonomy::ToolsMode::Guarded
    );
    h.dispatch().await;
    item
}

/// An `adaptive` item, dispatched, in a repository with an `origin` for
/// archond to deliver its one stage's commits to.
async fn adaptive_item(h: &Harness) -> archon_core::queue::QueueItem {
    add_origin(h);
    planned_item(h, "adaptive", &["build"]).await
}

/// A lead that delegates and holds the review, then an agent that ships:
/// what a pipeline of the user's own may still declare.
const LED_THEN_SHIPPED: &str = r#"[pipeline]
name = "led"
description = "a lead builds and reviews, then an agent ships"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.review]
level = "standard"
stage = "build"

[[stage]]
id = "build"
kind = "work"
prompt = "implement.md"
allow_delegation = true

[[stage]]
id = "ship"
kind = "ship"
prompt = "implement.md"
"#;

/// An item on [`LED_THEN_SHIPPED`], dispatched: its lead's stage has a next one.
async fn led_item(h: &Harness) -> archon_core::queue::QueueItem {
    h.add_pipeline("led", LED_THEN_SHIPPED);
    planned_item(h, "led", &["build", "ship"]).await
}

/// A draft, then a lead that holds the review: the lead's contract is built
/// only once the draft settles.
const DRAFTED_THEN_LED: &str = r#"[pipeline]
name = "drafted"
description = "an agent drafts, then a lead builds and reviews"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.review]
level = "standard"
stage = "build"

[[stage]]
id = "draft"
kind = "work"
prompt = "implement.md"

[[stage]]
id = "build"
kind = "work"
prompt = "implement.md"
allow_delegation = true
"#;

async fn seed_review_catalog(h: &Harness) {
    use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
    let models = [MODEL, REVIEW_MODEL]
        .into_iter()
        .map(|model| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(Tier::Balanced),
            is_default: false,
        })
        .collect();
    h.ctx
        .store
        .lock()
        .await
        .set_model_catalog(ModelCatalog::new(models));
}

/// The repository narrows its policy mid-run: the later lead is advised the
/// reviewer models allowed when it spawns, not those allowed at the start.
#[tokio::test]
async fn a_policy_narrowed_mid_run_bounds_the_next_lead_s_reviewers() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    let h = Harness::with_config(config).await;
    seed_review_catalog(&h).await;
    h.add_pipeline("drafted", DRAFTED_THEN_LED);
    planned_item(&h, "drafted", &["draft", "build"]).await;
    let draft = h.backend.requests()[0].clone();
    assert_eq!(draft.labels["archon.stage"], "draft");
    assert!(!draft.instructions.contains("Delegate every reviewer"));

    h.repo_config(&format!("[models]\ndeny = [\"{REVIEW_MODEL}\"]\n"));
    h.settle_running(|_| completed("drafted")).await;
    h.dispatch().await;
    let build = h.backend.requests().last().unwrap().clone();
    assert_eq!(build.labels["archon.stage"], "build");
    let own = format!("on {MODEL} at tier balanced and effort low");
    assert!(build.instructions.contains(&own), "{}", build.instructions);
    let named = build.instructions.matches(REVIEW_MODEL).count();
    assert_eq!(named, 0, "{}", build.instructions);
}

/// The running lead's result: its change committed, the ship record for it,
/// and a pass by one reviewer it delegated to, which passed that commit.
async fn reviewed(h: &Harness, summary: &str) -> StageOutcome {
    reviewed_by_one(h, summary).await.0
}

/// [`reviewed`], with the reviewer.
async fn reviewed_by_one(h: &Harness, summary: &str) -> (StageOutcome, AetherAgentId) {
    let lead = h.running().await.remove(0);
    let reviewer = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let built = reviewed_by(h, &lead, summary, std::slice::from_ref(&reviewer)).await;
    (built, reviewer)
}

/// The lead settles with its change reviewed by one agent.
async fn settle_reviewed(h: &Harness, summary: &str) -> Vec<StageRun> {
    let built = reviewed(h, summary).await;
    h.settle_running(|_| built.clone()).await
}

fn lead_agent(lead: &StageRun) -> AetherAgentId {
    lead.aether_agent.clone().expect("a lead agent")
}

/// Whether archond delivered the item's run: nothing else makes it succeed.
async fn delivered(h: &Harness, item: &archon_core::queue::QueueItem) -> bool {
    matches!(h.state(&item.id).await, QueueState::Succeeded { .. })
}

/// The running lead delegated to an agent on `model`, which ended as `ended`.
async fn delegated_by_the_lead(h: &Harness, model: &str, ended: DescendantState) -> AetherAgentId {
    let lead = lead_agent(&h.running().await[0]);
    let child = h.backend.delegate(&lead, model);
    h.backend.end_delegated(&child, ended);
    child
}

/// The item's failure, once its lead's review was refused.
async fn refused_review(h: &Harness, item: &archon_core::queue::QueueItem) -> String {
    h.dispatch().await;
    let branch = h.run(&item.id).await.branch;
    assert_eq!(pushed(&origin_of(h), &branch), None, "nothing is delivered");
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    error
}

/// The stage flag makes a guarded agent a delegating one: it is told how,
/// within which models, and to put its review on record. No agent ships:
/// archond opens the pull request, with the review it verified in its body.
#[tokio::test]
async fn adaptive_guarded_build_delegates_within_the_stated_policy_and_archond_ships() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    config.models.max_effort = Some(Effort::High);
    let h = Harness::with_config(config).await;
    seed_review_catalog(&h).await;
    let gh = FakeGh::install(&h);
    // Aether may list the review model as ready; this repository denies it.
    h.repo_config(&format!("[models]\ndeny = [\"{REVIEW_MODEL}\"]\n"));
    let item = adaptive_item(&h).await;
    let build = h.backend.requests()[0].clone();
    assert_eq!(build.labels["archon.stage"], "build");
    assert!(!build.leaf);
    assert_eq!(build.hidden_tools, guarded_hidden());
    let rules = &build.instructions[build.instructions.find("## Delegation").unwrap()..];
    let policy = format!("policy for this item: {MODEL}; effort at most high. Archon cancels");
    assert!(rules.contains(&policy), "{rules}");
    assert_eq!(rules.matches(REVIEW_MODEL).count(), 0, "{rules}");
    assert!(rules.contains("pass `workspace_mode: \"isolated\"`"));
    assert!(build.instructions.contains("\n## Review on record\n"));
    assert!(build.instructions.contains("\n## Ship record\n"));
    // The repository leaves the lead's model alone: its reviewers run on it.
    let own = format!("on {MODEL} at tier balanced and effort low");
    assert!(build.instructions.contains(&own), "{}", build.instructions);

    let (built, reviewer) = reviewed_by_one(&h, "Verified; one reviewer passed it").await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build"], "no agent ships");
    assert!(delivered(&h, &item).await, "{:?}", h.state(&item.id).await);
    assert_eq!(gh.count("pr create"), 1);
    let (title, body) = gh.created();
    assert_eq!(title, "Change the thing");
    let review = format!("REVIEW: pass, by {REVIEW_MODEL} (agent {reviewer})");
    assert!(body.contains(&review), "{body}");
}

/// A pass is the lead's word until Aether shows the reviewer: an agent it
/// names that ended without completing reviewed nothing.
#[tokio::test]
async fn adaptive_refuses_a_pass_whose_reviewer_aether_does_not_show() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let cancelled = delegated_by_the_lead(&h, REVIEW_MODEL, DescendantState::Stopped).await;
    let lead = h.running().await.remove(0);
    let named = pass_by(std::slice::from_ref(&cancelled));
    let built = shipped_by(&h, &lead, "Verified; one reviewer passed it", named).await;
    let build = h.settle_running(|_| built.clone()).await;
    let error = refused_review(&h, &item).await;
    let held = format!("it names {cancelled} on {REVIEW_MODEL} ended without completing");
    assert!(error.contains(&held), "{error}");
    let stored = h.ctx.store.lock().await.stage_run(&build[0].id).unwrap();
    let review = &stored.structured.as_ref().unwrap()["review"];
    assert_eq!(review.get("reviewer_agents"), None);
    assert!(review["unverified"].as_str().is_some());
}

/// A completed child is not a reviewer by completing: the builder the lead
/// names returned no verdict, even on the model the 1.0 record named.
#[tokio::test]
async fn a_completed_builder_named_as_the_reviewer_is_no_review() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let builder = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let mut named = pass_by(std::slice::from_ref(&builder));
    named["review"]["reviewer_model"] = REVIEW_MODEL.into();
    let built = shipped_by(&h, &lead, "Built with a helper", named).await;
    let work = serde_json::json!({"summary": "implemented the parser", "files": 3});
    h.backend.complete_delegated(&builder, work);
    h.settle_running(|_| built.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(error.contains(&format!("reviewer {builder}")), "{error}");
    assert!(error.contains("no `verdict`"), "{error}");
}

/// Codex can finish with JSON text that Aether stores only as the summary.
#[tokio::test]
async fn a_luna_summary_json_verdict_still_needs_to_pass_the_current_commit() {
    for verdict in ["pass", "fail", "missing", "stale"] {
        let h = Harness::new().await;
        let item = adaptive_item(&h).await;
        let lead = h.running().await.remove(0);
        let reviewer = h.backend.delegate(&lead_agent(&lead), "codex/gpt-6-luna");
        let named = pass_by(std::slice::from_ref(&reviewer));
        let built = shipped_by(&h, &lead, "Reviewed", named).await;
        let mut data = verdict_on(&built, if verdict == "fail" { "fail" } else { "pass" });
        match verdict {
            "missing" => {
                data.as_object_mut().unwrap().remove("verdict");
            }
            "stale" => {
                data["head"] = git(&h.repo, &["rev-parse", "main"]).into();
            }
            _ => {}
        }
        h.backend.complete_delegated_with(
            &reviewer,
            StageOutcome::Completed {
                summary: data.to_string(),
                structured: None,
            },
        );
        h.settle_running(|_| built.clone()).await;
        if verdict == "pass" {
            h.dispatch().await;
            assert!(delivered(&h, &item).await, "{:?}", h.state(&item.id).await);
        } else {
            let error = refused_review(&h, &item).await;
            let why = match verdict {
                "fail" => "0 of the reviewers named passed",
                "missing" => "no `verdict`",
                "stale" => "read the base branch, not the change",
                _ => unreachable!(),
            };
            assert!(error.contains(why), "{verdict}: {error}");
        }
    }
}

/// A reviewer that failed the change does not pass it, whatever the lead says.
#[tokio::test]
async fn a_reviewer_that_failed_the_change_is_no_pass() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let reviewer = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let mut named = pass_by(std::slice::from_ref(&reviewer));
    named["review"]["reviewer_model"] = REVIEW_MODEL.into();
    let built = shipped_by(&h, &lead, "Reviewed", named).await;
    h.backend
        .complete_delegated(&reviewer, verdict_on(&built, "fail"));
    h.settle_running(|_| built.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(error.contains("0 of the reviewers named passed"), "{error}");
    assert!(error.contains(&format!("{reviewer} failed")), "{error}");
}

/// A pass holds for the commit its reviewer read: a later commit needs a
/// reviewer of its own, who read from the first one's head.
#[tokio::test]
async fn a_commit_after_the_review_needs_its_own_reviewer() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let first = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let read = shipped_by(&h, &lead, "Built", serde_json::json!({})).await;
    h.backend
        .complete_delegated(&first, verdict_on(&read, "pass"));
    let named = pass_by(std::slice::from_ref(&first));
    let later = shipped_by(&h, &lead, "Built, then fixed", named).await;
    h.settle_running(|_| later.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(
        error.contains(&format!("{first} passed")) && error.contains("commits after it"),
        "{error}"
    );
}

/// The re-review: a second reviewer read the fix from the first one's head,
/// so together they read the change as it is delivered.
#[tokio::test]
async fn a_reviewer_of_the_fix_carries_the_first_pass_to_the_head() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let first = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let read = shipped_by(&h, &lead, "Built", serde_json::json!({})).await;
    let first_pass = verdict_on(&read, "pass");
    h.backend.complete_delegated(&first, first_pass.clone());
    let delta = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let both = [first.clone(), delta.clone()];
    let fixed = shipped_by(&h, &lead, "Built, then fixed", pass_by(&both)).await;
    let mut on_fix = verdict_on(&fixed, "pass");
    on_fix["base"] = first_pass["head"].clone();
    h.backend.complete_delegated(&delta, on_fix);
    h.settle_running(|_| fixed.clone()).await;
    h.dispatch().await;
    let QueueState::Succeeded { summary, .. } = h.state(&item.id).await else {
        panic!("delivered: {:?}", h.state(&item.id).await);
    };
    assert!(summary.contains(&format!("(agent {first})")), "{summary}");
    assert!(summary.contains(&format!("(agent {delta})")), "{summary}");
}

/// The lead's change, then a fix whose result names only the reviewer of
/// the fix, who read it from the change's head. The change's head is returned.
async fn fix_named_alone(h: &Harness, lead: &StageRun) -> (StageOutcome, serde_json::Value) {
    let read = shipped_by(h, lead, "Built", serde_json::json!({})).await;
    let first_head = verdict_on(&read, "pass")["head"].clone();
    let delta = h.backend.delegate(&lead_agent(lead), REVIEW_MODEL);
    let named = pass_by(std::slice::from_ref(&delta));
    let fixed = shipped_by(h, lead, "Built, then fixed", named).await;
    let mut on_fix = verdict_on(&fixed, "pass");
    on_fix["base"] = first_head.clone();
    h.backend.complete_delegated(&delta, on_fix);
    (fixed, first_head)
}

/// Earlier reviewers of the lead that cannot anchor: one cancelled after it
/// returned a pass of `head`, one whose head the workspace does not hold.
fn unanchoring_reviewers(h: &Harness, lead: &StageRun, head: &serde_json::Value) {
    let cancelled = h.backend.delegate(&lead_agent(lead), REVIEW_MODEL);
    let whole = serde_json::json!({"verdict": "pass", "head": head, "findings": []});
    h.backend.complete_delegated(&cancelled, whole);
    h.backend
        .end_delegated(&cancelled, DescendantState::Stopped);
    let unheld = h.backend.delegate(&lead_agent(lead), REVIEW_MODEL);
    let elsewhere = serde_json::json!({"verdict": "pass", "head": "deadbeefdeadbeef"});
    h.backend.complete_delegated(&unheld, elsewhere);
}

/// The lead named only its last delta reviewer: Archon anchors it on the
/// whole-change pass the lead delegated earlier, and records that it did.
#[tokio::test]
async fn an_earlier_unnamed_whole_change_pass_anchors_the_named_delta() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let first = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let (fixed, first_head) = fix_named_alone(&h, &lead).await;
    let whole = serde_json::json!({"verdict": "pass", "head": first_head, "findings": []});
    h.backend.complete_delegated(&first, whole);
    unanchoring_reviewers(&h, &lead, &first_head);
    let build = h.settle_running(|_| fixed.clone()).await;
    h.dispatch().await;
    assert!(delivered(&h, &item).await, "{:?}", h.state(&item.id).await);
    let stored = h.ctx.store.lock().await.stage_run(&build[0].id).unwrap();
    let review = &stored.structured.as_ref().unwrap()["review"];
    let implicit = serde_json::json!([first.as_str()]);
    assert_eq!(review["implicit_reviewer_agents"], implicit);
    assert_eq!(review["reviewer_agents"].as_array().unwrap().len(), 1);
}

/// A cancelled earlier review, or one of a commit the branch never held,
/// anchors nothing: the named delta is refused as before.
#[tokio::test]
async fn an_earlier_review_that_cannot_anchor_leaves_the_named_delta_refused() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let (fixed, first_head) = fix_named_alone(&h, &lead).await;
    unanchoring_reviewers(&h, &lead, &first_head);
    h.settle_running(|_| fixed.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(error.contains("read only from"), "{error}");
    assert!(
        error.contains("nor any earlier one of this attempt"),
        "{error}"
    );
}

/// The lead named only its delta reviewer, and earlier reviewers it delegated
/// would anchor it: the stage's settled result, with the earlier pass to come.
async fn anchored_by_an_earlier_pass(h: &Harness) -> (StageRun, StageOutcome) {
    let lead = h.running().await.remove(0);
    let first = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let (fixed, first_head) = fix_named_alone(h, &lead).await;
    let whole = serde_json::json!({"verdict": "pass", "head": first_head, "findings": []});
    h.backend.complete_delegated(&first, whole);
    (lead, fixed)
}

/// A result Aether could not show may be a fail of the head: it keeps the
/// earlier reviewers from anchoring, as it refuses a named one.
#[tokio::test]
async fn an_earlier_reviewer_aether_cannot_show_leaves_the_named_delta_refused() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let (lead, fixed) = anchored_by_an_earlier_pass(&h).await;
    let late = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    h.backend
        .complete_delegated(&late, verdict_on(&fixed, "fail"));
    h.backend
        .unreadable_delegated(&late, "internal: database is locked");
    h.settle_running(|_| fixed.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(error.contains("read only from"), "{error}");
    let unshown = format!("earlier reviewer {late} has a result Aether could not show");
    assert!(error.contains(&unshown), "{error}");
}

/// A reviewer still running may yet fail the head, so nothing anchors on
/// the earlier reviewers until it ends.
#[tokio::test]
async fn a_reviewer_still_running_keeps_earlier_ones_from_anchoring() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let (lead, fixed) = anchored_by_an_earlier_pass(&h).await;
    let running = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    h.settle_running(|_| fixed.clone()).await;
    let error = refused_review(&h, &item).await;
    assert!(error.contains("read only from"), "{error}");
    let held = format!("{running} on {REVIEW_MODEL} still running");
    assert!(error.contains(&held), "{error}");
    assert!(error.contains("or cancel it"), "{error}");
}

/// The reviewer's agent and model are Archon's finding: what the lead wrote
/// there itself is replaced by what Aether shows.
#[tokio::test]
async fn adaptive_records_the_reviewer_it_found_not_the_one_the_lead_names() {
    let h = Harness::new().await;
    adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let reviewer = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let mut forged = pass_by(std::slice::from_ref(&reviewer));
    let claims = serde_json::json!({"reviewer_model": "a/forged", "reviewer_models": ["a/forged"],
        "reviewer_agent": "agt_forged", "reviewer_agents": ["agt_forged"]});
    let review = forged["review"].as_object_mut().unwrap();
    review.extend(claims.as_object().unwrap().clone());
    let built = shipped_by(&h, &lead, "Verified", forged).await;
    h.backend
        .complete_delegated(&reviewer, verdict_on(&built, "pass"));
    let build = h.settle_running(|_| built.clone()).await;
    let stored = h.ctx.store.lock().await.stage_run(&build[0].id).unwrap();
    let review = &stored.structured.as_ref().unwrap()["review"];
    assert_eq!(
        review["reviewer_agents"],
        serde_json::json!([reviewer.as_str()])
    );
    assert_eq!(review["reviewer_models"], serde_json::json!([REVIEW_MODEL]));
    assert_eq!(review.get("reviewer_agent"), None);
    assert_eq!(review.get("reviewer_model"), None);
    assert_eq!(review.get("unverified"), None);
}

/// While Aether cannot be asked, the stage is not settled on a guess: it
/// stays live and the same outcome settles it once Aether answers.
#[tokio::test]
async fn adaptive_leaves_the_build_live_while_aether_cannot_show_the_reviewer() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = lead_agent(&h.running().await[0]);
    let built = reviewed(&h, "Verified").await;
    h.backend
        .fail_next_descendants(ArchonError::Unavailable("aether: disconnected".into()));
    let refused = executor::stage_settled(&h.ctx, &lead, built.clone()).await;
    assert!(
        matches!(refused, Err(ArchonError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(h.running().await.len(), 1, "the build is still live");

    h.settle_running(|_| built.clone()).await;
    assert!(h.running().await.is_empty());
    h.dispatch().await;
    assert!(
        delivered(&h, &item).await,
        "the delivery follows the checked pass"
    );
}

/// A lead that says it reviewed, without the record, ends the item at
/// `build`: an unreviewed change is not delivered, whatever its ship record.
#[tokio::test]
async fn adaptive_never_ships_a_build_that_completed_without_its_review() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0);
    let unreviewed = serde_json::json!({});
    let built = shipped_by(&h, &lead, "Verified; independent review passed", unreviewed).await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "only build ever ran: {requests:?}");
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    assert!(h.live_stages().await.is_empty());
    let branch = h.run(&item.id).await.branch;
    assert_eq!(
        pushed(&origin_of(&h), &branch),
        None,
        "nothing is delivered"
    );
}

const READ_ONLY_DELEGATING: &str = r#"[pipeline]
name = "survey"
description = "reads the repository with helpers"
version = 1
workspace = "repo-readonly"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
allow_delegation = true
"#;

/// A delegating agent of a read-only pipeline cannot integrate a child's
/// work, by its tools and by its contract.
#[tokio::test]
async fn a_read_only_delegating_stage_has_nothing_to_integrate() {
    let h = Harness::new().await;
    h.add_pipeline("survey", READ_ONLY_DELEGATING);
    let plan = archon_core::model::ModelPlan(
        [("implement".to_owned(), vec![choice(MODEL, Effort::Medium)])].into(),
    );
    h.enqueue_in("survey", "survey the code", plan)
        .await
        .unwrap();
    h.dispatch().await;
    let request = h.backend.requests()[0].clone();
    assert!(!request.leaf);
    let mut hidden = guarded_hidden();
    hidden.push("integrate_workspace".to_owned());
    assert_eq!(request.hidden_tools, hidden);
    let rules = &request.instructions[request.instructions.find("## Delegation").unwrap()..];
    assert!(rules.contains("Your workspace is READ-ONLY, and so is every child's"));
    assert_eq!(rules.matches("integrate_workspace").count(), 0, "{rules}");
    assert!(rules.contains("policy for this item: any model Aether lists. Archon cancels"));
}

fn unreachable_aether() -> ArchonError {
    ArchonError::Unavailable("aether is restarting".into())
}

fn stage_labels(h: &Harness) -> Vec<String> {
    let requests = h.backend.requests();
    requests
        .iter()
        .map(|r| r.labels["archon.stage"].clone())
        .collect()
}

/// Aether ends an agent's children only on its cancel, not its completion. So
/// the ended lead is cancelled, and `ship` is not created before that.
#[tokio::test]
async fn a_completed_leads_helpers_are_stopped_before_the_next_stage_starts() {
    let h = Harness::new().await;
    led_item(&h).await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    // The stop fails at the settle and at the first sweep.
    h.backend.fail_next_cancel(unreachable_aether());
    h.backend.fail_next_cancel(unreachable_aether());
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build"], "ship waits for the stop");
    assert!(h.backend.cancelled().is_empty());
    let waiting = h.live_stages().await;
    assert_eq!(waiting.len(), 1);
    assert_eq!(
        (waiting[0].stage_id.as_str(), &waiting[0].state),
        ("ship", &StageState::Spawning)
    );

    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), [lead]);
    assert_eq!(stage_labels(&h), ["build", "ship"]);
}

/// The stop is in the store: an archond that restarts before Aether took
/// it still cancels the lead before it creates the next stage's agent.
#[tokio::test]
async fn an_owed_stop_survives_a_restart_and_still_goes_first() {
    let mut h = Harness::new().await;
    led_item(&h).await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    h.backend.fail_next_cancel(unreachable_aether());
    h.backend.fail_next_cancel(unreachable_aether());
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build"]);

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    h.backend.fail_next_cancel(unreachable_aether());
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build"], "still owed after the restart");
    assert!(h.backend.cancelled().is_empty());
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), [lead]);
    assert_eq!(stage_labels(&h), ["build", "ship"]);
}

/// A lead that died is replaced, and the replacement would share the
/// worktree with whatever the dead one delegated: that is stopped first.
#[tokio::test]
async fn a_failed_leads_helpers_are_stopped_before_its_replacement_starts() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let dead = h.running().await.remove(0).aether_agent.unwrap();
    h.backend.fail_next_cancel(unreachable_aether());
    h.backend.fail_next_cancel(unreachable_aether());
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine crashed".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    assert_eq!(h.backend.created(), 1, "the replacement waits");
    assert!(h.backend.cancelled().is_empty());

    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&dead));
    assert_eq!(stage_labels(&h), ["build", "build"]);
    let replacement = h.running().await.remove(0);
    assert_ne!(replacement.aether_agent.as_ref(), Some(&dead));

    // A lead that gives up ends the run; its helpers are stopped all the same.
    h.settle_running(|_| StageOutcome::Failed {
        error: "the brief contradicts itself".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let cancelled = h.backend.cancelled();
    assert!(
        cancelled.contains(replacement.aether_agent.as_ref().unwrap()),
        "{cancelled:?}"
    );
    assert_eq!(stage_labels(&h), ["build", "build"], "nothing ships");
}

/// The lead is left to end the turn it completed in, as a run node leaves
/// it: only then is it cancelled, and only then does `ship` start.
#[tokio::test]
async fn the_stop_waits_for_the_lead_to_end_its_turn() {
    let h = Harness::new().await;
    led_item(&h).await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    h.backend.hold_turn(&lead);
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    assert!(
        h.backend.cancelled().is_empty(),
        "its turn is not cut short"
    );
    assert_eq!(stage_labels(&h), ["build"]);

    h.backend.end_turn(&lead);
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), [lead]);
    assert_eq!(stage_labels(&h), ["build", "ship"]);
}

/// `ship` shares the worktree with what the lead delegated: it is not created while
/// Aether holds one of them live, however deep, and the cancel is not repeated.
#[tokio::test]
async fn the_next_stage_waits_until_every_helper_of_the_lead_has_ended() {
    let h = Harness::new().await;
    led_item(&h).await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    let helper = h.backend.delegate(&lead, MODEL);
    let below = h.backend.delegate(&helper, MODEL);
    h.backend.end_delegated(&helper, DescendantState::Completed);
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&lead));
    assert_eq!(stage_labels(&h), ["build"], "accepted is not ended");

    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build"], "a helper's helper still runs");
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&lead));
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());

    h.backend.end_delegated(&below, DescendantState::Stopped);
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build", "ship"]);
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&lead));

    // What the lead delegated to is on record as Aether held it at the end.
    let recorded = h.ctx.store.lock().await.delegated_beneath(&lead, true);
    let recorded: Vec<_> = recorded
        .unwrap()
        .into_iter()
        .map(|d| (d.agent, d.parent, d.effort))
        .collect();
    let medium = Some(Effort::Medium);
    assert_eq!(recorded.len(), 3, "helper, its helper, the reviewer");
    assert!(recorded.contains(&(helper.clone(), lead.clone(), medium)));
    assert!(recorded.contains(&(below, helper, medium)), "{recorded:?}");
}

/// archond's own delivery reads the worktree the lead's helpers may still
/// write: it waits for them to end, as a next stage's agent would.
#[tokio::test]
async fn archond_delivers_only_once_the_leads_helpers_have_ended() {
    let h = Harness::new().await;
    let item = adaptive_item(&h).await;
    let lead = h.running().await.remove(0).aether_agent.unwrap();
    let helper = h.backend.delegate(&lead, MODEL);
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&lead));
    let branch = h.run(&item.id).await.branch;
    assert_eq!(pushed(&origin_of(&h), &branch), None, "a helper still runs");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));

    h.backend.end_delegated(&helper, DescendantState::Stopped);
    h.dispatch().await;
    assert!(delivered(&h, &item).await, "{:?}", h.state(&item.id).await);
    assert!(pushed(&origin_of(&h), &branch).is_some());
    assert_eq!(stage_labels(&h), ["build"], "no agent ships");
}

/// A dead lead's replacement would work beside what the dead one delegated:
/// it waits for those agents to end too, across a restart of archond.
#[tokio::test]
async fn a_replacement_lead_waits_for_the_dead_leads_helpers_across_a_restart() {
    let mut h = Harness::new().await;
    adaptive_item(&h).await;
    let dead = h.running().await.remove(0).aether_agent.unwrap();
    let helper = h.backend.delegate(&dead, MODEL);
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine crashed".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    assert_eq!(h.backend.created(), 1, "the replacement waits");

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    h.backend.fail_next_descendants(unreachable_aether());
    h.dispatch().await;
    assert_eq!(h.backend.created(), 1, "no answer confirms nothing");
    h.dispatch().await;
    assert_eq!(h.backend.created(), 1, "the helper still runs");
    assert_eq!(h.backend.cancelled(), std::slice::from_ref(&dead));

    h.backend.end_delegated(&helper, DescendantState::Stopped);
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build", "build"]);
    assert_eq!(h.backend.cancelled(), [dead], "the stored cancel is kept");
}

/// A stop Aether takes long over holds the item visibly: the operator is told
/// once which agents it waits for, and again when they have ended.
#[tokio::test]
async fn a_stop_that_stays_unconfirmed_is_reported_once_and_stays_owed() {
    let h = Harness::new().await;
    let item = led_item(&h).await;
    let stage = h.running().await.remove(0);
    let helper = h
        .backend
        .delegate(stage.aether_agent.as_ref().unwrap(), MODEL);
    settle_reviewed(&h, "Built; reviewed").await;
    h.dispatch().await;
    let key = format!("descendant_stop:{}", stage.id);
    let mut owed: serde_json::Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    let long_ago = archon_core::now() - chrono::Duration::minutes(10);
    owed["cancelled_at"] = serde_json::json!(long_ago);
    let aged = owed.to_string();
    h.ctx.store.lock().await.kv_set(&key, &aged).unwrap();

    h.dispatch().await;
    h.dispatch().await;
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "said once: {blockers:?}");
    assert!(blockers[0].contains("Stage `build` ended"), "{blockers:?}");
    assert!(blockers[0].contains(helper.as_str()), "{blockers:?}");
    assert_eq!(stage_labels(&h), ["build"], "still owed");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));

    h.backend.end_delegated(&helper, DescendantState::Stopped);
    h.dispatch().await;
    assert_eq!(stage_labels(&h), ["build", "ship"]);
    assert!(h.kv(&key).await.is_none());
}

/// A stage agent that could not delegate left nothing running: it is not
/// cancelled, and the next stage does not wait.
#[tokio::test]
async fn a_leaf_stage_owes_no_stop() {
    let h = Harness::new().await;
    h.enqueue("an ordinary change").await;
    h.dispatch().await;
    assert!(h.backend.requests()[0].leaf);
    h.backend.fail_next_cancel(unreachable_aether());
    h.settle_running(passing).await;
    h.dispatch().await;
    assert!(h.backend.cancelled().is_empty());
    assert_eq!(stage_labels(&h), ["implement", "review", "review"]);
}
