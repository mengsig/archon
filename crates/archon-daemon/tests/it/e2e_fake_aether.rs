//! End to end against a fake aetherd speaking the real wire protocol:
//! enqueue → dispatch → `create_agent` for the run node, then for the stage
//! agent under it with the planned model, effort, stage contract and
//! worktree → task outcomes read with `get_task` → gates → a terminal item.
//! The fake's fleet enforces what Aether documents of a child's scope: it
//! may not widen its parent's network.

use crate::common;
mod repository_root;
mod steer_helpers;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::backend::AetherBackend;
use archon_aether::client::{AetherClient, ClientOptions};
use archon_aether::fake::{self, FakeAether, Scripted};
use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::config::Config;
use archon_core::model::Effort;
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, StageRun, StageState};
use archon_daemon::executor;
use common::{FakeBackend, Harness, MODEL, REVIEW_MODEL};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(5);

/// What the fake aetherd knows: one agent and task per stage idempotency
/// key, one passive agent per run-node key with the network its scope
/// stated, and each task's status.
#[derive(Default)]
struct Fleet {
    by_key: BTreeMap<String, u32>,
    nodes_by_key: BTreeMap<String, u32>,
    node_network: BTreeMap<String, Value>,
    status: BTreeMap<String, Value>,
    cancelled: BTreeSet<String>,
    requests: BTreeMap<String, Value>,
    lose_next_reply: bool,
}

impl Fleet {
    fn create(&mut self, frame: &Value) -> Scripted {
        let key = frame["idempotency_key"].as_str().expect("keyed").to_owned();
        assert!(!key.is_empty(), "fleet creations are keyed");
        assert!(
            frame["start"].is_boolean(),
            "creation states whether it starts"
        );
        if frame["start"] == json!(false) {
            let next = self.nodes_by_key.len() as u32 + 1;
            let n = *self.nodes_by_key.entry(key).or_insert(next);
            self.node_network
                .insert(node_id(n), frame["scope"]["network"].clone());
            return Scripted::reply("agent", fake::agent(&node_id(n), None));
        }
        if self.widens_parent(frame) {
            return Scripted::refuse(
                "invalid_argument",
                "child scope must narrow its parent's: network",
            );
        }
        let next = self.by_key.len() as u32 + 1;
        let n = *self.by_key.entry(key).or_insert(next);
        let (agent, task) = ids(n);
        Scripted::reply("agent", fake::agent(&agent, Some(&task)))
    }

    /// Aether refuses a child whose scope is wider than its parent's.
    fn widens_parent(&self, frame: &Value) -> bool {
        let Some(parent) = frame["parent"].as_str() else {
            return false;
        };
        let parent_network = self.node_network.get(parent);
        parent_network == Some(&json!("denied")) && frame["scope"]["network"] == json!("allowed")
    }

    fn agents(&self) -> Vec<Value> {
        let nodes = self
            .nodes_by_key
            .iter()
            .map(|(key, n)| (key, node_id(*n), None));
        let stages = self.by_key.iter().map(|(key, n)| {
            let (agent, task) = ids(*n);
            (key, agent, Some(task))
        });
        nodes
            .chain(stages)
            .map(|(key, id, task)| {
                let mut agent = fake::agent(&id, task.as_deref());
                agent["parent"] = self.requests[key]["parent"].clone();
                agent["status"] = json!({"state": self.agent_state(&id)});
                agent
            })
            .collect()
    }

    /// The task's owner completed with it, as far as the fake knows.
    fn agent_state(&self, agent: &str) -> &'static str {
        if self.cancelled.contains(agent) {
            return "cancelled";
        }
        let task = agent.replace("agt_", "tsk_");
        match self.status.get(&task) {
            Some(status) if status["state"] != json!("active") => "completed",
            _ => "ready",
        }
    }
}

fn ids(n: u32) -> (String, String) {
    (format!("agt_{n:04}"), format!("tsk_{n:04}"))
}

fn node_id(n: u32) -> String {
    format!("agt_n{n:04}")
}

