//! Crash and restart: a fresh context over the same store runs
//! `executor::recover`, then the dispatcher repairs what Aether still owes.

use crate::common;

use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, StageRunId};
use archon_core::ports::NetworkPolicy;
use archon_core::protocol::Request;
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_core::steer::{InsertStage, Steer, Then};
use archon_daemon::executor;
use common::{Harness, completed, verdict};
use serde_json::Value;

#[tokio::test]
async fn spawning_stage_is_reissued_under_the_same_key() {
    let mut h = Harness::new().await;
    // Aether creates the agent, but the daemon dies before hearing so.
    h.backend.lose_next_reply();
    let item = h.enqueue("crash mid-spawn").await;
    h.dispatch().await;
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].state, StageState::Spawning);

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    assert_eq!(
        h.backend.requests().len(),
        1,
        "recovery leaves Aether to the dispatcher"
    );
    h.dispatch().await;

    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "byte-identical request, same idempotency key"
    );
    assert_eq!(h.backend.created(), 1, "no second agent");
    assert_eq!(
        h.backend.nodes_created(),
        1,
        "the run node survives the restart"
    );
    assert_eq!(requests[1].parent, Some(h.backend.node_ids()[0].clone()));
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "the same stage run, not a new one");
    assert_eq!(stages[0].state, StageState::Running);
}

#[tokio::test]
async fn dispatching_item_without_a_run_is_requeued() {
    let mut h = Harness::new().await;
    let item = h.enqueue("crash before the run").await;
    {
        let mut store = h.ctx.store.lock().await;
        store
            .transition_item(&item.id, QueueState::Dispatching, archon_core::now())
            .expect("dispatching");
    }
    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);

    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(h.state(&item.id).await, QueueState::Running { run: run.id });
    assert_eq!(h.backend.requests().len(), 1);
}

/// A daemon from before run nodes created stage agents as roots. One that
/// crashed after Aether created such an agent, but before hearing so,
/// must replay the request exactly as it stored it: Aether fingerprints
/// the frame, and a parent added on replay would be refused.
#[tokio::test]
async fn a_pre_upgrade_root_stage_is_replayed_as_a_root() {
    let mut h = Harness::new().await;
    h.backend.lose_next_reply();
    let item = h.enqueue("upgrade mid-spawn").await;
    h.dispatch().await;
    let stage = h.stages(&item.id).await.remove(0);
    assert_eq!(stage.state, StageState::Spawning);
    legacy_spawn_record(&h, &stage.id).await;
    h.backend.recorded_as_root(stage.id.as_str());

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].parent, None, "replayed as stored: a root");
    assert_eq!(h.backend.created(), 1, "no second agent");
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].state, StageState::Running);
    assert!(stages[0].aether_agent.is_some());

    assert_eq!(requests[1].network, None, "and states no network");
    let run = h.run(&item.id).await;
    let root_run = run.aether_run.clone().expect("recorded from the reply");
    assert_eq!(root_run.as_str(), "run_root");

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2);
    assert_eq!(h.backend.nodes_created(), 1, "the run's first node");
    let node = &h.backend.node_requests()[0];
    assert_eq!(node.idempotency_key, format!("{}:node:1", run.id));
    assert_eq!(node.attach_run, Some(root_run), "in the root's run");
    assert!(
        h.backend.requests()[2..].iter().all(|r| r.parent.is_some()),
        "later stages of the run sit under a node"
    );
}

/// The store as the previous version left it: a spawn record with no
/// binding and neither parent nor network in its request, no run node,
/// and no Aether run yet, since the create's reply was lost.
async fn legacy_spawn_record(h: &Harness, id: &StageRunId) {
    let key = format!("spawn_request:{id}");
    let mut store = h.ctx.store.lock().await;
    let raw = store.kv_get(&key).expect("kv").expect("stored spawn");
    let mut spawn: Value = serde_json::from_str(&raw).expect("json");
    let record = spawn.as_object_mut().expect("object");
    assert_eq!(record.remove("parent"), Some(Value::String("bound".into())));
    let request = record["request"].as_object_mut().expect("request");
    let node = request.remove("parent").expect("bound to a node");
    assert!(request.remove("network").is_some_and(|n| n.is_string()));
    store.kv_set(&key, &spawn.to_string()).expect("kv set");

    let stage = store.stage_run(id).expect("stage run");
    let mut run = store.run(&stage.run).expect("run");
    assert!(run.aether_run.take().is_some(), "joined the node's run");
    store.update_run(&run).expect("update run");
    let node_key = format!("run_node:{}", run.id);
    assert!(store.kv_get(&node_key).expect("kv").is_some());
    store.kv_delete(&node_key).expect("kv delete");
    let node = node.as_str().expect("a node id");
    store
        .kv_delete(&format!("run_node_agent:{node}"))
        .expect("kv delete");
}

