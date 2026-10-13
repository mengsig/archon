//! `AetherBackend` and the typed commands against the fake daemon:
//! idempotent stage creation, task outcome mapping, and resubscribing from
//! a cursor after the connection drops.

mod common;

use std::sync::Arc;

use archon_aether::fake::{self, Scripted};
use archon_aether::wire::EventQuery;
use archon_aether::{AetherBackend, AetherLink};
use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::config::ModelsConfig;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelChoice};
use archon_core::policy::ModelPolicy;
use archon_core::ports::{
    AgentBackend, DescendantState, NetworkPolicy, RunNodeRequest, SpawnRequest,
};
use archon_core::run::StageOutcome;
use common::{AGENT, RUN, TASK, WAIT, completed, connect};

/// The run node the stage request names as its parent.
const NODE: &str = "agt_01BX5ZZKBKACTAV9WEVGEMMVRZ";
use serde_json::{Value, json};

fn spawn_request() -> SpawnRequest {
    SpawnRequest {
        idempotency_key: "stg_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        role: "archon-stage:quick-pr/implement".into(),
        name: "implement · the thing".into(),
        objective: "implement the thing".into(),
        instructions: "stage contract".into(),
        choice: ModelChoice {
            model: "claude-code/claude-opus-5-5".parse().unwrap(),
            effort: Effort::High,
        },
        workspace: "/work/tree".into(),
        repository: Some("repo".into()),
        labels: [("archon.item".to_owned(), "qi_1".to_owned())].into(),
        hidden_tools: vec!["mcp__archon__enqueue".into()],
        leaf: true,
        cost_budget_usd: Some(5.0),
        aether_cost_budget: None,
        deadline: None,
        attach_run: None,
        parent: Some(AetherAgentId::parse(NODE).unwrap()),
        network: Some(NetworkPolicy::Denied),
    }
}

fn node_request() -> RunNodeRequest {
    RunNodeRequest {
        idempotency_key: "prun_01ARZ3NDEKTSV4RRFFQ69G5FAV:node:1".into(),
        role: "archon-run:quick-pr".into(),
        name: "the thing".into(),
        model: "claude-code/claude-opus-5-5".parse().unwrap(),
        workspace: "/work/tree".into(),
        repository: Some("repo".into()),
        network: NetworkPolicy::Denied,
        labels: [("archon.item".to_owned(), "qi_1".to_owned())].into(),
        attach_run: None,
    }
}

async fn fake_aether() -> (tempfile::TempDir, fake::FakeAether) {
    let (dir, fake) = common::fake_aether().await;
    fake.set_default(
        "list_models",
        fake::Scripted::reply(
            "models",
            json!([{
                "model": {"provider": "claude-code", "model": "claude-opus-5-5"},
                "capabilities": {"cost": null},
            }]),
        ),
    );
    fake.set_default(
        "providers",
        fake::Scripted::reply(
            "providers",
            json!([
                {"provider": "claude-code", "protocol": "claude-code", "engine": true},
                {"provider": "codex", "protocol": "codex", "engine": true},
            ]),
        ),
    );
    (dir, fake)
}

async fn backend(fake: &fake::FakeAether, policy: ModelPolicy) -> AetherBackend {
    let link = Arc::new(AetherLink::new());
    link.replace(Some(Arc::new(connect(fake).await))).unwrap();
    AetherBackend::new(link, policy)
}

fn any_model() -> ModelPolicy {
    ModelsConfig::default().policy()
}