/// `create_agent` is idempotent per key: a `start: false` create is a run
/// node without a task, any other a stage agent with one, refused when it
/// widens its parent's network. `get_agent` reports an agent `ready` until
/// its task settled and `completed` after; `get_task` the status the test
/// set, `active` until then.
fn script_fleet(aether: &FakeAether) -> Arc<Mutex<Fleet>> {
    let fleet = Arc::new(Mutex::new(Fleet::default()));
    let created = fleet.clone();
    aether.set_responder(
        "create_agent",
        Arc::new(move |frame: &Value| {
            let key = frame["idempotency_key"].as_str().expect("keyed");
            let mut request = frame.clone();
            request.as_object_mut().unwrap().remove("req");
            let mut fleet = created.lock().expect("fleet");
            if let Some(stored) = fleet.requests.get(key)
                && stored != &request
            {
                return Scripted::refuse("invalid_argument", "a different request needs a new key");
            }
            fleet.requests.insert(key.into(), request);
            let reply = fleet.create(frame);
            if std::mem::take(&mut fleet.lose_next_reply) {
                return Scripted::Silent;
            }
            reply
        }),
    );
    let states = fleet.clone();
    aether.set_responder(
        "get_agent",
        Arc::new(move |frame: &Value| {
            let id = frame["id"].as_str().expect("agent id");
            let mut agent = fake::agent(id, None);
            let state = states.lock().expect("fleet").agent_state(id);
            agent["status"] = json!({"state": state});
            Scripted::reply("agent", agent)
        }),
    );
    let listed = fleet.clone();
    aether.set_responder(
        "list_agents",
        Arc::new(move |_| Scripted::reply("agents", json!(listed.lock().expect("fleet").agents()))),
    );
    let tasks = fleet.clone();
    aether.set_responder(
        "get_task",
        Arc::new(move |frame: &Value| {
            let task = frame["id"].as_str().expect("task id").to_owned();
            let agent = task.replace("tsk_", "agt_");
            let fleet = tasks.lock().expect("fleet");
            let status = fleet
                .status
                .get(&task)
                .cloned()
                .unwrap_or(json!({"state": "active"}));
            Scripted::reply("task", fake::task(&task, Some(&agent), "run_0001", status))
        }),
    );
    let cancelled = fleet.clone();
    aether.set_responder(
        "cancel",
        Arc::new(move |frame: &Value| {
            let agent = frame["agent"].as_str().expect("cancel names an agent");
            cancelled
                .lock()
                .expect("fleet")
                .cancelled
                .insert(agent.to_owned());
            Scripted::reply("ok", json!({}))
        }),
    );
    fleet
}

async fn harness(aether: &FakeAether) -> Harness {
    harness_with(aether, ClientOptions::default()).await
}

async fn harness_with(aether: &FakeAether, options: ClientOptions) -> Harness {
    harness_with_config(aether, options, Config::default()).await
}

async fn harness_with_config(
    aether: &FakeAether,
    options: ClientOptions,
    config: Config,
) -> Harness {
    let policy = config.models.policy();
    let h = Harness::build(config, Arc::new(FakeBackend::default()), move |link| {
        Arc::new(AetherBackend::new(link, policy))
    })
    .await;
    let client = AetherClient::connect_with(&aether.endpoint(), options)
        .await
        .expect("connect");
    h.ctx.aether.replace(Some(Arc::new(client))).expect("link");
    h
}

async fn start_aether(h_dir: &std::path::Path) -> FakeAether {
    FakeAether::start(&h_dir.join("aetherd.sock"))
        .await
        .expect("fake aetherd")
}

/// Complete every running stage's task in the fake, then settle it the
/// way the event pump does: read the outcome back through the backend.
async fn complete_running(h: &Harness, fleet: &Mutex<Fleet>, result: impl Fn(&StageRun) -> Value) {
    for stage in h.running().await {
        let task = stage.aether_task.clone().expect("task");
        let status = json!({"state": "completed", "result": result(&stage)});
        fleet
            .lock()
            .expect("fleet")
            .status
            .insert(task.to_string(), status);
        let outcome = h.ctx.backend.task_outcome(&task).await.expect("get_task");
        let outcome = outcome.expect("the task has settled");
        let agent = stage.aether_agent.clone().expect("agent");
        executor::stage_settled(&h.ctx, &agent, outcome)
            .await
            .expect("settle");
    }
}

fn effort(effort: Effort) -> Value {
    serde_json::to_value(effort).expect("effort")
}

fn model(model: &str) -> Value {
    let (provider, name) = model.split_once('/').expect("provider/model");
    json!({"provider": provider, "model": name})
}

/// The run node's `create_agent`: keyed by the run, passive, scoped to the
/// worktree, named after the item.
fn assert_run_node(frame: &Value, run: &PipelineRun, title: &str) {
    assert_eq!(
        frame["idempotency_key"],
        json!(format!("{}:node:1", run.id))
    );
    assert_eq!(frame["role"], json!(format!("archon-run:{}", run.pipeline)));
    assert_eq!(frame["name"], json!(title));
    assert_eq!(frame["start"], json!(false));
    assert!(frame.get("objective").is_none() && frame.get("parent").is_none());
    assert_eq!(frame["scope"]["workspace_root"], json!(run.workspace));
    assert_eq!(
        frame["scope"]["labels"]["archon.run"],
        json!(run.id.as_str())
    );
    assert_eq!(frame["approval"], json!({"require_for": []}));
    assert_eq!(frame["config"]["tools"]["mode"], json!("except"));
    let hidden = frame["config"]["tools"]["names"].as_array().expect("names");
    assert!(
        hidden.contains(&json!("mcp__archon__enqueue")),
        "{hidden:?}"
    );
}