/// The node's create reached Aether but its reply did not: after the
/// restart the stored request is replayed under its key and the stage
/// sits under the node Aether had already created.
#[tokio::test]
async fn a_lost_node_reply_is_replayed_after_a_restart() {
    let mut h = Harness::new().await;
    h.backend.lose_next_node_reply();
    let item = h.enqueue("crash mid-node").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    assert!(
        h.backend.requests().is_empty(),
        "no stage agent without its node"
    );

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0], nodes[1], "the stored request, byte for byte");
    assert_eq!(h.backend.nodes_created(), 1);
    let stages = h.stages(&item.id).await;
    assert_eq!(stages[0].state, StageState::Running);
    assert_eq!(
        h.backend.requests()[0].parent,
        Some(h.backend.node_ids()[0].clone())
    );
}

/// The run finished, but its node's cancel was unanswered when the daemon
/// died: the next daemon retires it from the store, with no stage left to
/// remind it.
#[tokio::test]
async fn a_node_retirement_pending_at_a_crash_is_finished_after_restart() {
    let mut h = Harness::new().await;
    let item = h.enqueue("crash before retire").await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    h.backend
        .fail_next_cancel(archon_core::error::ArchonError::Unavailable(
            "aether away".into(),
        ));
    h.settle_running(|_| completed("opened a PR")).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(h.backend.cancelled().is_empty());

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    let run = h.run(&item.id).await;
    assert_eq!(h.kv(&format!("run_node:{}", run.id)).await, None);
}

/// The run finished while its last stage agent was still ending its turn,
/// and the daemon died: the next daemon keeps waiting for that agent from
/// the store, then retires the node.
#[tokio::test]
async fn a_retirement_waiting_for_a_turn_at_a_crash_resumes_after_restart() {
    let mut h = Harness::new().await;
    let item = h.enqueue("crash while ending").await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    let ship = h.running().await.remove(0);
    let agent = ship.aether_agent.clone().expect("agent");
    h.backend.hold_turn(&agent);
    h.settle_running(|_| completed("opened a PR")).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let run = h.run(&item.id).await;
    let key = format!("run_node:{}", run.id);
    assert!(h.kv(&key).await.is_some());

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert!(
        h.backend.cancelled().is_empty(),
        "still waiting for the agent"
    );
    assert!(h.kv(&key).await.is_some());
    h.backend.end_turn(&agent);
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), h.backend.node_ids());
    assert_eq!(h.kv(&key).await, None);
}

/// Aether restarted with another `[network] default` while the daemon was
/// down: the node's recorded network still binds the run's later stages.
#[tokio::test]
async fn a_changed_network_default_across_a_restart_keeps_the_nodes() {
    let mut h = Harness::new().await;
    h.backend.set_network_default(NetworkPolicy::Denied);
    let item = h.enqueue("default changed").await;
    h.dispatch().await;
    assert_eq!(h.backend.requests()[0].network, Some(NetworkPolicy::Denied));

    h.restart();
    h.backend.set_network_default(NetworkPolicy::Allowed);
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "{:?}", h.stages(&item.id).await);
    assert!(
        h.backend.requests()[1..]
            .iter()
            .all(|r| r.network == Some(NetworkPolicy::Denied)),
        "{:?}",
        h.backend.requests()
    );
    assert_eq!(h.backend.nodes_created(), 1, "the node was never replaced");
}