#[tokio::test]
async fn budgeted_codex_stage_spawns_without_an_unenforceable_aether_cost_budget() {
    let (_dir, fake) = fake_aether().await;
    // Use the model-list shape from the checked-in protocol schema. Archon's
    // current ModelInfo drops the capabilities supplied by Aether.
    fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            json!([{
                "model": {"provider": "codex", "model": "gpt-6-luna"},
                "name": "GPT-6 Luna", "provider_name": "Codex",
                "configured": true, "is_default": true,
                "capabilities": {
                    "context_window": 200_000, "max_output_tokens": 32_000,
                    "cost": null,
                },
            }]),
        ),
    );
    fake.set_responder(
        "create_agent",
        Arc::new(|frame| {
            assert_eq!(frame["model"]["provider"], "codex");
            assert_eq!(frame["model"]["model"], "gpt-6-luna");
            // Match Aether's check_cost_budget refusal for a provider that
            // reports no dollar cost; a create without a cap is accepted.
            if !frame["config"]["cost_budget"].is_null() {
                return Scripted::refuse(
                    "invalid_argument",
                    "provider codex reports no dollar cost, so a cost budget on it could never be enforced; set token_budget instead",
                );
            }
            Scripted::reply("agent", fake::agent(AGENT, Some(TASK)))
        }),
    );
    fake.set_default(
        "get_task",
        Scripted::reply(
            "task",
            fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"})),
        ),
    );
    let backend = backend(&fake, any_model()).await;
    let mut request = spawn_request();
    request.choice.model = "codex/gpt-6-luna".parse().unwrap();
    request.choice.effort = Effort::Low;
    request.cost_budget_usd = None;
    let uncapped = backend.spawn(request.clone()).await.unwrap();

    request.idempotency_key.push_str(":budgeted");
    request.cost_budget_usd = Some(2.0);
    let budgeted = backend.spawn(request).await;
    assert!(
        budgeted.is_ok(),
        "a $2 Archon item budget must not cause an unenforceable Aether cost_budget: {budgeted:?}"
    );
    assert_eq!(budgeted.unwrap(), uncapped);
    let creates = fake.received_named("create_agent");
    assert_eq!(creates.len(), 2);
    assert!(creates[1]["config"]["cost_budget"].is_null());
}

#[tokio::test]
async fn spawn_sends_the_keyed_create_and_reads_task_and_run() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
    );
    let task = fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"}));
    fake.set_default("get_task", Scripted::reply("task", task));
    let backend = backend(&fake, any_model()).await;

    let spawned = backend.spawn(spawn_request()).await.unwrap();
    assert_eq!(spawned.agent.as_str(), AGENT);
    assert_eq!(spawned.task.as_str(), TASK);
    assert_eq!(spawned.run.as_str(), RUN);

    // A retry after a lost acknowledgement sends the same key again.
    let again = backend.spawn(spawn_request()).await.unwrap();
    assert_eq!(again, spawned);
    let creates = fake.received_named("create_agent");
    assert_eq!(creates.len(), 2);
    assert_eq!(
        creates[0]["idempotency_key"],
        "stg_01ARZ3NDEKTSV4RRFFQ69G5FAV"
    );
    assert_eq!(creates[0], creates[1].clone().with_req(&creates[0]));
    assert_create_shape(&creates[0]);
    assert_eq!(fake.received_named("get_task")[0]["id"], TASK);
}

/// The persisted network is what the frame states, whatever Aether's
/// default is now: a child may not widen its node, and a replay under the
/// key must repeat the frame.
#[tokio::test]
async fn spawn_states_the_persisted_network_without_asking_aether() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "allowed"})),
    );
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
    );
    let task = fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"}));
    fake.set_default("get_task", Scripted::reply("task", task));
    let backend = backend(&fake, any_model()).await;

    backend.spawn(spawn_request()).await.unwrap();
    let create = &fake.received_named("create_agent")[0];
    assert_eq!(create["scope"]["network"], "denied");
    assert!(fake.received_named("status").is_empty());
}

/// A record from before the network was persisted is sent as that daemon
/// sent it: with Aether's default of the moment.
#[tokio::test]
async fn a_request_without_a_persisted_network_states_aethers_default() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
    );
    let task = fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"}));
    fake.set_default("get_task", Scripted::reply("task", task));
    let backend = backend(&fake, any_model()).await;

    let mut legacy = spawn_request();
    legacy.network = None;
    backend.spawn(legacy).await.unwrap();
    let create = &fake.received_named("create_agent")[0];
    assert_eq!(create["scope"]["network"], "denied");
    assert_eq!(fake.received_named("status").len(), 1);
}