/// Every stage agent sits under the run node.
fn assert_children_of(creates: &[Value], node: &str) {
    assert!(!creates.is_empty());
    for create in creates {
        assert_eq!(create["parent"], json!(node), "{create}");
        assert_eq!(create["start"], json!(true));
    }
}

/// The implementer's `create_agent`: keyed by its stage run, with the
/// planned model and effort, the stage contract and the worktree.
fn assert_create_agent(frame: &Value, stage: &StageRun, workspace: &std::path::Path) {
    assert_eq!(frame["idempotency_key"], json!(stage.id.as_str()));
    assert_eq!(frame["name"], json!("implement"));
    assert_eq!(frame["model"], model(MODEL));
    assert_eq!(frame["config"]["effort"], effort(Effort::Medium));
    let instructions = frame["config"]["instructions"].as_str().expect("contract");
    assert!(!instructions.is_empty());
    assert_eq!(frame["scope"]["workspace_root"], json!(workspace));
    assert_eq!(frame["start"], json!(true));
    let objective = frame["objective"].as_str().expect("objective");
    assert!(objective.contains("wire the greeting"), "{objective}");
}

/// Two reviewers with their planned choices, attached to the item's run.
fn assert_reviewers(creates: &[Value]) {
    let reviewers: Vec<(Value, Value)> = creates
        .iter()
        .map(|c| (c["model"].clone(), c["config"]["effort"].clone()))
        .collect();
    assert_eq!(reviewers.len(), 2, "{reviewers:?}");
    assert!(reviewers.contains(&(model(REVIEW_MODEL), effort(Effort::Medium))));
    assert!(reviewers.contains(&(model(MODEL), effort(Effort::Low))));
    let attached = creates.iter().all(|c| c["run"] == json!("run_0001"));
    assert!(attached, "stages share the item's Aether run");
}

#[tokio::test]
async fn pipeline_runs_to_success_through_a_fake_aetherd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    let fleet = script_fleet(&aether);
    let h = harness(&aether).await;
    let item = h.enqueue("wire the greeting").await;
    h.dispatch().await;

    let creates = aether.wait_for("create_agent", 2, WAIT).await;
    let run = h.run(&item.id).await;
    assert_run_node(&creates[0], &run, "wire the greeting");
    let stage = &h.stages(&item.id).await[0];
    assert_create_agent(&creates[1], stage, &run.workspace);
    assert_children_of(&creates[1..], &node_id(1));
    assert_eq!(stage.state, StageState::Running);
    assert_eq!(
        stage.aether_agent,
        Some(AetherAgentId::parse("agt_0001").expect("id"))
    );

    complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 4, WAIT).await;
    assert_reviewers(&creates[2..]);
    assert_children_of(&creates[2..], &node_id(1));
    let names: Vec<&Value> = creates[2..].iter().map(|c| &c["name"]).collect();
    assert!(names.contains(&&json!("review 1/2")), "{names:?}");
    assert!(names.contains(&&json!("review 2/2")), "{names:?}");

    let pass =
        json!({"summary": "VERDICT: pass", "structured": {"verdict": "pass", "findings": []}});
    complete_running(&h, &fleet, |_| pass.clone()).await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 5, WAIT).await;
    assert!(
        creates[4]["role"]
            .as_str()
            .expect("role")
            .ends_with("/ship")
    );
    assert_children_of(&creates[4..], &node_id(1));

    let shipped =
        json!({"summary": "opened a PR", "structured": {"pr_url": "https://example.invalid/1"}});
    complete_running(&h, &fleet, |_| shipped.clone()).await;
    let QueueState::Succeeded { summary, .. } = h.state(&item.id).await else {
        panic!("not succeeded: {:?}", h.state(&item.id).await);
    };
    assert!(!summary.is_empty());
    assert!(h.live_stages().await.is_empty());
    assert_eq!(
        aether.received_named("create_agent").len(),
        5,
        "every agent created once: the node and four stage agents"
    );
    let cancels = aether.received_named("cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(
        cancels[0]["agent"],
        json!(node_id(1)),
        "the node ends with the run"
    );
    assert!(!run.workspace.exists(), "the shipped worktree is removed");
}

/// Aether's `[network] default` changes (a restart with a new config)
/// while a run is under way: the node was created `denied`, so every later
/// stage agent still states `denied`, which the fake would otherwise refuse
/// as widening its parent. Only the node asks Aether for the default.
#[tokio::test]
async fn a_changed_network_default_never_widens_the_run_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    let fleet = script_fleet(&aether);
    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    let h = harness(&aether).await;
    let item = h.enqueue("offline first").await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 2, WAIT).await;
    assert_eq!(creates[0]["scope"]["network"], json!("denied"), "the node");
    assert_eq!(
        creates[1]["scope"]["network"],
        json!("denied"),
        "the implementer"
    );

    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "allowed"})),
    );
    complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 4, WAIT).await;
    assert_children_of(&creates[2..], &node_id(1));
    assert!(
        creates[2..]
            .iter()
            .all(|c| c["scope"]["network"] == json!("denied")),
        "reviewers state the node's network: {creates:?}"
    );
    let reviewers = h.running().await;
    assert_eq!(reviewers.len(), 2, "{:?}", h.stages(&item.id).await);
    assert_eq!(
        aether.received_named("status").len(),
        3,
        "the default is read once per stage binding, the node's included"
    );
}