#[tokio::test]
async fn running_run_resumes_without_duplicate_spawns() {
    let mut h = Harness::new().await;
    let item = h.enqueue("crash mid-review").await;
    h.dispatch().await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviewers = h.running().await;
    assert_eq!(reviewers.len(), 2);
    // One reviewer settles; the daemon dies before the other does.
    let first = reviewers[0].aether_agent.clone().expect("agent");
    executor::stage_settled(&h.ctx, &first, verdict(true))
        .await
        .expect("settle");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 3, "nothing is re-spawned");
    assert_eq!(h.backend.nodes_created(), 1, "no new run node either");
    let still = h.running().await;
    assert_eq!(still.len(), 1);
    assert_eq!(still[0].id, reviewers[1].id);

    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    let ship = h.running().await;
    assert_eq!(ship.len(), 1);
    assert_eq!(ship[0].stage_id, "ship");
    h.settle_running(|_| completed("shipped")).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.backend.created(), h.backend.requests().len());
    let node = h.backend.node_ids()[0].clone();
    let parents: Vec<_> = h
        .backend
        .requests()
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert!(
        parents.iter().all(|p| *p == Some(node.clone())),
        "{parents:?}"
    );
    assert_eq!(
        h.backend.cancelled(),
        vec![node],
        "the node ends with its run"
    );
}

/// A repository config broken while the daemon was down does not fail work
/// under way: recovery needs no model policy, only a later spawn does.
#[tokio::test]
async fn a_malformed_repo_config_at_restart_keeps_a_running_item() {
    let mut h = Harness::new().await;
    let item = h.enqueue("restart over a broken config").await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    h.repo_config("[models\nallow = ");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
    let still: Vec<StageRunId> = h.running().await.into_iter().map(|s| s.id).collect();
    assert_eq!(still, [running[0].id.clone()]);

    // Mended, the config lets the run spawn its next stage.
    h.repo_config("");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let next: Vec<String> = h.running().await.into_iter().map(|s| s.stage_id).collect();
    assert_eq!(next, ["review", "review"]);
}

/// Aether lost the run node (a wipe, an operator cancel in its TUI) while
/// the daemon was down: the next stage gets a replacement under the next
/// generation's key, and the run goes on.
#[tokio::test]
async fn a_run_node_lost_during_downtime_is_replaced() {
    let mut h = Harness::new().await;
    let item = h.enqueue("node lost").await;
    h.dispatch().await;
    let first = h.backend.node_ids()[0].clone();
    h.backend.kill(&first);

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviewers = h.running().await;
    assert_eq!(reviewers.len(), 2);
    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 2, "one create per generation");
    let run = h.run(&item.id).await;
    assert_eq!(nodes[0].idempotency_key, format!("{}:node:1", run.id));
    assert_eq!(nodes[1].idempotency_key, format!("{}:node:2", run.id));
    assert_eq!(
        nodes[1].attach_run.as_ref(),
        run.aether_run.as_ref(),
        "the replacement joins the run's Aether run"
    );
    let second = h.backend.node_ids()[1].clone();
    assert_ne!(first, second);
    let parents: Vec<_> = h.backend.requests()[1..]
        .iter()
        .map(|r| r.parent.clone())
        .collect();
    assert_eq!(parents, vec![Some(second.clone()), Some(second)]);
}