#[tokio::test]
async fn network_default_reads_aethers_status() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    fake.script("status", Scripted::reply("status", json!({})));
    let backend = backend(&fake, any_model()).await;
    assert_eq!(
        backend.network_default().await.unwrap(),
        NetworkPolicy::Denied
    );
    assert_eq!(
        backend.network_default().await.unwrap(),
        NetworkPolicy::Allowed,
        "a daemon predating the field ships allowed"
    );
}

fn assert_create_shape(create: &Value) {
    assert_eq!(create["role"], "archon-stage:quick-pr/implement");
    assert_eq!(create["objective"], "implement the thing");
    assert_eq!(create["start"], true);
    assert_eq!(
        create["parent"], NODE,
        "stage agents sit under the run node"
    );
    assert_eq!(create["name"], "implement · the thing");
    assert_eq!(
        create["model"],
        json!({"provider": "claude-code", "model": "claude-opus-5-5"})
    );
    assert_eq!(create["scope"]["workspace_root"], "/work/tree");
    assert_eq!(create["scope"]["labels"]["archon.item"], "qi_1");
    // Aether denies network to an explicit scope that does not state it;
    // the request states what it persisted.
    assert_eq!(create["scope"]["network"], "denied");
    assert_eq!(create["approval"], json!({"require_for": []}));
    let config = &create["config"];
    assert_eq!(config["instructions"], "stage contract");
    assert_eq!(config["effort"], "high");
    assert_eq!(config["leaf"], true);
    assert_eq!(config["cost_budget"], 5.0);
    assert_eq!(
        config["tools"],
        json!({"mode": "except", "names": ["mcp__archon__enqueue"]})
    );
}

trait WithReq {
    fn with_req(self, other: &Value) -> Value;
}

impl WithReq for Value {
    /// The same frame under `other`'s request id, to compare bodies.
    fn with_req(mut self, other: &Value) -> Value {
        self["req"] = other["req"].clone();
        self
    }
}

#[tokio::test]
async fn spawn_refuses_a_disallowed_model_without_calling_aether() {
    let (_dir, fake) = fake_aether().await;
    let only_codex = ModelPolicy::from_lists(&["codex/astra".parse().unwrap()], &[], None);
    let backend = backend(&fake, only_codex).await;
    let error = backend.spawn(spawn_request()).await.unwrap_err();
    assert!(matches!(error, ArchonError::Policy(_)));
    assert!(fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn spawn_fails_loudly_when_aether_makes_no_task() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, None)),
    );
    let backend = backend(&fake, any_model()).await;
    let error = backend.spawn(spawn_request()).await.unwrap_err();
    assert!(error.to_string().contains("without a task"), "{error}");
    assert!(fake.received_named("get_task").is_empty());
}

#[tokio::test]
async fn run_nodes_are_created_passive_and_keyed() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(NODE, None)),
    );
    let backend = backend(&fake, any_model()).await;
    let node = backend.create_run_node(node_request()).await.unwrap();
    assert_eq!(node.agent.as_str(), NODE);
    assert_eq!(node.run.as_str(), fake::FAKE_RUN, "the run its stages join");
    let create = &fake.received_named("create_agent")[0];
    assert_eq!(
        create["idempotency_key"],
        "prun_01ARZ3NDEKTSV4RRFFQ69G5FAV:node:1"
    );
    assert_eq!(create["role"], "archon-run:quick-pr");
    assert_eq!(create["name"], "the thing");
    assert_eq!(create["start"], false);
    assert!(create.get("objective").is_none() && create.get("parent").is_none());
    assert_eq!(create["scope"]["workspace_root"], "/work/tree");
    assert_eq!(
        create["scope"]["network"], "denied",
        "the request's, not the fake's `allowed` default"
    );
    assert_eq!(create["approval"], json!({"require_for": []}));
    assert_eq!(create["config"]["leaf"], false);
    assert_eq!(create["config"]["tools"]["mode"], "except");
    assert!(
        fake.received_named("get_task").is_empty(),
        "a node has no task"
    );
    assert!(fake.received_named("status").is_empty());
}

