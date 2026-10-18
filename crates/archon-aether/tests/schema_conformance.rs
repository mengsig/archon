//! Shared commands match Aether v17/v15 schemas; optional memory and recovery
//! commands match their published schema subsets, including nested fields.

use std::collections::BTreeSet;

use archon_aether::github::{RepositoryConfig, WatcherConfig};
use archon_aether::memory::MemoryQuery;
use archon_aether::wire::{
    AgentConfig, AgentPatch, AgentQuery, ApprovalPolicy, ClientFrame, Command, CreateAgent,
    EventQuery, Hello, NetworkPolicy, Scope, ToolExposure,
};
use archon_core::aether_ids::{AetherAgentId, AetherMemoryId, AetherRunId, AetherTaskId};
use archon_core::model::Effort;
use serde_json::{Value, json};

/// v17 + host_context feature (operator logins and opt-in host context).
/// Vendored schemas omit issue numbers from Aether's descriptions.
const SCHEMA: &str = include_str!("fixtures/aether-protocol-v17.schema.json");
const SCHEMA_V15: &str = include_str!("fixtures/aether-protocol-v15.schema.json");

fn schema() -> Value {
    serde_json::from_str(SCHEMA).expect("the vendored schema is JSON")
}

fn schema_v15() -> Value {
    serde_json::from_str(SCHEMA_V15).expect("the vendored v15 schema is JSON")
}

