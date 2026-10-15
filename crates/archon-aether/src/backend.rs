//! `AgentBackend` over a live Aether connection (ARCHITECTURE §4.5).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use archon_core::policy::ModelPolicy;
use archon_core::ports::{
    AgentBackend, AgentUpdate, CreatedNode, Descendant, DescendantState, RunNodeRequest,
    SpawnRequest, SpawnedAgent,
};
use archon_core::run::StageOutcome;
use async_trait::async_trait;

use crate::client::AetherClient;
use crate::link::AetherLink;
use crate::resumed;
use crate::wire::{
    Agent, AgentConfig, AgentPatch, AgentQuery, ApprovalPolicy, CreateAgent, NetworkPolicy, Scope,
    Task, TaskStatus, ToolExposure,
};

/// Aether agent states in which an agent can still act or take children.
/// Everything else (`completed`, `failed`, `cancelled`, `imported`) is final.
pub const LIVE_STATES: &[&str] = &["created", "ready", "running", "waiting", "blocked"];

#[derive(Debug)]
pub struct AetherBackend {
    link: Arc<AetherLink>,
    /// Re-checked at spawn so a plan persisted under an older policy is
    /// refused (ARCHITECTURE §7.4).
    policy: ModelPolicy,
}

impl AetherBackend {
    pub fn new(link: Arc<AetherLink>, policy: ModelPolicy) -> Self {
        Self { link, policy }
    }
}

fn require_host_context(
    client: &AetherClient,
    stored: archon_core::config::HostContext,
) -> Result<(), ArchonError> {
    let welcome = client.welcome();
    assert!(welcome.protocol_version >= crate::wire::MIN_PROTOCOL_VERSION);
    assert!(
        !welcome.instance_id.is_empty(),
        "the client has an Aether store identity"
    );
    let effective = welcome.effective_host_context(stored);
    assert!(
        effective.is_full() || effective == stored,
        "negotiation only downgrades"
    );
    if stored != effective {
        return Err(ArchonError::Unavailable(
            "Aether version lacks host_context required by the frozen lean creation request".into(),
        ));
    }
    Ok(())
}