/// Aether's default changing between a node's lost reply and its replay
/// must not change the frame: the key would be refused and the node lost.
#[tokio::test]
async fn a_replayed_node_create_repeats_the_frame_after_the_default_changed() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "denied"})),
    );
    fake.script("create_agent", Scripted::Silent);
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(NODE, None)),
    );
    let link = Arc::new(AetherLink::new());
    let options = archon_aether::client::ClientOptions {
        request_timeout: std::time::Duration::from_millis(200),
        ..Default::default()
    };
    let client = archon_aether::client::AetherClient::connect_with(&fake.endpoint(), options)
        .await
        .unwrap();
    link.replace(Some(Arc::new(client))).unwrap();
    let backend = AetherBackend::new(link, any_model());

    let lost = backend.create_run_node(node_request()).await.unwrap_err();
    assert!(matches!(lost, ArchonError::Unavailable(_)), "{lost}");
    fake.set_default(
        "status",
        Scripted::reply("status", json!({"network_default": "allowed"})),
    );
    let node = backend.create_run_node(node_request()).await.unwrap();
    assert_eq!(node.agent.as_str(), NODE);
    let creates = fake.received_named("create_agent");
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[0], creates[1].clone().with_req(&creates[0]));
    assert_eq!(creates[1]["scope"]["network"], "denied");
}

#[tokio::test]
async fn a_run_node_given_a_task_is_a_protocol_error() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "create_agent",
        Scripted::reply("agent", fake::agent(NODE, Some(TASK))),
    );
    let backend = backend(&fake, any_model()).await;
    let error = backend.create_run_node(node_request()).await.unwrap_err();
    assert!(matches!(error, ArchonError::Protocol(_)), "{error}");
    assert!(error.to_string().contains("passive"), "{error}");
}

#[tokio::test]
async fn a_run_node_without_a_run_is_a_protocol_error() {
    let (_dir, fake) = fake_aether().await;
    let mut runless = fake::agent(NODE, None);
    runless.as_object_mut().unwrap().remove("run");
    fake.script("create_agent", Scripted::reply("agent", runless));
    let backend = backend(&fake, any_model()).await;
    let error = backend.create_run_node(node_request()).await.unwrap_err();
    assert!(matches!(error, ArchonError::Protocol(_)), "{error}");
    assert!(error.to_string().contains("has no run"), "{error}");
}

#[tokio::test]
async fn is_live_reads_the_agent_state_and_treats_unknown_as_dead() {
    let (_dir, fake) = fake_aether().await;
    let mut cancelled = fake::agent(NODE, None);
    cancelled["status"] = json!({"state": "cancelled", "reason": "done"});
    fake.script(
        "get_agent",
        Scripted::reply("agent", fake::agent(NODE, None)),
    );
    fake.script("get_agent", Scripted::reply("agent", cancelled));
    fake.script("get_agent", Scripted::refuse("not_found", "no such agent"));
    fake.script("get_agent", Scripted::refuse("invalid_argument", "bad id"));
    let backend = backend(&fake, any_model()).await;
    let node = AetherAgentId::parse(NODE).unwrap();
    assert!(backend.is_live(&node).await.unwrap(), "ready is live");
    assert!(!backend.is_live(&node).await.unwrap(), "cancelled is dead");
    assert!(!backend.is_live(&node).await.unwrap(), "unknown is dead");
    let error = backend.is_live(&node).await.unwrap_err();
    assert!(!matches!(error, ArchonError::NotFound(_)), "{error}");
}