/// The operator tightened Aether's default to `denied` mid-run: stage
/// agents bound after that state `denied` under the same node, narrowing
/// it, so none is wider than Aether's current default.
#[tokio::test]
async fn a_default_tightened_mid_run_narrows_later_stage_agents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    let fleet = script_fleet(&aether);
    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "allowed"})),
    );
    let h = harness(&aether).await;
    let item = h.enqueue("online first").await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 2, WAIT).await;
    assert_eq!(creates[0]["scope"]["network"], json!("allowed"), "the node");
    assert_eq!(creates[1]["scope"]["network"], json!("allowed"));

    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 4, WAIT).await;
    assert_children_of(&creates[2..], &node_id(1));
    assert!(
        creates[2..]
            .iter()
            .all(|c| c["scope"]["network"] == json!("denied")),
        "reviewers state the tightened default: {creates:?}"
    );
    assert_eq!(h.running().await.len(), 2, "{:?}", h.stages(&item.id).await);
    assert_eq!(fleet.lock().expect("fleet").nodes_by_key.len(), 1);
}

/// The node's create reached Aether but its reply did not, and Aether's
/// default changed before the replay: the replay repeats the stored frame
/// byte for byte, so Aether answers with the node it already created.
#[tokio::test]
async fn a_lost_node_reply_is_replayed_unchanged_after_the_default_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    aether.script("create_agent", Scripted::Silent);
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        ..ClientOptions::default()
    };
    let h = harness_with(&aether, options).await;
    let item = h.enqueue("lost node reply").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);

    aether.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "allowed"})),
    );
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 3, WAIT).await;
    let strip = |frame: &Value| {
        let mut frame = frame.clone();
        frame.as_object_mut().expect("object").remove("req");
        frame
    };
    assert_eq!(
        strip(&creates[0]),
        strip(&creates[1]),
        "the node, replayed as stored"
    );
    assert_eq!(creates[1]["scope"]["network"], json!("denied"));
    assert_eq!(creates[2]["parent"], json!(node_id(1)));
    assert_eq!(creates[2]["scope"]["network"], json!("denied"));
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
}

#[tokio::test]
async fn aether_refusing_create_agent_fails_the_stage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    // The first create is the run node's; the stage's is the refused one.
    aether.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(&node_id(1), None)),
    );
    aether.script(
        "create_agent",
        Scripted::refuse("invalid_argument", "no such model"),
    );
    let h = harness(&aether).await;
    let item = h.enqueue("refused").await;
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("no such model"), "{error}");
    let stages = h.stages(&item.id).await;
    assert!(
        matches!(stages[0].state, StageState::Failed { .. }),
        "{:?}",
        stages[0].state
    );
    h.dispatch().await;
    assert_eq!(
        aether.received_named("create_agent").len(),
        2,
        "a refusal is not retried"
    );
}

/// Aether refusing a run node's key (a record it cannot replay, or work
/// it created and lost) moves the node to the next generation once; the
/// stage is created under the replacement.
#[tokio::test]
async fn a_refused_run_node_key_is_left_for_the_next_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    aether.script(
        "create_agent",
        Scripted::refuse(
            "not_found",
            "created under this idempotency_key and no longer in the store",
        ),
    );
    let h = harness(&aether).await;
    let item = h.enqueue("stale key").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let creates = aether.wait_for("create_agent", 3, WAIT).await;
    assert_eq!(
        creates[0]["idempotency_key"],
        json!(format!("{}:node:1", run.id))
    );
    assert_eq!(
        creates[1]["idempotency_key"],
        json!(format!("{}:node:2", run.id))
    );
    assert_eq!(creates[1]["start"], json!(false));
    assert_children_of(&creates[2..], &node_id(1));
    assert_eq!(h.running().await.len(), 1);
}

#[tokio::test]
async fn aether_refusing_the_run_node_twice_fails_the_stage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    for _ in 0..2 {
        aether.script(
            "create_agent",
            Scripted::refuse("invalid_argument", "repository cannot be stored"),
        );
    }
    let h = harness(&aether).await;
    let item = h.enqueue("node refused").await;
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("repository cannot be stored"), "{error}");
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.len(), 2, "no stage agent without its node");
    assert!(creates.iter().all(|c| c["start"] == json!(false)));
}