fn agent() -> AetherAgentId {
    AetherAgentId::parse("agt_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn task() -> AetherTaskId {
    AetherTaskId::parse("tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn memory() -> AetherMemoryId {
    AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

fn run() -> AetherRunId {
    AetherRunId::parse("run_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

/// Forces this test to be revisited when a variant is added: no wildcard.
fn variant_name(command: &Command) -> &'static str {
    match command {
        Command::Hello(_) => "hello",
        Command::Status => "status",
        Command::Ping => "ping",
        Command::CreateAgent(_) => "create_agent",
        Command::GetAgent { .. } => "get_agent",
        Command::ListAgents { .. } => "list_agents",
        Command::UpdateAgent { .. } => "update_agent",
        Command::SendMessage { .. } => "send_message",
        Command::ReplyToQuestion { .. } => "reply_to_question",
        Command::Trigger { .. } => "trigger",
        Command::AppendNote { .. } => "append_note",
        Command::Cancel { .. } => "cancel",
        Command::Interrupt { .. } => "interrupt",
        Command::Wake { .. } => "wake",
        Command::Restart { .. } => "restart",
        Command::Pause { .. } => "pause",
        Command::Resume { .. } => "resume",
        Command::GetTask { .. } => "get_task",
        Command::Subscribe { .. } => "subscribe",
        Command::Unsubscribe { .. } => "unsubscribe",
        Command::GetEvents { .. } => "get_events",
        Command::ListModels => "list_models",
        Command::Providers => "providers",
        Command::McpServers { .. } => "mcp_servers",
        Command::GetGithubWatcher => "get_github_watcher",
        Command::ConfigureGithubWatcher { .. } => "configure_github_watcher",
        Command::Memories { .. } => "memories",
        Command::ForgetMemory { .. } => "forget_memory",
        Command::RestoreMemory { .. } => "restore_memory",
        Command::MemoriesInContext { .. } => "memories_in_context",
        Command::RecordMemoryFeedback(_) => "record_memory_feedback",
        Command::RetractMemoryFeedback { .. } => "retract_memory_feedback",
        Command::ResolveSubmission { .. } => "resolve_submission",
    }
}

fn full_create_agent() -> CreateAgent {
    CreateAgent {
        role: "archon-stage:direct/implement".into(),
        name: Some("implement · the thing".into()),
        objective: Some("do it".into()),
        model: Some("claude-code/claude-opus-5-5".parse().unwrap()),
        scope: Some(Scope {
            workspace_root: Some("/work/tree".into()),
            repository: Some("repo".into()),
            network: NetworkPolicy::Allowed,
            labels: [("archon.item".to_owned(), "qi_1".to_owned())].into(),
        }),
        repository_root: None,
        parent: Some(agent()),
        config: Some(AgentConfig {
            host_context: Default::default(),
            instructions: Some("contract".into()),
            effort: Some(Effort::High),
            cost_budget: Some(5.0),
            deadline: Some("2026-09-26T12:00:00Z".parse().unwrap()),
            tools: ToolExposure::Except {
                names: ["mcp__archon__enqueue".to_owned()].into(),
            },
            long_lived: false,
            leaf: true,
        }),
        approval: Some(ApprovalPolicy::none()),
        run: Some(run()),
        start: true,
        idempotency_key: Some("stg_01".into()),
    }
}

fn full_event_query() -> EventQuery {
    EventQuery {
        agent: Some(agent()),
        task: Some(task()),
        run: Some(run()),
        families: vec!["task".into()],
        types: vec!["task.completed".into()],
    }
}

fn watcher_config(repository: RepositoryConfig) -> WatcherConfig {
    WatcherConfig {
        enabled: true,
        mention_regex: "(?i)@archon".into(),
        label_regex: "(?i)archon".into(),
        poll_interval_secs: 120,
        repositories: vec![repository],
        operator_users: None,
        rest: serde_json::Map::new(),
    }
}

/// The save only protocol 17 can carry: a watcher naming operator logins.
fn operator_logins_sample() -> Command {
    Command::ConfigureGithubWatcher {
        config: WatcherConfig {
            operator_users: Some(vec!["octocat".into(), "monalisa".into()]),
            ..watcher_config(RepositoryConfig::fixed("o/r".into(), agent()))
        },
        expected_revision: 5,
    }
}

/// Each variant twice where it has optional fields: fully populated (every
/// key must exist in the schema) and minimal (required keys must be there).
/// Everything here is sent on every protocol Archon speaks.
fn samples() -> Vec<Command> {
    let hello = |token: Option<&str>| {
        Command::Hello(Hello {
            protocol_version: 17,
            client_name: "archon".into(),
            client_version: "0.1.0".into(),
            features: vec![],
            token: token.map(str::to_owned),
        })
    };
    let mut commands = vec![
        hello(None),
        hello(Some("t".repeat(32).as_str())),
        Command::Status,
        Command::Ping,
        Command::CreateAgent(Box::new(full_create_agent())),
        Command::CreateAgent(Box::new(CreateAgent {
            role: "archon-orchestrator".into(),
            ..CreateAgent::default()
        })),
        Command::GetAgent { id: agent() },
        Command::ListAgents {
            query: AgentQuery {
                parent: Some(agent()),
                run: Some(run()),
                statuses: vec!["running".into()],
                live_only: true,
                include_archived: false,
            },
        },
        Command::ListAgents {
            query: AgentQuery::default(),
        },
        Command::UpdateAgent {
            id: agent(),
            patch: AgentPatch {
                model: Some("claude-code/claude-opus-5-5".parse().unwrap()),
                effort: Some(Effort::High),
                cost_budget: Some(3.0),
                deadline: Some(json!({"set": "2026-09-26T12:00:00Z"})),
            },
        },
        Command::UpdateAgent {
            id: agent(),
            patch: AgentPatch::default(),
        },
    ];
    commands.extend(message_samples());
    commands.extend(control_samples());
    commands
}

fn message_samples() -> Vec<Command> {
    vec![
        Command::SendMessage {
            to: agent(),
            content: "hi".into(),
            subtree: false,
            idempotency_key: Some("fd_1".into()),
        },
        Command::SendMessage {
            to: agent(),
            content: "hi".into(),
            subtree: false,
            idempotency_key: None,
        },
        Command::ReplyToQuestion {
            to: agent(),
            question_seq: 9,
            content: "yes".into(),
            idempotency_key: Some("q_1".into()),
        },
        Command::Trigger {
            agent: agent(),
            source: "archon".into(),
            payload: json!({"kind": "item_done"}),
            idempotency_key: Some("tr_1".into()),
        },
        Command::Cancel {
            agent: agent(),
            reason: Some("deadline".into()),
        },
        Command::Cancel {
            agent: agent(),
            reason: None,
        },
        Command::Interrupt { agent: agent() },
    ]
}

fn control_samples() -> Vec<Command> {
    vec![
        Command::Wake { agent: agent() },
        Command::Restart { agent: agent() },
        Command::Pause {
            agent: agent(),
            subtree: true,
        },
        Command::Resume {
            agent: agent(),
            subtree: true,
        },
        Command::GetTask { id: task() },
        Command::Subscribe {
            after: Some(41),
            query: full_event_query(),
            deltas: false,
        },
        Command::Subscribe {
            after: None,
            query: EventQuery::default(),
            deltas: false,
        },
        Command::Unsubscribe { id: 2 },
        Command::GetEvents {
            after: 0,
            limit: 100,
            query: full_event_query(),
        },
        Command::ListModels,
        Command::Providers,
        Command::McpServers {
            refresh: true,
            server: Some("archon".into()),
        },
        Command::McpServers {
            refresh: false,
            server: None,
        },
        Command::GetGithubWatcher,
        Command::ConfigureGithubWatcher {
            config: watcher_config(RepositoryConfig::fixed("o/r".into(), agent())),
            expected_revision: 3,
        },
        Command::ConfigureGithubWatcher {
            config: watcher_config(RepositoryConfig {
                allowed_users: Some(vec!["monalisa".into()]),
                ..RepositoryConfig::fixed("o/r".into(), agent())
            }),
            expected_revision: 4,
        },
        Command::Memories {
            query: MemoryQuery {
                id: memory(),
                include_forgotten: true,
                include_superseded: true,
                limit: 1,
            },
        },
        Command::ForgetMemory {
            id: memory(),
            reason: Some("archon fb_1: stale rule".into()),
        },
        Command::ForgetMemory {
            id: memory(),
            reason: None,
        },
        Command::RestoreMemory { id: memory() },
    ]
}

/// Resolve `$ref` and `anyOf [X, null]` down to the definition that matters.
fn resolve<'a>(schema: &'a Value, node: &'a Value) -> &'a Value {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        let name = reference.strip_prefix("#/$defs/").expect("local reference");
        return resolve(schema, &schema["$defs"][name]);
    }
    if let Some(options) = node.get("anyOf").and_then(Value::as_array) {
        let non_null: Vec<&Value> = options.iter().filter(|o| o["type"] != "null").collect();
        if let [only] = non_null.as_slice() {
            return resolve(schema, only);
        }
    }
    node
}

/// For a tagged `oneOf`, the branch whose `const` properties all match.
fn branch<'a>(schema: &'a Value, node: &'a Value, value: &Value) -> Option<&'a Value> {
    let options = node.get("oneOf")?.as_array()?;
    options.iter().map(|o| resolve(schema, o)).find(|option| {
        let Some(properties) = option.get("properties").and_then(Value::as_object) else {
            return false;
        };
        let consts: Vec<_> = properties
            .iter()
            .filter(|(_, p)| p.get("const").is_some())
            .collect();
        !consts.is_empty()
            && consts
                .iter()
                .all(|(key, p)| value.get(key.as_str()) == Some(&p["const"]))
    })
}

/// Properties and required keys of an object schema, merging a `$ref`
/// sibling (how a flattened newtype variant like `create_agent` is written).
fn shape(schema: &Value, node: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut properties = BTreeSet::new();
    let mut required = BTreeSet::new();
    for part in [node, resolve(schema, node)] {
        if let Some(props) = part.get("properties").and_then(Value::as_object) {
            properties.extend(props.keys().cloned());
        }
        if let Some(list) = part.get("required").and_then(Value::as_array) {
            required.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    (properties, required)
}

fn property<'a>(schema: &'a Value, node: &'a Value, key: &str) -> Option<&'a Value> {
    [node, resolve(schema, node)]
        .into_iter()
        .find_map(|part| part.get("properties")?.get(key))
}

/// Check `value` against the object schema `node`, recursing into nested
/// objects. Returns every violation found, prefixed with its path.
fn check(schema: &Value, node: &Value, value: &Value, path: &str) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let node = branch(schema, resolve(schema, node), value).unwrap_or(node);
    let (properties, required) = shape(schema, node);
    if properties.is_empty() {
        // A map-typed field (labels, payload): its keys are data, not schema.
        return Vec::new();
    }
    let mut problems = Vec::new();
    for key in object.keys() {
        let Some(child) = property(schema, node, key) else {
            problems.push(format!("{path}.{key} is not in the schema"));
            continue;
        };
        let child = resolve(schema, child);
        problems.extend(check(schema, child, &object[key], &format!("{path}.{key}")));
    }
    for key in required.difference(&object.keys().cloned().collect()) {
        problems.push(format!("{path}.{key} is required but missing"));
    }
    problems
}

#[test]
fn the_vendored_schemas_are_protocols_17_and_15() {
    let schema = schema();
    assert_eq!(schema["protocol_version"], 17);
    assert_eq!(schema["min_protocol_version"], 16);
    let older = schema_v15();
    assert_eq!(older["protocol_version"], 15);
    assert_eq!(older["min_protocol_version"], 14);
}

/// How `command`'s frame breaks `schema`; empty when it conforms.
fn violations(schema: &Value, command: &Command) -> Vec<String> {
    let name = variant_name(command);
    let variants = schema["$defs"]["ClientFrame"]["oneOf"]
        .as_array()
        .expect("a tagged union");
    let frame = serde_json::to_value(ClientFrame { req: 1, command }).unwrap();
    assert_eq!(frame["cmd"], name, "serde tag and variant_name agree");
    assert!(frame["req"].is_u64(), "every frame carries req");
    let variant = variants
        .iter()
        .find(|v| v["properties"]["cmd"]["const"] == name)
        .unwrap_or_else(|| panic!("`{name}` is not an Aether command"));
    // The frame's own `req` sits beside the command's fields.
    let mut body = frame;
    body.as_object_mut().unwrap().remove("req");
    check(schema, variant, &body, name)
}

#[test]
fn every_command_archon_emits_matches_both_schemas() {
    for schema in [schema(), schema_v15()] {
        let version = &schema["protocol_version"];
        let mut seen = BTreeSet::new();
        let mut problems = Vec::new();
        for command in samples() {
            seen.insert(variant_name(&command));
            problems.extend(violations(&schema, &command));
        }
        assert!(
            problems.is_empty(),
            "protocol {version} schema violations:\n{}",
            problems.join("\n")
        );
        assert_eq!(
            seen.len(),
            28,
            "every Command variant has a sample: {seen:?}"
        );
    }
}

/// `operator_users` exists from protocol 17 on. The v15 schema has no such
/// key, which is why Archon refuses to send the list below 17.
#[test]
fn operator_logins_conform_to_v17_and_are_unknown_to_v15() {
    let sample = operator_logins_sample();
    let current = violations(&schema(), &sample);
    assert!(current.is_empty(), "{current:?}");
    assert_eq!(
        violations(&schema_v15(), &sample),
        ["configure_github_watcher.config.operator_users is not in the schema"]
    );
    let config = &schema()["$defs"]["GithubWatcherConfig"];
    assert_eq!(
        config["properties"]["operator_users"]["items"]["type"],
        "string"
    );
    let required = config["required"].as_array().cloned().unwrap_or_default();
    assert!(!required.contains(&json!("operator_users")), "optional");
}

#[test]
fn the_checker_catches_unknown_and_missing_keys() {
    let schema = schema();
    let variants = schema["$defs"]["ClientFrame"]["oneOf"].as_array().unwrap();
    let send = variants
        .iter()
        .find(|v| v["properties"]["cmd"]["const"] == "send_message")
        .unwrap();
    let bad = json!({"cmd": "send_message", "to": "agt_1", "text": "hi"});
    let problems = check(&schema, send, &bad, "send_message");
    assert!(
        problems
            .iter()
            .any(|p| p.contains(".text is not in the schema"))
    );
    assert!(problems.iter().any(|p| p.contains(".content is required")));
    let create = variants
        .iter()
        .find(|v| v["properties"]["cmd"]["const"] == "create_agent")
        .unwrap();
    let nested =
        json!({"cmd": "create_agent", "role": "r", "config": {"tools": {"mode": "except"}}});
    let problems = check(&schema, create, &nested, "create_agent");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("config.tools.names is required"))
    );
}

/// Replies are read leniently, so a misnamed field silently stays `None`;
/// every `Agent` key Archon reads must exist in the schema.
#[test]
fn the_agent_fields_archon_reads_exist_in_the_schema() {
    let schema = schema();
    let (properties, _) = shape(&schema, &schema["$defs"]["Agent"]);
    let read = [
        "id",
        "parent",
        "role",
        "name",
        "model",
        "status",
        "paused",
        "current_task",
        "run",
        "restarted_as",
    ];
    for key in read {
        assert!(properties.contains(key), "Agent.{key} is not in the schema");
    }
    let mut value = archon_aether::fake::agent("agt_01ARZ3NDEKTSV4RRFFQ69G5FAV", None);
    value["restarted_as"] = json!("agt_01ARZ3NDEKTSV4RRFFQ69G5FAW");
    let parsed: archon_aether::wire::Agent = serde_json::from_value(value).unwrap();
    assert_eq!(
        parsed.successor.as_ref().map(AetherAgentId::as_str),
        Some("agt_01ARZ3NDEKTSV4RRFFQ69G5FAW")
    );
}

/// The memory record fields and replies curation reads, in both schemas.
#[test]
fn the_memory_fields_archon_reads_exist_in_both_schemas() {
    for schema in [schema(), schema_v15()] {
        let version = &schema["protocol_version"];
        let (properties, required) = shape(&schema, &schema["$defs"]["MemoryRecord"]);
        for key in ["id", "scope", "kind", "provenance", "created_at"] {
            assert!(
                required.contains(key),
                "v{version} MemoryRecord.{key} is required"
            );
        }
        for key in ["forgotten_at", "superseded_by", "pinned"] {
            assert!(
                properties.contains(key),
                "v{version} MemoryRecord.{key} exists"
            );
        }
        let replies = schema["$defs"]["Reply"]["oneOf"].as_array().unwrap();
        let data = |kind: &str| {
            let reply = replies
                .iter()
                .find(|r| r["properties"]["reply"]["const"] == kind)
                .unwrap_or_else(|| panic!("v{version} has no `{kind}` reply"));
            reply["properties"]["data"].clone()
        };
        assert_eq!(data("memory")["$ref"], "#/$defs/MemoryRecord");
        assert_eq!(data("memories")["items"]["$ref"], "#/$defs/MemoryRecord");
        let scope = schema["$defs"]["MemoryScope"]["oneOf"].as_array().unwrap();
        assert_eq!(scope.len(), 6, "v{version}: every scope has a label");
    }
}

/// The values a string schema allows: v15 writes `CostBasis` as one `enum`,
/// v17 as a `oneOf` of an `enum` and a documented `const`.
fn allowed_strings(node: &Value) -> Vec<Value> {
    let options = node.get("oneOf").and_then(Value::as_array);
    let parts: Vec<&Value> = options.map_or_else(|| vec![node], |o| o.iter().collect());
    parts
        .into_iter()
        .flat_map(|part| {
            let listed = part.get("enum").and_then(Value::as_array).cloned();
            listed.unwrap_or_else(|| part.get("const").cloned().into_iter().collect())
        })
        .collect()
}

#[test]
fn usage_fixtures_include_cost_provenance_in_the_vendored_schemas() {
    for schema in [schema(), schema_v15()] {
        for fixture in [
            include_str!("fixtures/usage_list_price.json"),
            include_str!("fixtures/usage_provider_reported.json"),
        ] {
            let event: Value = serde_json::from_str(fixture).unwrap();
            let usage = &event["kind"]["total"];
            let problems = check(&schema, &schema["$defs"]["Usage"], usage, "usage");
            assert!(problems.is_empty(), "{problems:?}");
            let basis = resolve(
                &schema,
                &schema["$defs"]["Usage"]["properties"]["cost_basis"],
            );
            let allowed = allowed_strings(basis);
            assert_eq!(allowed.len(), 4, "{allowed:?}");
            assert!(allowed.contains(&usage["cost_basis"]), "{allowed:?}");
        }
    }
}

#[test]
fn lean_context_matches_the_feature_schema_without_changing_protocol() {
    use archon_core::config::HostContext;
    let mut create = full_create_agent();
    create.config.as_mut().unwrap().host_context = HostContext::Lean;
    let command = Command::CreateAgent(Box::new(create));
    let current = violations(&schema(), &command);
    assert!(current.is_empty(), "{current:?}");
    assert_eq!(
        violations(&schema_v15(), &command),
        ["create_agent.config.host_context is not in the schema"]
    );
    let current = schema();
    let config = &current["$defs"]["AgentConfig"];
    assert!(
        config["required"]
            .as_array()
            .is_none_or(|keys| !keys.contains(&json!("host_context")))
    );
    assert_eq!(
        current["$defs"]["HostContext"]["enum"],
        json!(["full", "lean"])
    );
}

#[test]
fn optional_memory_commands_match_aethers_feature_schema() {
    use archon_aether::memory_feedback::{MemoryEvidence, RecordMemoryFeedback};
    use archon_core::aether_ids::AetherMemoryFeedbackId;

    let schema: Value =
        serde_json::from_str(include_str!("fixtures/aether-memory-feedback.schema.json")).unwrap();
    let commands = [
        Command::MemoriesInContext { task: task() },
        Command::RecordMemoryFeedback(RecordMemoryFeedback {
            memory: memory(),
            verdict: "hurt".into(),
            evidence: MemoryEvidence::Outcome,
            task: task(),
            reason: "archon fb_1: correction".into(),
            idempotency_key: "archon:fb_1:mem:task".into(),
        }),
        Command::RetractMemoryFeedback {
            feedback: AetherMemoryFeedbackId::parse("mfb_1").unwrap(),
            reason: "undo".into(),
        },
    ];
    assert_eq!(
        schema["$defs"]["ClientFrame"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        commands.len()
    );
    for command in commands {
        let problems = violations(&schema, &command);
        assert!(problems.is_empty(), "{problems:?}");
    }
}

#[test]
fn submission_resolution_matches_aethers_published_wire_shape() {
    use archon_aether::memory_feedback::{FeedbackCommand, MemoryEvidence, RecordMemoryFeedback};
    let schema: Value =
        serde_json::from_str(include_str!("fixtures/aether-submission.schema.json")).unwrap();
    let request = RecordMemoryFeedback {
        memory: memory(),
        verdict: "hurt".into(),
        evidence: MemoryEvidence::Outcome,
        task: task(),
        reason: "archon fb_1: correction".into(),
        idempotency_key: "archon:fb_1:mem:task".into(),
    };
    for horizon in [None, Some("epoch:0".into())] {
        let command = Command::ResolveSubmission {
            command: FeedbackCommand {
                cmd: "record_memory_feedback",
                request: request.clone(),
            },
            horizon,
        };
        let problems = violations(&schema, &command);
        assert!(problems.is_empty(), "{problems:?}");
        let data = serde_json::to_value(&command).unwrap();
        assert_eq!(data["command"]["idempotency_key"], request.idempotency_key);
    }
}