/// The agent's `status.state`; `unknown` when Aether sent something else.
pub fn agent_state(agent: &Agent) -> String {
    agent
        .status
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

pub fn agent_is_live(agent: &Agent) -> bool {
    LIVE_STATES.contains(&agent_state(agent).as_str())
}

/// How `agent` stands for whoever waits on it: an unknown state is `Live`, so
/// nothing that may still act is taken for ended.
fn descendant_state(agent: &Agent) -> DescendantState {
    match agent_state(agent).as_str() {
        "completed" => DescendantState::Completed,
        "failed" | "cancelled" | "imported" => DescendantState::Stopped,
        _ => DescendantState::Live,
    }
}

/// The agents of `agents` beneath `root`, at any depth, nearest first. An
/// ended agent is passed through: what it delegated may still run.
pub fn subtree(root: &AetherAgentId, agents: &[Agent]) -> Vec<Descendant> {
    let mut children: BTreeMap<&AetherAgentId, Vec<&Agent>> = BTreeMap::new();
    for agent in agents {
        if let Some(parent) = &agent.parent {
            children.entry(parent).or_default().push(agent);
        }
    }
    let mut found = Vec::new();
    let mut seen = BTreeSet::from([root]);
    let mut next = vec![root];
    while let Some(parent) = next.pop() {
        for child in children.get(parent).into_iter().flatten() {
            // Parentage is a tree in Aether; a repeat would loop forever.
            if !seen.insert(&child.id) {
                continue;
            }
            next.push(&child.id);
            found.push(Descendant {
                agent: child.id.clone(),
                parent: parent.clone(),
                model: child.model.clone(),
                effort: child.config.effort.as_deref().and_then(|e| e.parse().ok()),
                state: descendant_state(child),
                task: child.current_task.clone(),
            });
        }
    }
    assert!(
        found.len() <= agents.len(),
        "an agent is found at most once"
    );
    assert!(
        found.iter().all(|d| d.agent != *root),
        "the root is no descendant"
    );
    found
}

/// The exact `create_agent` for a stage agent. `network` is the request's
/// persisted one (its run node's, which a child may not widen), or Aether's
/// default for a record that predates it. Stage agents are headless, so
/// nothing is gated: under `[approval] default = "ask"` a gated call would
/// wait for nobody.
pub fn stage_create_agent(request: &SpawnRequest, network: NetworkPolicy) -> CreateAgent {
    assert!(
        request.network.is_none_or(|stated| stated == network),
        "a persisted network is sent as stored"
    );
    assert!(
        !request.idempotency_key.is_empty(),
        "stage agents are always created under an idempotency key"
    );
    assert!(
        request.workspace.is_absolute(),
        "workspace must be absolute: {}",
        request.workspace.display()
    );
    let tools = match request.hidden_tools.is_empty() {
        true => ToolExposure::All,
        false => ToolExposure::Except {
            names: request.hidden_tools.iter().cloned().collect(),
        },
    };
    CreateAgent {
        role: request.role.clone(),
        name: Some(request.name.clone()),
        objective: Some(request.objective.clone()),
        model: Some(request.choice.model.clone()),
        scope: Some(Scope {
            workspace_root: Some(request.workspace.clone()),
            repository: request.repository.clone(),
            network,
            labels: request.labels.clone(),
        }),
        repository_root: request.repository_root.clone(),
        parent: request.parent.clone(),
        config: Some(AgentConfig {
            host_context: request.host_context,
            instructions: Some(request.instructions.clone()),
            effort: Some(request.choice.effort),
            cost_budget: request
                .cost_budget_usd
                .filter(|_| request.aether_cost_budget != Some(false)),
            deadline: request.deadline,
            tools,
            long_lived: request.keep,
            leaf: request.leaf,
        }),
        approval: Some(ApprovalPolicy::none()),
        run: request.attach_run.clone(),
        start: true,
        idempotency_key: Some(request.idempotency_key.clone()),
    }
}

/// The exact `create_agent` for a pipeline run's node: a root with no
/// objective and `start: false`, so Aether never schedules it. Its scope is
/// the run's worktree with the network the request recorded, and its
/// approval gates nothing, so every stage agent (same worktree and network,
/// `require_for: []`) narrows it and can be its child. Archon's tools are
/// hidden: should an operator ever restart a retired node in Aether, the
/// agent that results cannot act for the orchestrator.
pub fn run_node_create_agent(request: &RunNodeRequest) -> CreateAgent {
    assert!(
        !request.idempotency_key.is_empty(),
        "run nodes are always created under an idempotency key"
    );
    assert!(
        request.workspace.is_absolute(),
        "workspace must be absolute: {}",
        request.workspace.display()
    );
    assert!(!request.name.trim().is_empty(), "a run node has a name");
    CreateAgent {
        role: request.role.clone(),
        name: Some(request.name.clone()),
        objective: None,
        model: Some(request.model.clone()),
        scope: Some(Scope {
            workspace_root: Some(request.workspace.clone()),
            repository: request.repository.clone(),
            network: request.network,
            labels: request.labels.clone(),
        }),
        repository_root: request.repository_root.clone(),
        parent: None,
        config: Some(AgentConfig {
            host_context: request.host_context,
            instructions: None,
            effort: None,
            cost_budget: None,
            deadline: None,
            tools: ToolExposure::Except {
                names: archon_core::mcp_tools::qualified_names()
                    .into_iter()
                    .collect(),
            },
            long_lived: false,
            leaf: false,
        }),
        approval: Some(ApprovalPolicy::none()),
        run: request.attach_run.clone(),
        start: false,
        idempotency_key: Some(request.idempotency_key.clone()),
    }
}

/// `None` while the task is open.
pub fn task_outcome(task: &Task) -> Option<StageOutcome> {
    match &task.status {
        TaskStatus::Queued | TaskStatus::Assigned | TaskStatus::Active => None,
        TaskStatus::Blocked { .. } => None,
        TaskStatus::Completed { result } => Some(StageOutcome::Completed {
            summary: result.summary.clone(),
            structured: result.structured.clone(),
        }),
        TaskStatus::Failed { error, .. } => Some(StageOutcome::Failed {
            error: error.clone(),
            infra: false,
        }),
        TaskStatus::Cancelled { reason } => Some(StageOutcome::Cancelled {
            reason: reason.clone(),
        }),
    }
}

/// Task from `agent.current_task`, run from `get_task`, which also proves
/// the task exists and belongs to this agent.
async fn spawned_from(
    client: &AetherClient,
    agent: Agent,
    key: &str,
) -> Result<SpawnedAgent, ArchonError> {
    let task_id = agent.current_task.clone().ok_or_else(|| {
        ArchonError::Aether(format!(
            "create_agent (key {key}) returned agent {} without a task",
            agent.id
        ))
    })?;
    let task = client.get_task(&task_id).await?;
    if task.owner.as_ref().is_some_and(|owner| *owner != agent.id) {
        return Err(ArchonError::Protocol(format!(
            "task {task_id} of agent {} is owned by {:?}",
            agent.id, task.owner
        )));
    }
    Ok(SpawnedAgent {
        agent: agent.id,
        task: task_id,
        run: task.run,
    })
}

/// `update` as Aether's patch, which leaves alone what it does not name.
fn agent_patch(update: AgentUpdate) -> AgentPatch {
    let budget = update.cost_budget_usd;
    assert!(
        budget.is_none_or(|usd| usd.is_finite() && usd > 0.0),
        "a hard cap is a positive dollar amount, not {budget:?}"
    );
    let (model, effort) = match update.choice {
        Some(choice) => (Some(choice.model), Some(choice.effort)),
        None => (None, None),
    };
    AgentPatch {
        model,
        effort,
        cost_budget: budget,
        deadline: update.deadline.map(|at| serde_json::json!({"set": at})),
    }
}

#[async_trait]
impl AgentBackend for AetherBackend {
    async fn spawn(&self, mut request: SpawnRequest) -> Result<SpawnedAgent, ArchonError> {
        self.policy.check(&request.choice)?;
        if request.cost_budget_usd.is_some() && request.aether_cost_budget.is_none() {
            self.prepare_spawn(&mut request, true).await?;
        }
        let client = self.link.client()?;
        let network = match request.network {
            Some(stated) => stated,
            // A record from before the network was persisted: sent as that
            // daemon sent it, with Aether's default of the moment.
            None => client.daemon_defaults().await?.network_default,
        };
        require_host_context(&client, request.host_context)?;
        let agent = client
            .create_agent(stage_create_agent(&request, network))
            .await?;
        spawned_from(&client, agent, &request.idempotency_key).await
    }

    async fn host_context(
        &self,
        requested: archon_core::config::HostContext,
    ) -> Result<archon_core::config::HostContext, ArchonError> {
        if requested.is_full() {
            return Ok(requested);
        }
        let client = self.link.client()?;
        assert!(client.welcome().protocol_version >= crate::wire::MIN_PROTOCOL_VERSION);
        assert!(
            !client.welcome().instance_id.is_empty(),
            "the client has an Aether store identity"
        );
        Ok(client.welcome().effective_host_context(requested))
    }

    async fn supports_repository_root(&self) -> Result<bool, ArchonError> {
        Ok(self.link.client()?.welcome().supports_repository_root())
    }

    async fn prepare_spawn(
        &self,
        request: &mut SpawnRequest,
        count_estimated_cost: bool,
    ) -> Result<(), ArchonError> {
        if request.cost_budget_usd.is_none() {
            return Ok(());
        }
        self.policy.check(&request.choice)?;
        let reports_cost = match request.aether_cost_budget {
            Some(stored) => stored,
            None => {
                let client = self.link.client()?;
                let models = client.list_models().await?;
                let providers = client.providers().await?;
                let model = models.iter().find(|info| {
                    info.model.provider == request.choice.model.provider
                        && (info.model.model == request.choice.model.model
                            || info.aliases.contains(&request.choice.model.model))
                });
                let provider = providers
                    .iter()
                    .find(|info| info.provider == request.choice.model.provider);
                // v14 to v17 expose no reports_cost flag. Mirror Aether's protocol rule:
                // engines differ even when they carry a price; HTTP needs a price.
                // Missing metadata never proves a hard cap is enforceable. Archon
                // then requires estimates and refuses unaccounted usage in the pump.
                match provider.map(|info| info.protocol.as_str()) {
                    Some("claude-code") => true,
                    Some("codex") | None => false,
                    Some(_) => {
                        provider.is_some_and(|info| info.engine == Some(false))
                            && model.is_some_and(|info| info.capabilities.cost.is_some())
                    }
                }
            }
        };
        if !reports_cost && !count_estimated_cost {
            return Err(ArchonError::Policy(format!(
                "cannot enforce the dollar budget for {}: Aether cost reporting is \
                 unavailable or unconfirmed and limits.count_estimated_cost is disabled; \
                 enable subscription estimates or choose a cost-reporting model",
                request.choice.model
            )));
        }
        request.aether_cost_budget = Some(reports_cost);
        Ok(())
    }

    async fn create_run_node(&self, request: RunNodeRequest) -> Result<CreatedNode, ArchonError> {
        let client = self.link.client()?;
        require_host_context(&client, request.host_context)?;
        let agent = client.create_agent(run_node_create_agent(&request)).await?;
        let key = &request.idempotency_key;
        if agent.current_task.is_some() {
            return Err(ArchonError::Protocol(format!(
                "run node {} (key {key}) was given a task; it must stay passive",
                agent.id
            )));
        }
        let run = agent.run.ok_or_else(|| {
            ArchonError::Protocol(format!("run node {} (key {key}) has no run", agent.id))
        })?;
        Ok(CreatedNode {
            agent: agent.id,
            run,
        })
    }

    async fn network_default(&self) -> Result<NetworkPolicy, ArchonError> {
        let defaults = self.link.client()?.daemon_defaults().await?;
        Ok(defaults.network_default)
    }

    async fn is_live(&self, agent: &AetherAgentId) -> Result<bool, ArchonError> {
        match self.link.client()?.get_agent(agent).await {
            Ok(found) => Ok(agent_is_live(&found)),
            Err(ArchonError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn cancel(&self, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError> {
        self.link.client()?.cancel(agent, reason).await
    }

    async fn update_agent(
        &self,
        agent: &AetherAgentId,
        update: AgentUpdate,
    ) -> Result<(), ArchonError> {
        assert!(!update.is_empty(), "an update changes something");
        if let Some(choice) = &update.choice {
            self.policy.check(choice)?;
        }
        let patch = agent_patch(update);
        self.link.client()?.update_agent(agent, patch).await?;
        Ok(())
    }

    async fn interrupt(&self, agent: &AetherAgentId) -> Result<bool, ArchonError> {
        self.link.client()?.interrupt(agent).await
    }

    /// Two reads however deep the tree: the agent's run, then that run's agents.
    async fn descendants(&self, agent: &AetherAgentId) -> Result<Vec<Descendant>, ArchonError> {
        let client = self.link.client()?;
        let root = match client.get_agent(agent).await {
            Ok(root) => root,
            Err(ArchonError::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let run = root.run.ok_or_else(|| {
            ArchonError::Protocol(format!(
                "agent {agent} has no run to list its descendants in"
            ))
        })?;
        let query = AgentQuery {
            run: Some(run),
            // An agent cleared from the view still links what it delegated.
            include_archived: true,
            ..AgentQuery::default()
        };
        Ok(subtree(agent, &client.list_agents(query).await?))
    }

    async fn task_outcome(&self, task: &AetherTaskId) -> Result<Option<StageOutcome>, ArchonError> {
        let task = self.link.client()?.get_task(task).await?;
        Ok(task_outcome(&task))
    }

    async fn send_message(
        &self,
        to: &AetherAgentId,
        content: &str,
        idempotency_key: &str,
    ) -> Result<(), ArchonError> {
        self.link
            .client()?
            .send_message(to, content, idempotency_key)
            .await
    }

    async fn resumed_outcome(
        &self,
        agent: &AetherAgentId,
        message: &str,
    ) -> Result<Option<StageOutcome>, ArchonError> {
        let client = self.link.client()?;
        let found = match client.get_agent(agent).await {
            Ok(found) => found,
            Err(ArchonError::NotFound(why)) => return Ok(Some(resumed::lost(&why))),
            Err(error) => return Err(error),
        };
        if !agent_is_live(&found) {
            let state = agent_state(&found);
            return Ok(Some(resumed::lost(&format!("Aether holds it as {state}"))));
        }
        if !resumed::is_idle(&found) {
            return Ok(None);
        }
        resumed::read(&client, agent, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::{Effort, ModelChoice};

    fn node_id() -> AetherAgentId {
        AetherAgentId::parse("agt_node").unwrap()
    }

    fn request() -> SpawnRequest {
        SpawnRequest {
            host_context: Default::default(),
            idempotency_key: "stg_01".into(),
            role: "archon-stage:direct/implement".into(),
            name: "implement · add a greeting".into(),
            objective: "do it".into(),
            instructions: "contract".into(),
            choice: ModelChoice {
                model: "claude-code/claude-opus-5-5".parse().unwrap(),
                effort: Effort::High,
                tier: None,
            },
            workspace: "/work/tree".into(),
            repository: Some("repo".into()),
            repository_root: None,
            labels: [("archon.item".to_owned(), "qi_1".to_owned())].into(),
            hidden_tools: vec!["mcp__archon__enqueue".into()],
            leaf: true,
            cost_budget_usd: Some(5.0),
            aether_cost_budget: None,
            deadline: None,
            attach_run: None,
            parent: Some(node_id()),
            network: Some(NetworkPolicy::Denied),
            keep: false,
        }
    }

    fn node_request() -> RunNodeRequest {
        RunNodeRequest {
            host_context: Default::default(),
            idempotency_key: "prun_01:node:1".into(),
            role: "archon-run:direct".into(),
            name: "add a greeting".into(),
            model: "claude-code/claude-opus-5-5".parse().unwrap(),
            workspace: "/work/tree".into(),
            repository: Some("repo".into()),
            repository_root: None,
            network: NetworkPolicy::Denied,
            labels: [("archon.item".to_owned(), "qi_1".to_owned())].into(),
            attach_run: None,
        }
    }

    #[test]
    fn stage_agents_are_keyed_children_of_the_run_node_with_archon_tools_hidden() {
        let create = stage_create_agent(&request(), NetworkPolicy::Denied);
        assert_eq!(create.idempotency_key.as_deref(), Some("stg_01"));
        assert_eq!(create.parent, Some(node_id()));
        assert_eq!(create.name.as_deref(), Some("implement · add a greeting"));
        let config = create.config.unwrap();
        assert!(config.leaf && !config.long_lived);
        assert_eq!(config.effort, Some(Effort::High));
        assert!(
            matches!(config.tools, ToolExposure::Except { ref names } if names.contains("mcp__archon__enqueue"))
        );
    }

    #[test]
    fn a_stage_without_a_node_is_still_a_root() {
        let mut req = request();
        req.parent = None;
        req.network = None;
        let create = stage_create_agent(&req, NetworkPolicy::Allowed);
        assert!(create.parent.is_none());
        let frame = serde_json::to_value(&create).unwrap();
        assert!(frame.get("parent").is_none(), "absent, not null");
        assert_eq!(frame["scope"]["network"], "allowed");
    }

    #[test]
    fn stage_agents_state_network_and_gate_nothing() {
        for network in [NetworkPolicy::Allowed, NetworkPolicy::Denied] {
            let mut req = request();
            req.network = Some(network);
            let create = stage_create_agent(&req, network);
            assert_eq!(create.scope.as_ref().map(|s| s.network), Some(network));
            assert_eq!(create.approval, Some(ApprovalPolicy::none()));
            let frame = serde_json::to_value(&create).unwrap();
            assert_eq!(frame["approval"]["require_for"], serde_json::json!([]));
        }
        let mut req = request();
        req.network = Some(NetworkPolicy::Allowed);
        let allowed = serde_json::to_value(stage_create_agent(&req, NetworkPolicy::Allowed));
        assert_eq!(allowed.unwrap()["scope"]["network"], "allowed");
    }

    #[test]
    #[should_panic(expected = "sent as stored")]
    fn a_persisted_network_is_never_overridden() {
        stage_create_agent(&request(), NetworkPolicy::Allowed);
    }

    #[test]
    fn run_nodes_are_passive_keyed_roots_that_stages_can_narrow() {
        let node = run_node_create_agent(&node_request());
        assert_eq!(node.idempotency_key.as_deref(), Some("prun_01:node:1"));
        assert!(node.parent.is_none() && node.objective.is_none());
        assert!(!node.start, "never scheduled: no turns, no spend");
        assert_eq!(node.name.as_deref(), Some("add a greeting"));
        let config = node.config.as_ref().unwrap();
        assert!(!config.leaf, "a leaf could not take stage agents");
        assert!(!config.long_lived && config.instructions.is_none());
        let ToolExposure::Except { names } = &config.tools else {
            panic!("a node hides Archon's tools: {:?}", config.tools);
        };
        assert!(names.contains("mcp__archon__enqueue"));
        assert_eq!(names.len(), archon_core::mcp_tools::qualified_names().len());
        let frame = serde_json::to_value(&node).unwrap();
        assert_eq!(frame["start"], false);
        assert_eq!(
            frame["scope"]["network"], "denied",
            "as the request recorded"
        );
        assert!(frame.get("objective").is_none());
        let stage = stage_create_agent(&request(), NetworkPolicy::Denied);
        let (node_scope, stage_scope) = (node.scope.unwrap(), stage.scope.unwrap());
        assert_eq!(node_scope.workspace_root, stage_scope.workspace_root);
        assert_eq!(node_scope.repository, stage_scope.repository);
        assert_eq!(node_scope.network, stage_scope.network);
        assert_eq!(node.approval, stage.approval, "equal gates narrow");
    }

    #[test]
    fn liveness_follows_the_status_state() {
        let mut agent: Agent = serde_json::from_value(crate::fake::agent("agt_1", None)).unwrap();
        for state in LIVE_STATES {
            agent.status = serde_json::json!({"state": state});
            assert!(agent_is_live(&agent), "{state}");
        }
        for state in ["completed", "failed", "cancelled", "imported", "bogus"] {
            agent.status = serde_json::json!({"state": state});
            assert!(!agent_is_live(&agent), "{state}");
        }
        agent.status = serde_json::json!({});
        assert_eq!(agent_state(&agent), "unknown");
    }

    fn held(id: &str, parent: Option<&str>, state: &str) -> Agent {
        let mut value = crate::fake::agent(id, None);
        value["parent"] = serde_json::json!(parent);
        value["status"] = serde_json::json!({"state": state});
        value["config"] = serde_json::json!({"effort": "medium"});
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_subtree_reaches_through_ended_agents_and_leaves_strangers_out() {
        let id = |raw: &str| AetherAgentId::parse(raw).unwrap();
        let agents = [
            held("agt_lead", Some("agt_node"), "completed"),
            held("agt_child", Some("agt_lead"), "completed"),
            held("agt_grandchild", Some("agt_child"), "running"),
            held("agt_odd", Some("agt_lead"), "bogus"),
            held("agt_sibling", Some("agt_node"), "running"),
            held("agt_cancelled", Some("agt_lead"), "cancelled"),
        ];
        let mut found = subtree(&id("agt_lead"), &agents);
        found.sort_by(|a, b| a.agent.cmp(&b.agent));
        let states: Vec<(&str, &str, DescendantState)> = found
            .iter()
            .map(|d| (d.agent.as_str(), d.parent.as_str(), d.state))
            .collect();
        assert_eq!(
            states,
            [
                ("agt_cancelled", "agt_lead", DescendantState::Stopped),
                ("agt_child", "agt_lead", DescendantState::Completed),
                ("agt_grandchild", "agt_child", DescendantState::Live),
                ("agt_odd", "agt_lead", DescendantState::Live),
            ]
        );
        assert!(found.iter().all(|d| d.effort == Some(Effort::Medium)));
        assert!(subtree(&id("agt_grandchild"), &agents).is_empty());
    }

    #[test]
    fn an_agent_patch_names_only_what_the_update_changes() {
        let budget = AgentUpdate {
            cost_budget_usd: Some(4.0),
            ..AgentUpdate::default()
        };
        let patch = serde_json::to_value(agent_patch(budget)).unwrap();
        assert_eq!(patch, serde_json::json!({"cost_budget": 4.0}));
        let choice = AgentUpdate {
            choice: Some(request().choice),
            ..AgentUpdate::default()
        };
        let patch = agent_patch(choice);
        assert_eq!(patch.model, Some(request().choice.model));
        assert_eq!(patch.effort, Some(Effort::High));
        assert!(patch.cost_budget.is_none() && patch.deadline.is_none());
    }

    #[test]
    #[should_panic(expected = "hard cap")]
    fn a_non_positive_hard_cap_is_a_bug() {
        agent_patch(AgentUpdate {
            cost_budget_usd: Some(0.0),
            ..AgentUpdate::default()
        });
    }

    #[test]
    #[should_panic(expected = "absolute")]
    fn relative_workspaces_are_a_bug() {
        let mut req = request();
        req.workspace = "relative".into();
        stage_create_agent(&req, NetworkPolicy::Denied);
    }

    #[test]
    #[should_panic(expected = "absolute")]
    fn relative_node_workspaces_are_a_bug() {
        let mut req = node_request();
        req.workspace = "relative".into();
        run_node_create_agent(&req);
    }
}