#[tokio::test]
async fn unanswered_create_agent_is_reissued_under_the_same_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    // No reply in time: Aether may have created the agent, so ask again.
    // The first create is the run node's; the stage's is the silent one.
    aether.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(&node_id(1), None)),
    );
    aether.script("create_agent", Scripted::Silent);
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        ..ClientOptions::default()
    };
    let h = harness_with(&aether, options).await;
    let item = h.enqueue("busy aetherd").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    h.dispatch().await;
    let creates = aether.wait_for("create_agent", 3, WAIT).await;
    let strip = |frame: &Value| {
        let mut frame = frame.clone();
        frame.as_object_mut().expect("object").remove("req");
        frame
    };
    assert_eq!(creates[0]["start"], json!(false), "the node, created once");
    assert_eq!(
        strip(&creates[1]),
        strip(&creates[2]),
        "the same request, byte for byte, under the same node"
    );
    assert_eq!(creates[2]["parent"], json!(node_id(1)));
    let stage = &h.stages(&item.id).await[0];
    assert_eq!(stage.state, StageState::Running);
    assert_eq!(
        stage.aether_task,
        Some(AetherTaskId::parse("tsk_0001").expect("id"))
    );
}

fn script_budget_models(aether: &FakeAether) {
    aether.set_default(
        "providers",
        Scripted::reply(
            "providers",
            json!([
                {"provider": "codex", "protocol": "codex", "engine": true},
                {"provider": "claude-code", "protocol": "claude-code", "engine": true},
            ]),
        ),
    );
    aether.set_default(
        "list_models",
        Scripted::reply(
            "models",
            json!([
                {"model": model(REVIEW_MODEL), "capabilities": {"cost": null}},
                {"model": model(MODEL), "capabilities": {"cost": null}},
            ]),
        ),
    );
}

async fn usage_event(h: &Harness, stage: &StageRun, total: Value) {
    let mut event = fake::event(
        stage.aether_agent.as_ref().map(|id| id.as_str()),
        stage.aether_task.as_ref().map(|id| id.as_str()),
        json!({"type": "agent.usage_updated", "total": total}),
    );
    event["seq"] = json!(1);
    event["at"] = json!(archon_core::now());
    let event = archon_aether::events::parse_event(event).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
}

#[tokio::test]
async fn budgeted_codex_stops_on_counted_estimates_for_item_and_config_caps() {
    use archon_core::protocol::{ItemPatch, Request};
    for item_cap in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        let fleet = script_fleet(&aether);
        script_budget_models(&aether);
        let mut config = Config::default();
        if !item_cap {
            config.limits.budget_usd_per_item = 2.0;
        }
        let h = harness_with_config(&aether, ClientOptions::default(), config).await;
        let item = h
            .enqueue_with("budgeted subscription", common::plan())
            .await
            .unwrap();
        if item_cap {
            h.cli(Request::UpdateItem {
                item: item.id.clone(),
                patch: ItemPatch {
                    budget_usd: Some(2.0),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        }
        h.dispatch().await;
        let first = h.running().await.remove(0);
        assert_eq!(
            aether.received_named("create_agent")[1]["config"]["cost_budget"],
            2.0
        );
        usage_event(
            &h,
            &first,
            json!({"input_tokens": 100, "output_tokens": 20, "cost_complete": false}),
        )
        .await;
        assert!(
            matches!(h.state(&item.id).await, QueueState::Running { .. }),
            "a reporting engine can finish its accounting at turn end"
        );
        usage_event(&h, &first, json!({"cost_usd": 0.5, "cost_complete": true})).await;
        complete_running(&h, &fleet, |_| json!({"summary": "implemented"})).await;
        h.dispatch().await;
        let reviewers = h.running().await;
        let codex = reviewers
            .iter()
            .find(|s| s.choice.model.provider == "codex")
            .unwrap();
        let creates = aether.received_named("create_agent");
        let create = creates
            .iter()
            .find(|frame| frame["idempotency_key"] == json!(codex.id.as_str()))
            .unwrap();
        assert!(create["config"]["cost_budget"].is_null());
        assert!(create.get("fallbacks").is_none());
        let stored = h
            .ctx
            .store
            .lock()
            .await
            .kv_get(&format!("spawn_request:{}", codex.id))
            .unwrap()
            .unwrap();
        let stored: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(stored["request"]["cost_budget_usd"], 1.5);
        assert_eq!(stored["request"]["aether_cost_budget"], false);
        usage_event(
            &h,
            codex,
            json!({"input_tokens": 100, "output_tokens": 20,
            "cost_complete": false, "estimated_cost_usd": 1.0}),
        )
        .await;
        assert!(matches!(
            h.state(&item.id).await,
            QueueState::Running { .. }
        ));
        usage_event(
            &h,
            codex,
            json!({"input_tokens": 200, "output_tokens": 40,
            "cost_complete": false, "estimated_cost_usd": 1.5}),
        )
        .await;
        let QueueState::Failed { error, .. } = h.state(&item.id).await else {
            panic!("budget did not stop the item")
        };
        assert!(error.contains("budget is used up"), "{error}");
        assert!(h.live_stages().await.is_empty());
        assert_eq!(
            h.ctx
                .store
                .lock()
                .await
                .spend_for_item(&item.id, true)
                .unwrap(),
            2.0
        );
        assert!(aether.received_named("cancel").iter().any(|frame| frame["agent"] == json!(codex.aether_agent.as_ref().unwrap().as_str())));
        h.dispatch().await;
        assert_eq!(
            aether.received_named("create_agent").len(),
            4,
            "no ship stage starts"
        );
    }
}

#[tokio::test]
async fn a_budgeted_codex_stage_is_refused_when_estimates_are_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    script_budget_models(&aether);
    let mut config = Config::default();
    config.limits.budget_usd_per_item = 2.0;
    config.limits.count_estimated_cost = false;
    let h = harness_with_config(&aether, ClientOptions::default(), config).await;
    let mut plan = common::plan();
    plan.0.get_mut("implement").unwrap()[0] = common::choice(REVIEW_MODEL, Effort::Low);
    let item = h.enqueue_with("no estimates", plan).await.unwrap();
    h.dispatch().await;
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("unenforceable budget accepted")
    };
    assert!(
        error.contains("count_estimated_cost is disabled"),
        "{error}"
    );
    assert!(aether.received_named("create_agent").is_empty());
}