/// The stop of a lead's helpers waits on this: the whole run is read once,
/// archived agents included, and only what sits beneath the agent is returned.
#[tokio::test]
async fn descendants_lists_the_agents_run_once_and_keeps_only_its_subtree() {
    let (_dir, fake) = fake_aether().await;
    let held = |id: &str, parent: &str, state: &str| {
        let mut agent = fake::agent(id, None);
        agent["parent"] = json!(parent);
        agent["status"] = json!({"state": state});
        agent["config"] = json!({"effort": "x_high"});
        agent
    };
    let listed = json!([
        fake::agent(NODE, None),
        held(AGENT, NODE, "completed"),
        held("agt_helper", AGENT, "running"),
        held("agt_other_stage", NODE, "running"),
    ]);
    fake.script(
        "get_agent",
        Scripted::reply("agent", fake::agent(AGENT, None)),
    );
    fake.script("list_agents", Scripted::reply("agents", listed));
    fake.script("get_agent", Scripted::refuse("not_found", "no such agent"));
    let backend = backend(&fake, any_model()).await;
    let lead = AetherAgentId::parse(AGENT).unwrap();

    let found = backend.descendants(&lead).await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].agent.as_str(), "agt_helper");
    assert_eq!(found[0].parent, lead);
    assert_eq!(found[0].state, DescendantState::Live);
    assert_eq!(found[0].effort, Some(Effort::XHigh));
    let query = &fake.received_named("list_agents")[0]["query"];
    assert_eq!(query["run"], fake::FAKE_RUN);
    assert_eq!(query["include_archived"], true);
    assert!(query.get("parent").is_none(), "one read, not one per agent");

    let forgotten = backend.descendants(&lead).await.unwrap();
    assert!(forgotten.is_empty(), "a forgotten agent left nothing");
    assert_eq!(fake.received_named("list_agents").len(), 1);
}

#[tokio::test]
async fn spawn_without_a_connection_is_unavailable() {
    let backend = AetherBackend::new(Arc::new(AetherLink::new()), any_model());
    let error = backend.spawn(spawn_request()).await.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)));
}

#[tokio::test]
async fn task_outcomes_map_every_settled_state() {
    let (_dir, fake) = fake_aether().await;
    let statuses = [
        json!({"state": "active"}),
        json!({"state": "blocked", "reason": "waiting on a child"}),
        json!({"state": "completed", "result": {"summary": "VERDICT: PASS", "artifacts": [],
            "structured": {"verdict": "pass"}}}),
        json!({"state": "failed", "error": "tests failed"}),
        json!({"state": "cancelled", "reason": "deadline"}),
    ];
    for status in statuses {
        fake.script(
            "get_task",
            Scripted::reply("task", fake::task(TASK, Some(AGENT), RUN, status)),
        );
    }
    let backend = backend(&fake, any_model()).await;
    let task = AetherTaskId::parse(TASK).unwrap();
    let mut outcomes = Vec::new();
    for _ in 0..5 {
        outcomes.push(backend.task_outcome(&task).await.unwrap());
    }
    assert_eq!(outcomes[0], None);
    assert_eq!(outcomes[1], None, "blocked is still open");
    let Some(StageOutcome::Completed {
        summary,
        structured,
    }) = &outcomes[2]
    else {
        panic!("expected completed, got {:?}", outcomes[2]);
    };
    assert_eq!(summary, "VERDICT: PASS");
    assert_eq!(structured.as_ref().unwrap()["verdict"], "pass");
    assert_eq!(
        outcomes[3],
        Some(StageOutcome::Failed {
            error: "tests failed".into(),
            infra: false
        })
    );
    assert_eq!(
        outcomes[4],
        Some(StageOutcome::Cancelled {
            reason: Some("deadline".into())
        })
    );
}