#[tokio::test]
async fn run_resumes_on_its_snapshot_when_the_pipeline_is_edited() {
    let mut h = Harness::new().await;
    let item = h.enqueue("pipeline edited mid-run").await;
    h.dispatch().await;
    let pipeline = h.repo.join(".archon/pipelines").join(common::PIPELINE);
    std::fs::write(pipeline.join("pipeline.toml"), "not toml [").expect("break the pipeline");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviewers = h.running().await;
    assert_eq!(
        reviewers.len(),
        2,
        "the frozen spec still has two reviewers"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn run_without_its_snapshot_fails_cleanly() {
    let mut h = Harness::new().await;
    let item = h.enqueue("snapshot lost").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let snapshot = h
        .ctx
        .paths
        .data_dir
        .join("pipeline-runs")
        .join(run.id.as_str());
    assert!(snapshot.is_dir(), "the run's pipeline is frozen at start");
    std::fs::remove_dir_all(&snapshot).expect("remove snapshot");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the run fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("cannot resume"), "{error}");
    // The orphaned implementer is stopped once Aether is reachable.
    h.dispatch().await;
    assert!(h.live_stages().await.is_empty());
    let agent = h.stages(&item.id).await[0]
        .aether_agent
        .clone()
        .expect("agent");
    let node = h.backend.node_ids()[0].clone();
    assert_eq!(
        h.backend.cancelled(),
        vec![agent.clone(), node],
        "the implementer, then the run node it sat under"
    );
    let outcome = StageOutcome::Completed {
        summary: "late".into(),
        structured: None,
    };
    executor::stage_settled(&h.ctx, &agent, outcome)
        .await
        .expect("late settle");
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
}

/// An infra retry persists the replacement before settling the failed stage
/// run. A crash in between leaves both live; neither may spawn a third agent
/// or end the slot.
#[tokio::test]
async fn crash_mid_infra_retry_neither_respawns_nor_fails_the_slot() {
    let mut h = Harness::new().await;
    let item = h.enqueue("crash mid-retry").await;
    h.dispatch().await;
    let failing = h.running().await.remove(0);
    reserve_replacement_by_hand(&h, &failing).await;

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 2, "the replacement is issued");
    let agent = failing.aether_agent.clone().expect("agent");
    let infra = StageOutcome::Failed {
        error: "host lost".into(),
        infra: true,
    };
    executor::stage_settled(&h.ctx, &agent, infra)
        .await
        .expect("settle");
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 2, "no third agent for the slot");
    let live = h.running().await;
    assert_eq!(live.len(), 1, "only the replacement runs");
    assert_ne!(live[0].id, failing.id);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));

    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 2, "the run moves on to review");
}

/// What `infra_retry` persists first: a `spawning` stage run for the same
/// slot with its own stored request.
async fn reserve_replacement_by_hand(h: &Harness, failing: &StageRun) {
    let mut replacement = failing.clone();
    replacement.id = StageRunId::new();
    replacement.state = StageState::Spawning;
    replacement.aether_agent = None;
    replacement.aether_task = None;
    replacement.started_at = failing.started_at + chrono::Duration::microseconds(1);
    let mut store = h.ctx.store.lock().await;
    let raw = store
        .kv_get(&format!("spawn_request:{}", failing.id))
        .expect("kv")
        .expect("stored spawn");
    let mut spawn: serde_json::Value = serde_json::from_str(&raw).expect("json");
    spawn["request"]["idempotency_key"] = replacement.id.as_str().into();
    let key = format!("spawn_request:{}", replacement.id);
    store.kv_set(&key, &spawn.to_string()).expect("kv set");
    store.upsert_stage_run(&replacement).expect("replacement");
}

/// A catalog pipeline whose one stage a steer inserts into a run.
const STEER_SOURCE: &str = r#"[pipeline]
name = "steer-source"
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

/// A catalog pipeline a steer switches a run to.
const STEER_TARGET: &str = r#"[pipeline]
name = "steer-target"
description = "no review"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "ship"
kind = "ship"
prompt = "plain-ship.md"
timeout_minutes = 90
"#;

/// Add the two pipelines steers name, each with a prompt of its own.
fn add_steer_pipelines(h: &Harness) {
    let pipelines = h.repo.join(".archon/pipelines");
    h.add_pipeline("steer-source", STEER_SOURCE);
    let verify = "Verify {{task.title}} by running it.\n\n{{task.brief}}\n";
    std::fs::write(pipelines.join("steer-source/verify.md"), verify).expect("prompt");
    h.add_pipeline("steer-target", STEER_TARGET);
    let ship = "Ship {{task.title}} unreviewed: {{previous.implement.summary}}\n";
    std::fs::write(pipelines.join("steer-target/plain-ship.md"), ship).expect("prompt");
}

async fn steer(h: &Harness, item: &QueueItemId, steer: Steer) {
    let request = Request::SteerItem {
        item: item.clone(),
        steer,
    };
    h.cli(request).await.expect("steered");
}

/// The stage ids of the item's stage runs, in the order they ran.
async fn ran(h: &Harness, item: &QueueItemId) -> Vec<String> {
    let stages = h.stages(item).await;
    stages.into_iter().map(|s| s.stage_id).collect()
}