/// Aether prices a turn at list price while it streams and reports the
/// provider's figure when it ends: the item is charged that, not the peak.
#[tokio::test]
async fn the_providers_turn_end_total_corrects_the_interim_charge() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    script_budget_models(&aether);
    let h = harness_with_config(&aether, ClientOptions::default(), Config::default()).await;
    let item = h.enqueue_with("corrected", common::plan()).await.unwrap();
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let totals = [
        (0.5, "mixed", true, 0.5),
        (0.375, "provider_reported", true, 0.375),
        (0.625, "mixed", true, 0.625),
        // Not the provider's complete figure: the charge stays.
        (0.25, "provider_reported", false, 0.625),
    ];
    for (cost, basis, complete, charged) in totals {
        let total = json!({"cost_usd": cost, "cost_basis": basis, "cost_complete": complete});
        usage_event(&h, &stage, total).await;
        let store = h.ctx.store.lock().await;
        assert_eq!(store.spend_for_item(&item.id, false).unwrap(), charged);
        let today = store.spend_totals(archon_core::now(), false).unwrap();
        assert_eq!(
            today,
            (charged, charged),
            "the daily budget counts the same"
        );
    }
}

#[tokio::test]
async fn missing_subscription_estimates_fail_visibly_only_when_budgeted() {
    for budget in [0.0, 2.0] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        script_fleet(&aether);
        script_budget_models(&aether);
        let mut config = Config::default();
        config.limits.budget_usd_per_item = budget;
        let h = harness_with_config(&aether, ClientOptions::default(), config).await;
        let mut plan = common::plan();
        plan.0.get_mut("implement").unwrap()[0] = common::choice(REVIEW_MODEL, Effort::Low);
        let item = h.enqueue_with("missing accounting", plan).await.unwrap();
        h.dispatch().await;
        let stage = h.running().await.remove(0);
        usage_event(
            &h,
            &stage,
            json!({"input_tokens": 100, "output_tokens": 20, "cost_complete": false}),
        )
        .await;
        if budget == 0.0 {
            assert!(matches!(
                h.state(&item.id).await,
                QueueState::Running { .. }
            ));
            assert!(aether.received_named("cancel").is_empty());
        } else {
            let QueueState::Failed { error, .. } = h.state(&item.id).await else {
                panic!("missing estimates accepted")
            };
            assert!(
                error.contains("without complete dollar cost or a usable subscription estimate"),
                "{error}"
            );
            assert!(h.live_stages().await.is_empty());
        }
    }
}