#[tokio::test]
async fn typed_commands_send_what_architecture_4_3_says() {
    let (_dir, fake) = fake_aether().await;
    let sent = json!({"message": "msg_1"});
    fake.set_default(
        "send_message",
        Scripted::reply("message_sent", sent.clone()),
    );
    fake.set_default("reply_to_question", Scripted::reply("message_sent", sent));
    fake.set_default("trigger", Scripted::ok());
    fake.set_default("cancel", Scripted::ok());
    fake.set_default(
        "pause",
        Scripted::reply("paused", json!({"agents": [], "mid_turn": []})),
    );
    fake.set_default("resume", Scripted::reply("agents", json!([])));
    fake.set_default("mcp_servers", Scripted::reply("mcp_servers", json!([])));
    let client = connect(&fake).await;
    let agent = AetherAgentId::parse(AGENT).unwrap();
    client.send_message(&agent, "hello", "fd_1").await.unwrap();
    client.reply_to_question(&agent, 7, "yes").await.unwrap();
    client
        .trigger(&agent, json!({"kind": "news"}), "tr_1")
        .await
        .unwrap();
    client.cancel(&agent, "operator").await.unwrap();
    client.pause(&agent, true).await.unwrap();
    client.pause(&agent, false).await.unwrap();
    client.refresh_mcp("archon").await.unwrap();

    let got = |cmd: &str| fake.received_named(cmd).remove(0);
    let message = got("send_message");
    assert_eq!(
        (&message["idempotency_key"], &message["subtree"]),
        (&json!("fd_1"), &json!(false))
    );
    let answer = got("reply_to_question");
    assert_eq!(answer["question_seq"], 7);
    assert_eq!(
        answer["idempotency_key"],
        format!("archon-answer-{AGENT}-7"),
        "keyed per question so a retried answer is not refused as stale"
    );
    let trigger = got("trigger");
    assert_eq!(
        (&trigger["source"], &trigger["idempotency_key"]),
        (&json!("archon"), &json!("tr_1"))
    );
    assert_eq!(got("cancel")["reason"], "operator");
    assert_eq!(got("pause")["subtree"], true);
    assert_eq!(got("resume")["subtree"], true);
    let mcp = got("mcp_servers");
    assert_eq!(
        (&mcp["refresh"], &mcp["server"]),
        (&json!(true), &json!("archon"))
    );
}

#[tokio::test]
async fn list_models_reads_aethers_nested_model_ref() {
    let (_dir, fake) = fake_aether().await;
    let models = json!([{
        "model": {"provider": "claude-code", "model": "claude-opus-5-5"},
        "name": "Opus 5.5", "provider_name": "Claude Code", "capabilities": {},
        "configured": true, "is_default": true
    }]);
    fake.script("list_models", Scripted::reply("models", models));
    let client = connect(&fake).await;
    let listed = client.list_models().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].model.to_string(), "claude-code/claude-opus-5-5");
    assert!(listed[0].configured && listed[0].is_default);
}

#[tokio::test]
async fn restart_must_return_a_successor() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "restart",
        Scripted::reply("agent", fake::agent(AGENT, None)),
    );
    fake.script(
        "restart",
        Scripted::reply("agent", fake::agent(common::AGENT_2, None)),
    );
    let client = connect(&fake).await;
    let agent = AetherAgentId::parse(AGENT).unwrap();
    assert!(matches!(
        client.restart(&agent).await,
        Err(ArchonError::Protocol(_))
    ));
    assert_eq!(
        client.restart(&agent).await.unwrap().id.as_str(),
        common::AGENT_2
    );
}

