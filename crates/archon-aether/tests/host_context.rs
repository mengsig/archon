//! Lean host context is negotiated by feature, independently of protocol 17.
mod common;

use archon_aether::backend::{run_node_create_agent, stage_create_agent};
use archon_aether::fake::{self, Scripted};
use archon_aether::wire::{AgentConfig, CreateAgent};
use archon_core::config::HostContext;
use archon_core::ports::{NetworkPolicy, RunNodeRequest, SpawnRequest};
use common::{AGENT, TASK, connect, fake_aether};
use serde_json::json;

#[tokio::test]
async fn only_advertised_lean_context_is_sent_on_every_create() {
    for (advertised, requested, expected) in [
        (true, HostContext::Lean, Some("lean")),
        (false, HostContext::Lean, None),
        (true, HostContext::Full, None),
        (false, HostContext::Full, None),
    ] {
        let (_dir, fake) = fake_aether().await;
        if advertised {
            fake.set_features(vec!["host_context".into()]);
        }
        fake.set_default(
            "create_agent",
            Scripted::reply("agent", fake::agent(AGENT, Some(TASK))),
        );
        let client = connect(&fake).await;
        assert_eq!(client.welcome().protocol_version, 18);
        assert_eq!(
            client.welcome().effective_host_context(HostContext::Lean) == HostContext::Lean,
            advertised
        );
        for parent in [
            None,
            Some(archon_core::aether_ids::AetherAgentId::parse(AGENT).unwrap()),
        ] {
            let result = client
                .create_agent(CreateAgent {
                    role: "archon-test".into(),
                    parent,
                    config: Some(AgentConfig {
                        host_context: requested,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .await;
            if !advertised && requested == HostContext::Lean {
                assert!(matches!(result, Err(archon_core::ArchonError::Invalid(_))));
                continue;
            }
            assert_eq!(result.unwrap().id.as_str(), AGENT);
        }
        let creates = fake.received_named("create_agent");
        let refused = !advertised && requested == HostContext::Lean;
        assert_eq!(creates.len(), if refused { 0 } else { 2 });
        for create in creates {
            assert_eq!(create["config"]["host_context"].as_str(), expected);
            assert_eq!(
                create["config"].get("host_context").is_some(),
                expected.is_some()
            );
        }
    }
}

#[test]
fn stored_stage_and_node_requests_state_their_own_context() {
    let request: SpawnRequest = serde_json::from_value(json!({
        "idempotency_key": "stage-key", "host_context": "lean", "role": "stage", "name": "work",
        "objective": "work", "instructions": "contract", "choice": {"model": "codex/gpt-6-luna", "effort": "low"},
        "workspace": "/work", "repository": null, "labels": {}, "hidden_tools": [], "leaf": true,
        "cost_budget_usd": null, "deadline": null, "attach_run": null, "network": "denied", "keep": false,
    })).unwrap();
    let node: RunNodeRequest = serde_json::from_value(json!({
        "idempotency_key": "node-key", "host_context": "lean", "role": "node", "name": "work",
        "model": "codex/gpt-6-luna", "workspace": "/work", "repository": null,
        "network": "denied", "labels": {}, "attach_run": null,
    }))
    .unwrap();
    let stage = stage_create_agent(&request, NetworkPolicy::Denied);
    let passive = run_node_create_agent(&node);
    assert_eq!(stage.config.unwrap().host_context, HostContext::Lean);
    assert_eq!(passive.config.unwrap().host_context, HostContext::Lean);
    let mut legacy = serde_json::to_value(request).unwrap();
    legacy.as_object_mut().unwrap().remove("host_context");
    assert_eq!(
        serde_json::from_value::<SpawnRequest>(legacy)
            .unwrap()
            .host_context,
        HostContext::Full
    );
    let mut legacy = serde_json::to_value(node).unwrap();
    legacy.as_object_mut().unwrap().remove("host_context");
    assert_eq!(
        serde_json::from_value::<RunNodeRequest>(legacy)
            .unwrap()
            .host_context,
        HostContext::Full
    );
}

#[tokio::test]
async fn backend_requires_callers_to_refreeze_unsupported_lean_requests() {
    use archon_aether::{AetherBackend, AetherLink};
    use archon_core::ports::AgentBackend;
    use std::sync::Arc;
    let (_dir, fake) = fake_aether().await;
    let link = Arc::new(AetherLink::new());
    link.replace(Some(Arc::new(connect(&fake).await))).unwrap();
    let backend = AetherBackend::new(link, archon_core::config::ModelsConfig::default().policy());
    let request = RunNodeRequest {
        host_context: HostContext::Lean,
        idempotency_key: "frozen-lean-node".into(),
        role: "node".into(),
        name: "work".into(),
        model: "codex/gpt-6-luna".parse().unwrap(),
        workspace: "/work".into(),
        repository: None,
        repository_root: None,
        network: NetworkPolicy::Denied,
        labels: Default::default(),
        attach_run: None,
    };
    let refused = backend.create_run_node(request).await.unwrap_err();
    assert!(matches!(
        refused,
        archon_core::error::ArchonError::Unavailable(_)
    ));
    assert!(refused.to_string().contains("frozen lean creation request"));
    assert!(fake.received_named("create_agent").is_empty());
    assert_eq!(
        backend.host_context(HostContext::Lean).await.unwrap(),
        HostContext::Full
    );
}

#[test]
fn agents_accept_unknown_host_context_values() {
    let mut raw = fake::agent(AGENT, Some(TASK));
    raw["config"]["host_context"] = json!("future-context");
    let agent: archon_aether::wire::Agent = serde_json::from_value(raw).unwrap();
    assert_eq!(agent.id.as_str(), AGENT);
    assert_eq!(agent.config.host_context.as_deref(), Some("future-context"));
}