/// With budgets off, the config's per-item cap and an item's own budget
/// neither fail unpriced usage nor stop an overspent run, no `cost_budget`
/// reaches Aether, and the spend is still recorded.
#[tokio::test]
async fn budgets_off_enforces_no_item_budget_but_records_spend() {
    use archon_core::protocol::{ItemPatch, Request};
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    script_budget_models(&aether);
    let mut config = Config::default();
    config.limits.budgets_enabled = false;
    config.limits.budget_usd_per_item = 2.0;
    let h = harness_with_config(&aether, ClientOptions::default(), config).await;
    let mut plan = common::plan();
    plan.0.get_mut("implement").unwrap()[0] = common::choice(REVIEW_MODEL, Effort::Low);
    let item = h.enqueue_with("budgets off", plan).await.unwrap();
    let reply = h
        .cli(Request::UpdateItem {
            item: item.id.clone(),
            patch: ItemPatch {
                budget_usd: Some(1.0),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        reply,
        archon_core::protocol::Reply::ItemBudgetsOff(_)
    ));
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    assert!(aether.received_named("create_agent")[1]["config"]["cost_budget"].is_null());
    usage_event(
        &h,
        &stage,
        json!({"input_tokens": 100, "output_tokens": 20, "cost_complete": false}),
    )
    .await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    usage_event(&h, &stage, json!({"cost_usd": 5.0, "cost_complete": true})).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    assert!(aether.received_named("cancel").is_empty());
    let spent = h.ctx.store.lock().await.spend_for_item(&item.id, true);
    assert_eq!(spent.unwrap(), 5.0);
}

#[tokio::test]
async fn a_budget_decision_is_persisted_before_create_and_replayed_after_capabilities_change() {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    script_fleet(&aether);
    script_budget_models(&aether);
    // The node succeeds, but the stage's acknowledgement is lost.
    aether.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(&node_id(1), None)),
    );
    aether.script("create_agent", Scripted::Silent);
    let mut config = Config::default();
    config.limits.budget_usd_per_item = 2.0;
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let h = harness_with_config(&aether, options, config).await;
    let item = h.enqueue("persist the cap").await;
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    aether.set_default("providers", Scripted::reply("providers", json!([])));
    aether.set_default("list_models", Scripted::reply("models", json!([])));
    h.dispatch().await;
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.len(), 3);
    let mut original = creates[1].clone();
    let mut replay = creates[2].clone();
    original.as_object_mut().unwrap().remove("req");
    replay.as_object_mut().unwrap().remove("req");
    assert_eq!(original, replay);
    assert_eq!(replay["config"]["cost_budget"], 2.0);
    assert_eq!(aether.received_named("providers").len(), 1);
    assert_eq!(h.running().await.len(), 1);
}

#[tokio::test]
async fn host_context_is_frozen_for_nodes_and_stage_agents() {
    use archon_core::config::HostContext;
    let claude = "claude-code/claude-haiku-5-5";
    let codex = "codex/gpt-6-luna";
    for (advertised, requested, model, effective) in [
        (true, HostContext::Lean, claude, HostContext::Lean),
        (false, HostContext::Lean, claude, HostContext::Full),
        (true, HostContext::Full, claude, HostContext::Full),
        (true, HostContext::Lean, codex, HostContext::Full),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let aether = start_aether(dir.path()).await;
        if advertised {
            aether.set_features(vec!["host_context".into()]);
        }
        script_fleet(&aether);
        let mut config = Config::default();
        config.aether.host_context = requested;
        let h = harness_with_config(&aether, ClientOptions::default(), config).await;
        let mut plan = common::plan();
        plan.0.get_mut("implement").unwrap()[0] = common::choice(model, Effort::Low);
        let item = h.enqueue_with("record host context", plan).await.unwrap();
        h.dispatch().await;
        let creates = aether.wait_for("create_agent", 2, WAIT).await;
        assert_eq!(creates.len(), 2);
        for create in &creates {
            let sent = create["config"].get("host_context");
            assert_eq!(sent, (!effective.is_full()).then_some(&json!("lean")));
        }
        let run = h.run(&item.id).await;
        let stages = h.ctx.store.lock().await.stage_runs(&run.id).unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].host_context, effective);
        let key = format!("spawn_request:{}", stages[0].id);
        let raw = h.ctx.store.lock().await.kv_get(&key).unwrap().unwrap();
        let frozen: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(frozen["request"]["host_context"], effective.as_str());
        assert_eq!(creates[1]["parent"], node_id(1));
    }
}

async fn pending_lean_creation(
    stage: bool,
    landed: bool,
) -> (
    tempfile::TempDir,
    FakeAether,
    Harness,
    archon_core::queue::QueueItem,
) {
    let dir = tempfile::tempdir().unwrap();
    let aether = start_aether(dir.path()).await;
    aether.set_features(vec!["host_context".into()]);
    let fleet = script_fleet(&aether);
    if stage {
        aether.script(
            "create_agent",
            Scripted::reply("agent", fake::agent(&node_id(1), None)),
        );
    }
    if landed {
        fleet.lock().unwrap().lose_next_reply = true;
    } else {
        aether.script("create_agent", Scripted::Silent);
    }
    let mut config = Config::default();
    config.aether.host_context = archon_core::config::HostContext::Lean;
    let options = ClientOptions {
        request_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let h = harness_with_config(&aether, options, config).await;
    let mut plan = common::plan();
    plan.0.get_mut("implement").unwrap()[0] =
        common::choice("claude-code/claude-haiku-5-5", Effort::Low);
    let item = h
        .enqueue_with("downgrade a pending lean create", plan)
        .await
        .unwrap();
    h.dispatch().await;
    assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.last().unwrap()["config"]["host_context"], "lean");
    aether.set_features(Vec::new());
    let client = AetherClient::connect_with(&aether.endpoint(), options)
        .await
        .unwrap();
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
    (dir, aether, h, item)
}