#[tokio::test]
async fn a_dropped_connection_resumes_from_the_cursor_on_a_new_one() {
    let (_dir, fake) = fake_aether().await;
    let first = connect(&fake).await;
    let mut sub = first.subscribe(None, EventQuery::default()).await.unwrap();
    fake.push_event(completed("one"));
    fake.push_event(completed("two"));
    let mut cursor = 0;
    for _ in 0..2 {
        cursor = sub.next().await.unwrap().seq;
    }
    fake.hangup(Some(("unavailable", "aetherd is shutting down")));
    let error = sub.next().await.unwrap_err().to_string();
    assert!(error.contains("shutting down"), "{error}");

    // Missed while disconnected; the daemon also restarted.
    fake.push_event(completed("three"));
    fake.push_event(completed("four"));
    fake.set_instance_id("fake-instance-2");
    let second = connect(&fake).await;
    assert_ne!(second.welcome().instance_id, first.welcome().instance_id);
    let mut resumed = second
        .subscribe(Some(cursor), EventQuery::default())
        .await
        .unwrap();
    assert_eq!(resumed.last_seq, cursor);
    let mut summaries = Vec::new();
    for _ in 0..2 {
        let event = resumed.next().await.unwrap();
        let Some(StageOutcome::Completed { summary, .. }) = event.kind.stage_outcome() else {
            panic!("expected task.completed, got {:?}", event.kind);
        };
        summaries.push((event.seq, summary));
    }
    assert_eq!(summaries, [(3, "three".to_owned()), (4, "four".to_owned())]);
    assert_eq!(fake.connections_accepted(), 2);
    tokio::time::timeout(WAIT, second.ping())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn hard_cost_budgets_follow_capabilities_and_protocols_not_provider_names() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "get_task",
        Scripted::reply(
            "task",
            fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"})),
        ),
    );
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
    );
    let backend = backend(&fake, any_model()).await;
    for (protocol, engine, cost, expected) in [
        ("codex", true, json!({"input_per_mtok": 1.0}), false),
        ("claude-code", true, Value::Null, true),
        (
            "openai-responses",
            false,
            json!({"input_per_mtok": 1.0}),
            true,
        ),
        ("openai-responses", false, Value::Null, false),
        ("future-engine", true, json!({"input_per_mtok": 1.0}), false),
    ] {
        fake.set_default(
            "providers",
            Scripted::reply(
                "providers",
                json!([{
                    "provider": "custom", "protocol": protocol, "engine": engine,
                }]),
            ),
        );
        fake.set_default(
            "list_models",
            Scripted::reply(
                "models",
                json!([{
                    "model": {"provider": "custom", "model": "canonical"},
                    "aliases": ["alias"], "capabilities": {"cost": cost},
                }]),
            ),
        );
        let mut req = spawn_request();
        req.choice.model = "custom/alias".parse().unwrap();
        req.idempotency_key = format!("{protocol}:{expected}");
        backend.spawn(req).await.unwrap();
        let creates = fake.received_named("create_agent");
        let create = creates.last().unwrap();
        assert_eq!(
            create["config"]["cost_budget"],
            if expected { json!(5.0) } else { Value::Null }
        );
        // Archon always states the primary. Aether inherits a parent's
        // fallback chain only when the primary is inherited too.
        assert_eq!(create["model"]["model"], "alias");
        assert!(create.get("fallbacks").is_none());
        assert!(fake.received_named("get_agent").is_empty());
    }
}

#[tokio::test]
async fn unknown_budget_capabilities_require_estimates_and_never_send_a_hard_cap() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "list_models",
        Scripted::reply(
            "models",
            json!([{
                "model": {"provider": "custom", "model": "unpriced"},
            }]),
        ),
    );
    fake.set_default("providers", Scripted::reply("providers", json!([])));
    let backend = backend(&fake, any_model()).await;
    let mut req = spawn_request();
    req.choice.model = "custom/unpriced".parse().unwrap();
    let error = backend.prepare_spawn(&mut req, false).await.unwrap_err();
    assert!(matches!(error, ArchonError::Policy(_)));
    assert!(
        error
            .to_string()
            .contains("count_estimated_cost is disabled")
    );
    assert!(fake.received_named("create_agent").is_empty());
    backend.prepare_spawn(&mut req, true).await.unwrap();
    assert_eq!(req.aether_cost_budget, Some(false));
    assert_eq!(req.cost_budget_usd, Some(5.0), "keep Archon's grant");
    // A restart that disables estimates must refuse a stored soft cap too.
    let error = backend.prepare_spawn(&mut req, false).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("count_estimated_cost is disabled")
    );
}

#[tokio::test]
async fn an_unbudgeted_spawn_does_not_query_capabilities() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default(
        "create_agent",
        Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
    );
    fake.set_default(
        "get_task",
        Scripted::reply(
            "task",
            fake::task(TASK, Some(AGENT), RUN, json!({"state": "assigned"})),
        ),
    );
    let backend = backend(&fake, any_model()).await;
    let mut req = spawn_request();
    req.cost_budget_usd = None;
    backend.spawn(req).await.unwrap();
    assert!(fake.received_named("list_models").is_empty());
    assert!(fake.received_named("providers").is_empty());
}