/// Shape edits are applied to the run's snapshot before its driver is
/// rebuilt, so a restart keeps them whatever the catalog says by then.
#[tokio::test]
async fn an_inserted_stage_and_a_removed_one_survive_a_restart() {
    let mut h = Harness::new().await;
    add_steer_pipelines(&h);
    let item = h.enqueue("steered, then restarted").await;
    h.dispatch().await;
    let insert = Steer {
        insert_stage: Some(InsertStage {
            stage: "verify".into(),
            from: Some("steer-source".into()),
            after: Some("implement".into()),
        }),
        ..Steer::default()
    };
    steer(&h, &item.id, insert).await;
    let remove = Steer {
        stage: Some("review".into()),
        abort: Some(Then::Skip),
        ..Steer::default()
    };
    steer(&h, &item.id, remove).await;
    std::fs::remove_dir_all(h.repo.join(".archon/pipelines/steer-source")).expect("catalog edit");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(
        h.backend.created(),
        1,
        "the implementer is not spawned twice"
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let verify = h
        .backend
        .requests()
        .pop()
        .expect("the inserted stage's request");
    assert!(
        verify
            .objective
            .starts_with("Verify steered, then restarted by running it.")
    );
    h.settle_running(|_| completed("verified")).await;
    h.dispatch().await;
    h.settle_running(|_| completed("shipped")).await;
    h.dispatch().await;
    assert_eq!(ran(&h, &item.id).await, ["implement", "verify", "ship"]);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn a_switched_pipeline_survives_a_restart() {
    let mut h = Harness::new().await;
    add_steer_pipelines(&h);
    let item = h.enqueue("switched, then restarted").await;
    h.dispatch().await;
    let switch = Steer {
        switch_pipeline: Some("steer-target".into()),
        ..Steer::default()
    };
    steer(&h, &item.id, switch).await;
    std::fs::remove_dir_all(h.repo.join(".archon/pipelines/steer-target")).expect("catalog edit");

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented in one go"))
        .await;
    h.dispatch().await;
    let ship = h
        .backend
        .requests()
        .pop()
        .expect("the switched-in stage's request");
    assert_eq!(
        ship.objective.trim(),
        "Ship switched, then restarted unreviewed: implemented in one go"
    );
    h.settle_running(|_| completed("shipped")).await;
    h.dispatch().await;
    assert_eq!(ran(&h, &item.id).await, ["implement", "ship"]);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));

    // A later run of the item keeps the stages steered into it.
    let retried = h.item(&item.id).await;
    assert_eq!(
        retried.model_plan.0.len(),
        2,
        "a plan for implement and ship"
    );
    assert!(h.kv(&format!("shape:{}", item.id)).await.is_some());
}

/// A crash right after an abort is stored loses neither the retry nor the
/// stop its agent is owed, and an undelivered note is delivered once.
#[tokio::test]
async fn an_abort_and_an_undelivered_note_survive_a_restart() {
    let mut h = Harness::new().await;
    let item = h.enqueue("aborted, then restarted").await;
    h.dispatch().await;
    let working = h.running().await.remove(0);
    let agent = working.aether_agent.clone().expect("agent");
    let away = || ArchonError::Unavailable("aetherd is restarting".into());
    h.backend.fail_next_cancel(away());
    let abort = Steer {
        abort: Some(Then::Retry),
        ..Steer::default()
    };
    steer(&h, &item.id, abort).await;
    assert!(
        h.backend.cancelled().is_empty(),
        "Aether did not take the stop"
    );

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), [agent], "the stop is repeated");
    let retry = h.running().await.remove(0);
    assert_eq!((retry.stage_id.as_str(), retry.attempt), ("implement", 2));
    assert_eq!(h.backend.created(), 2, "one retry, not two");

    h.backend.fail_next_message(away());
    let note = Steer {
        note: Some("Keep the public API as it is.".into()),
        ..Steer::default()
    };
    steer(&h, &item.id, note).await;
    assert!(h.backend.messages().is_empty());
    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    h.dispatch().await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(Some(&messages[0].0), retry.aether_agent.as_ref());
    assert!(messages[0].1.contains("Keep the public API as it is."));
}