#[tokio::test]
async fn pending_lean_stage_and_node_creations_fall_back_to_full() {
    for stage_pending in [false, true] {
        let (_dir, aether, h, item) = pending_lean_creation(stage_pending, false).await;
        h.dispatch().await;
        let creates = aether.received_named("create_agent");
        let original = usize::from(stage_pending);
        assert_eq!(
            creates[original]["idempotency_key"],
            creates[original + 1]["idempotency_key"]
        );
        assert!(
            creates[original + 1]["config"]
                .get("host_context")
                .is_none()
        );
        let stage = &h.stages(&item.id).await[0];
        assert_eq!(stage.state, StageState::Running);
        assert!(stage.host_context.is_full());
        let raw = h.kv(&format!("spawn_request:{}", stage.id)).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["request"]["host_context"],
            "full"
        );
        let run = h.run(&item.id).await;
        let raw = h.kv(&format!("run_node:{}", run.id)).await.unwrap();
        let node: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            node["request"]["host_context"],
            if stage_pending { "lean" } else { "full" }
        );
    }
}

#[tokio::test]
async fn aether_rejecting_a_changed_lean_replay_refuses_stages_and_replaces_nodes() {
    for stage_pending in [false, true] {
        let (_dir, aether, h, item) = pending_lean_creation(stage_pending, true).await;
        h.dispatch().await;
        let creates = aether.received_named("create_agent");
        let replay = usize::from(stage_pending) + 1;
        assert_eq!(
            creates[replay - 1]["idempotency_key"],
            creates[replay]["idempotency_key"]
        );
        assert!(creates[replay]["config"].get("host_context").is_none());
        let stage = &h.stages(&item.id).await[0];
        assert!(stage.host_context.is_full());
        if stage_pending {
            let StageState::Failed { error } = &stage.state else {
                panic!("stage must be refused")
            };
            assert!(error.contains("a different request needs a new key"));
            assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
            h.dispatch().await;
            assert_eq!(aether.received_named("create_agent").len(), 3);
        } else {
            assert_eq!(stage.state, StageState::Running);
            let run = h.run(&item.id).await;
            assert_eq!(creates[2]["idempotency_key"], format!("{}:node:2", run.id));
            assert_eq!(creates[3]["parent"], node_id(2));
        }
    }
}

#[tokio::test]
async fn retiring_a_pending_lean_node_falls_back_instead_of_waiting_forever() {
    let (_dir, aether, h, item) = pending_lean_creation(false, false).await;
    executor::cancel_item(&h.ctx, &item.id, "cancel pending run")
        .await
        .unwrap();
    let run = h.run(&item.id).await;
    assert!(h.kv(&format!("run_node:{}", run.id)).await.is_none());
    let creates = aether.received_named("create_agent");
    assert_eq!(creates.len(), 2);
    assert!(creates[1]["config"].get("host_context").is_none());
    assert_eq!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    assert_eq!(aether.received_named("cancel")[0]["agent"], node_id(1));
}

#[tokio::test]
async fn a_stored_agent_context_mismatch_is_refused_without_panicking() {
    let (_dir, _aether, h, item) = pending_lean_creation(true, false).await;
    let mut stage = h.stages(&item.id).await.remove(0);
    stage.aether_agent = Some(AetherAgentId::parse("agt_already_created").unwrap());
    h.ctx.store.lock().await.upsert_stage_run(&stage).unwrap();
    h.dispatch().await;
    let StageState::Failed { error } = &h.stages(&item.id).await[0].state else {
        panic!("a stored context mismatch must be refused")
    };
    assert!(error.contains("cannot change its agent's host context"));
    assert_eq!(
        h.stages(&item.id).await[0].host_context,
        archon_core::config::HostContext::Lean
    );
}

#[tokio::test]
async fn downgraded_requests_stay_full_when_host_context_support_returns() {
    for stage_pending in [false, true] {
        let (_dir, aether, h, item) = pending_lean_creation(stage_pending, false).await;
        aether.script("create_agent", Scripted::Silent);
        h.dispatch().await;
        let before = aether.received_named("create_agent");
        let mut full = before.last().unwrap().clone();
        assert!(full["config"].get("host_context").is_none());
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Spawning);
        aether.set_features(vec!["host_context".into()]);
        let options = ClientOptions {
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let client = AetherClient::connect_with(&aether.endpoint(), options)
            .await
            .unwrap();
        h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
        h.dispatch().await;
        let after = aether.received_named("create_agent");
        let mut replay = after[before.len()].clone();
        full.as_object_mut().unwrap().remove("req");
        replay.as_object_mut().unwrap().remove("req");
        assert_eq!(replay, full);
        assert_eq!(
            h.stages(&item.id).await[0].host_context.is_full(),
            stage_pending
        );
        assert_eq!(h.stages(&item.id).await[0].state, StageState::Running);
    }
}
