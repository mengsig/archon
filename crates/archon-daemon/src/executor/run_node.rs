//! The passive Aether agent a pipeline run's stage agents sit under
//! (ARCHITECTURE §4.5), so Aether's tree mirrors Archon's. It is created
//! when the run's first stage is issued, reused by every later stage,
//! replaced when Aether lost it, and retired once the run has finished.
//!
//! Aether fingerprints the whole `create_agent` frame under its key, so
//! each generation's request, Aether's network default included, is
//! persisted before it is first sent, and every retry and every concurrent
//! caller sends that stored request. The request stays with the node so
//! no stage agent bound to it states a wider network.
//!
//! Pending lean requests may fall back to full; if Aether rejects the changed
//! replay, creation advances to a new generation with a new key.
//!
//! Every node agent Archon learns of is indexed, retired ones included, so
//! the fleet pump can tell a node's own events from a stage agent's. The
//! index becomes a routing tombstone after retirement and retention, so
//! late messages still have an owner even when lifecycle bookkeeping goes.

use std::path::PathBuf;

use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherRunId};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::model::ModelRef;
use archon_core::ports::{CreatedNode, DescendantState, NetworkPolicy, RunNodeRequest};
use archon_core::run::StageRun;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::{naming, resume};
use crate::context::DaemonContext;
use crate::resolve;

const NODE_KEY_PREFIX: &str = "run_node:";
/// `kv`: a node agent, holding its pipeline run. Kept after retirement
/// until [`prune_retired_node_agents`].
const NODE_AGENT_KEY_PREFIX: &str = "run_node_agent:";
const MESSAGE_NODE_KEY_PREFIX: &str = "message_run_node:";

/// How long after its stage settled an agent may still be ending its turn
/// before the node is cancelled. Retirement still waits for its subtree to end.
/// Aether settles a task mid-turn
/// and the agent completes when the turn ends.
pub(super) const TURN_END_GRACE_SECS: i64 = 60;

/// The live node a stage agent is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunNode {
    pub agent: AetherAgentId,
    /// What the node's scope states; a child may not widen it.
    pub network: NetworkPolicy,
    pub repository: RepositoryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum RepositoryIdentity {
    Name(Option<String>),
    Root(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepositoryMode {
    Name,
    Root,
}

impl RepositoryIdentity {
    fn from_request(request: &RunNodeRequest) -> Self {
        assert!(request.repository_root.is_none() || request.repository.is_none());
        assert!(
            request
                .repository_root
                .as_ref()
                .is_none_or(|root| root.is_absolute())
        );
        match &request.repository_root {
            Some(root) => Self::Root(root.clone()),
            None => Self::Name(request.repository.clone()),
        }
    }

    pub(super) fn parts(&self) -> (Option<String>, Option<PathBuf>) {
        match self {
            Self::Name(name) => (name.clone(), None),
            Self::Root(root) => (None, Some(root.clone())),
        }
    }

    fn check_checkout(&self) -> Result<(), ArchonError> {
        let Self::Root(root) = self else {
            return Ok(());
        };
        assert!(root.is_absolute(), "a frozen checkout is absolute");
        let problem = match std::fs::metadata(root) {
            Ok(metadata) if metadata.is_dir() => return Ok(()),
            Ok(_) => "is no longer a directory".to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "is missing".to_owned(),
            Err(error) => format!("cannot be read: {error}"),
        };
        Err(ArchonError::Invalid(format!(
            "the registered checkout {} {problem}; restore it there and retry the item \
             (a re-registered path applies to new runs)",
            root.display()
        )))
    }
}

/// The node of one pipeline run, in `kv` until `retire` forgets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredNode {
    /// `None` until the generation's create was answered.
    pub agent: Option<AetherAgentId>,
    /// Counted from one and part of the idempotency key, so a replacement
    /// is a new Aether agent rather than a replay of the dead one.
    pub generation: u32,
    /// The generation's `create_agent` inputs, stored before the first
    /// create and kept: stage agents bound later state its network. `None`
    /// only for a generation left behind after Aether refused its key.
    pub request: Option<RunNodeRequest>,
    /// Kept across refused or lost node generations; old requests use their name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository_identity: Option<RepositoryIdentity>,
}

impl StoredNode {
    fn fresh() -> Self {
        Self {
            agent: None,
            generation: 1,
            request: None,
            repository_identity: None,
        }
    }

    fn repository_identity(&self) -> Option<RepositoryIdentity> {
        self.repository_identity
            .clone()
            .or_else(|| self.request.as_ref().map(RepositoryIdentity::from_request))
    }

    fn check(&self) {
        assert!(self.generation >= 1, "generations are counted from one");
        assert!(
            self.agent.is_none() || self.request.is_some(),
            "a recorded node keeps the request it was created with"
        );
        if let Some(request) = &self.request {
            if let Some(identity) = &self.repository_identity {
                assert_eq!(identity, &RepositoryIdentity::from_request(request));
            }
            assert!(
                request
                    .idempotency_key
                    .ends_with(&format!(":node:{}", self.generation)),
                "the request belongs to another generation"
            );
        }
    }

    /// The node Aether was told to create, once its create was answered.
    fn recorded(&self) -> Option<RunNode> {
        let agent = self.agent.clone()?;
        let request = self
            .request
            .as_ref()
            .expect("checked: recorded with its request");
        Some(RunNode {
            agent,
            network: request.network,
            repository: RepositoryIdentity::from_request(request),
        })
    }
}

fn node_key(run: &PipelineRunId) -> String {
    format!("{NODE_KEY_PREFIX}{run}")
}

fn node_agent_key(agent: &AetherAgentId) -> String {
    format!("{NODE_AGENT_KEY_PREFIX}{agent}")
}

/// Whether `run` still has its node entry: live, or finished and not yet
/// retired.
pub(super) fn has_node(store: &Store, run: &PipelineRunId) -> Result<bool, ArchonError> {
    Ok(store.kv_get(&node_key(run))?.is_some())
}

/// Whether `agent` is, or was, the node of a pipeline run.
pub(crate) fn is_node_agent(store: &Store, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    Ok(node_run(store, agent)?.is_some())
}

/// Move retired node indexes past `cutoff` to message routing tombstones.
pub(crate) fn prune_retired_node_agents(
    store: &mut Store,
    cutoff: Timestamp,
) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    // Keep routing for one additional retention window, then forget it.
    let expired_cutoff = cutoff - crate::prune::retention();
    for (key, raw) in store.kv_prefix(MESSAGE_NODE_KEY_PREFIX)? {
        let run = PipelineRunId::parse(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key}: {e}")))?;
        let remove = match store.run(&run) {
            Ok(run) => run.finished_at.is_some_and(|at| at < expired_cutoff),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if remove && store.kv_get(&node_key(&run))?.is_none() {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    for (key, raw) in store.kv_prefix(NODE_AGENT_KEY_PREFIX)? {
        let run = PipelineRunId::parse(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
        let (retired, expired) = match store.run(&run) {
            Ok(found) => {
                assert_eq!(
                    found.state.is_terminal(),
                    found.finished_at.is_some(),
                    "a run has a finish time exactly when it finished"
                );
                let finished = found.finished_at.is_some_and(|at| at < cutoff);
                (
                    finished && store.kv_get(&node_key(&run))?.is_none(),
                    found.finished_at.is_some_and(|at| at < expired_cutoff),
                )
            }
            Err(ArchonError::NotFound(_)) => (true, true),
            Err(error) => return Err(error),
        };
        if retired {
            let agent = key
                .strip_prefix(NODE_AGENT_KEY_PREFIX)
                .expect("prefix scan");
            if !expired {
                store.kv_set(&format!("{MESSAGE_NODE_KEY_PREFIX}{agent}"), &raw)?;
            }
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// The pipeline run `agent` is, or was, the node of.
pub(crate) fn node_run(
    store: &Store,
    agent: &AetherAgentId,
) -> Result<Option<PipelineRunId>, ArchonError> {
    let key = node_agent_key(agent);
    let raw = store
        .kv_get(&key)?
        .or(store.kv_get(&format!("{MESSAGE_NODE_KEY_PREFIX}{agent}"))?);
    let Some(raw) = raw else {
        return Ok(None);
    };
    PipelineRunId::parse(&raw)
        .map(Some)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))
}

pub(crate) async fn recover_message_node(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    let found = match ctx.aether.client()?.get_agent(agent).await {
        Ok(found) => found,
        Err(ArchonError::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    if found.parent.is_some() {
        return Ok(());
    }
    let Some(pipeline) = found.role.strip_prefix("archon-run:") else {
        return Ok(());
    };
    let labels = found.scope.as_ref().map(|s| &s.labels);
    let raw = labels.and_then(|l| l.get("archon.run")).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "run node {agent} has no Archon run label; use message_orchestrator"
        ))
    })?;
    let run = PipelineRunId::parse(raw)?;
    let mut store = ctx.store.lock().await;
    let owner = store.run(&run)?;
    let item = labels.and_then(|l| l.get("archon.item"));
    if owner.pipeline != pipeline || item.map(String::as_str) != Some(owner.item.as_str()) {
        return Err(ArchonError::Invalid(format!(
            "run node {agent} labels do not match run {run}; use message_orchestrator"
        )));
    }
    store.kv_set(&format!("{MESSAGE_NODE_KEY_PREFIX}{agent}"), run.as_str())
}

/// Index a node agent Aether created under one of `run`'s keys.
fn index_node_agent(
    store: &mut Store,
    run: &PipelineRunId,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    let key = node_agent_key(agent);
    if let Some(indexed) = store.kv_get(&key)? {
        assert_eq!(indexed, run.as_str(), "a node agent belongs to one run");
        return Ok(());
    }
    store.kv_set(&key, run.as_str())
}

/// The run a `kv.run_node:*` key names.
fn run_of_key(key: &str) -> Result<PipelineRunId, ArchonError> {
    let raw = key
        .strip_prefix(NODE_KEY_PREFIX)
        .ok_or_else(|| ArchonError::Corrupt(format!("kv key {key} is not a run node")))?;
    PipelineRunId::parse(raw).map_err(|e| ArchonError::Corrupt(format!("run node key {key}: {e}")))
}

pub(crate) fn idempotency_key(run: &PipelineRunId, generation: u32) -> String {
    assert!(generation >= 1, "generations are counted from one");
    format!("{run}:node:{generation}")
}

/// The stored node of `run`; a run without one is at generation one.
pub(crate) fn load_node(store: &Store, run: &PipelineRunId) -> Result<StoredNode, ArchonError> {
    let Some(raw) = store.kv_get(&node_key(run))? else {
        return Ok(StoredNode::fresh());
    };
    let node: StoredNode = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("run node of {run}: {e}")))?;
    node.check();
    Ok(node)
}

fn save_node(store: &mut Store, run: &PipelineRunId, node: &StoredNode) -> Result<(), ArchonError> {
    node.check();
    store.kv_set(&node_key(run), &serde_json::to_string(node)?)
}

/// Finished runs whose node entry is still there, with no live stage left
/// under it: what the next sweep must `retire`. Only unretired runs have
/// an entry, and `kv_prefix` ranges over the key index, so the pass costs
/// what is still to retire, not the store's history.
pub(super) fn runs_to_retire(store: &Store) -> Result<Vec<PipelineRunId>, ArchonError> {
    let mut due = Vec::new();
    for (key, _) in store.kv_prefix(NODE_KEY_PREFIX)? {
        let run = run_of_key(&key)?;
        if !store.run(&run)?.state.is_terminal() {
            continue;
        }
        let stages = store.stage_runs(&run)?;
        if stages.iter().any(|s| !s.state.is_terminal()) {
            continue;
        }
        due.push(run);
    }
    Ok(due)
}

/// Reuse a live node, replacing lost nodes or one refused key.
pub(super) async fn ensure(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    model: &ModelRef,
    network: NetworkPolicy,
) -> Result<RunNode, ArchonError> {
    let observed = load_node(&*ctx.store.lock().await, run)?;
    if let Some(node) = observed.recorded() {
        if ctx.backend.is_live(&node.agent).await? {
            return Ok(node);
        }
        tracing::warn!(%run, agent = %node.agent, "Aether lost the run node; creating a replacement");
    }
    let refused = match create_generation(ctx, run, &observed, model, network).await? {
        Created::Agent(node) => return Ok(node),
        Created::Refused { generation, error } => {
            tracing::warn!(%run, generation, %error, "Aether refused the run node's key; trying the next generation");
            skip_generation(ctx, run, generation).await?
        }
    };
    match create_generation(ctx, run, &refused, model, network).await? {
        Created::Agent(node) => Ok(node),
        Created::Refused { generation, error } => {
            // Retire the refused generation so later attempts use a fresh key.
            skip_generation(ctx, run, generation).await?;
            Err(error)
        }
    }
}

/// What this caller sends for the node, decided under the store lock.
enum Claim {
    /// Another caller recorded an agent meanwhile.
    Agent(RunNode),
    Create {
        generation: u32,
        request: RunNodeRequest,
    },
}

enum Created {
    Agent(RunNode),
    /// Aether refused a generation's key; retry with a new key.
    Refused {
        generation: u32,
        error: ArchonError,
    },
}

/// A new generation's request records `network`; a stored request keeps
/// its own.
async fn create_generation(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    observed: &StoredNode,
    model: &ModelRef,
    network: NetworkPolicy,
) -> Result<Created, ArchonError> {
    let host_context = ctx
        .backend
        .host_context(ctx.config.aether.host_context.for_model(model))
        .await?;
    let repository_mode = if ctx.backend.supports_repository_root().await? {
        RepositoryMode::Root
    } else {
        RepositoryMode::Name
    };
    let claim = {
        let mut store = ctx.store.lock().await;
        claim_generation(
            &mut store,
            run,
            observed,
            model,
            network,
            host_context,
            repository_mode,
        )?
    };
    let (generation, request) = match claim {
        Claim::Agent(node) => return Ok(Created::Agent(node)),
        Claim::Create {
            generation,
            request,
        } => (generation, request),
    };
    assert_eq!(request.idempotency_key, idempotency_key(run, generation));
    let request = prepare_replay(ctx, run, generation, request).await?;
    send_generation(ctx, run, generation, request).await
}

async fn send_generation(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    generation: u32,
    request: RunNodeRequest,
) -> Result<Created, ArchonError> {
    assert_eq!(request.idempotency_key, idempotency_key(run, generation));
    let network = request.network;
    let repository = RepositoryIdentity::from_request(&request);
    repository.check_checkout()?;
    match ctx.backend.create_run_node(request).await {
        Ok(created) => {
            record_agent(&mut *ctx.store.lock().await, run, generation, &created)?;
            Ok(Created::Agent(RunNode {
                agent: created.agent,
                network,
                repository,
            }))
        }
        Err(error @ (ArchonError::Invalid(_) | ArchonError::NotFound(_))) => {
            Ok(Created::Refused { generation, error })
        }
        Err(error) => Err(error),
    }
}

async fn prepare_replay(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    generation: u32,
    mut request: RunNodeRequest,
) -> Result<RunNodeRequest, ArchonError> {
    assert_eq!(request.idempotency_key, idempotency_key(run, generation));
    let effective = ctx
        .backend
        .host_context(request.host_context.for_model(&request.model))
        .await?;
    if effective == request.host_context {
        return Ok(request);
    }
    assert!(effective.is_full(), "a frozen request can only downgrade");
    let mut store = ctx.store.lock().await;
    let mut current = load_node(&store, run)?;
    if current.generation != generation || current.agent.is_some() {
        return Ok(request);
    }
    let stored = current
        .request
        .as_ref()
        .expect("pending replay has a request");
    if stored.host_context == effective {
        return Ok(stored.clone());
    }
    assert_eq!(
        stored, &request,
        "only host context can change during replay"
    );
    request.host_context = effective;
    current.request = Some(request.clone());
    save_node(&mut store, run, &current)?;
    tracing::warn!(%run, generation, "lean host context unavailable for this engine or Aether; frozen node request falls back to full");
    Ok(request)
}

/// The request stored for the current generation, or a new generation's,
/// persisted before it is returned. `observed` is what the caller saw
/// before its liveness check; a node moved since is followed instead.
fn claim_generation(
    store: &mut Store,
    run: &PipelineRunId,
    observed: &StoredNode,
    model: &ModelRef,
    network: NetworkPolicy,
    host_context: archon_core::config::HostContext,
    repository_mode: RepositoryMode,
) -> Result<Claim, ArchonError> {
    let current = load_node(store, run)?;
    let moved = current != *observed;
    let generation = match (current.recorded(), &current.request) {
        (Some(node), _) if moved => return Ok(Claim::Agent(node)),
        // The dead node the caller saw: replace it.
        (Some(_), _) => current.generation + 1,
        (None, Some(request)) => {
            return Ok(Claim::Create {
                generation: current.generation,
                request: request.clone(),
            });
        }
        (None, None) => current.generation,
    };
    // Only a new run can choose roots; legacy generations keep their name identity.
    let repository_mode = match (repository_mode, current.generation) {
        (RepositoryMode::Root, 1) => RepositoryMode::Root,
        _ => RepositoryMode::Name,
    };
    let mut request = request(
        store,
        run,
        generation,
        model,
        network,
        current.repository_identity(),
        repository_mode,
    )?;
    request.host_context = host_context;
    let pending = StoredNode {
        agent: None,
        generation,
        request: Some(request.clone()),
        repository_identity: Some(RepositoryIdentity::from_request(&request)),
    };
    save_node(store, run, &pending)?;
    Ok(Claim::Create {
        generation,
        request,
    })
}

/// Record the agent Aether created for `generation`, and the Aether run
/// its stage agents will join, before any of them is sent.
fn record_agent(
    store: &mut Store,
    run: &PipelineRunId,
    generation: u32,
    created: &CreatedNode,
) -> Result<(), ArchonError> {
    let agent = &created.agent;
    index_node_agent(store, run, agent)?;
    record_aether_run(store, run, &created.run)?;
    let mut current = load_node(store, run)?;
    if current.generation != generation {
        tracing::warn!(%run, generation, %agent, "a later generation superseded this run node create");
        return Ok(());
    }
    if let Some(recorded) = &current.agent {
        assert_eq!(recorded, agent, "one key, one agent (Aether dedupes)");
        return Ok(());
    }
    assert!(
        current.request.is_some(),
        "a create was sent from a stored request"
    );
    current.agent = Some(agent.clone());
    save_node(store, run, &current)
}

/// The pipeline run joins its node's Aether run unless a stage agent
/// already recorded one (a root from before run nodes).
fn record_aether_run(
    store: &mut Store,
    run_id: &PipelineRunId,
    aether_run: &AetherRunId,
) -> Result<(), ArchonError> {
    let mut run = store.run(run_id)?;
    match &run.aether_run {
        None => {
            run.aether_run = Some(aether_run.clone());
            store.update_run(&run)
        }
        Some(joined) if joined == aether_run => Ok(()),
        Some(joined) => {
            tracing::warn!(run = %run_id, %joined, node_run = %aether_run, "run node sits in another Aether run");
            Ok(())
        }
    }
}

/// Leave `generation` behind after Aether refused its key; the next one
/// starts without a request. A node recorded or moved meanwhile stands.
async fn skip_generation(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    generation: u32,
) -> Result<StoredNode, ArchonError> {
    let mut store = ctx.store.lock().await;
    let current = load_node(&store, run)?;
    if current.generation != generation || current.agent.is_some() {
        return Ok(current);
    }
    let next = StoredNode {
        agent: None,
        generation: generation + 1,
        request: None,
        repository_identity: current.repository_identity(),
    };
    save_node(&mut store, run, &next)?;
    Ok(next)
}

/// The node's `create_agent` inputs: the run's worktree (a team run's
/// directory of worktrees) and `network` as scope, so every stage agent's
/// scope narrows it, and the item's title as name.
fn request(
    store: &Store,
    run_id: &PipelineRunId,
    generation: u32,
    model: &ModelRef,
    network: NetworkPolicy,
    identity: Option<RepositoryIdentity>,
    repository_mode: RepositoryMode,
) -> Result<RunNodeRequest, ArchonError> {
    let run = store.run(run_id)?;
    let item = store.item(&run.item)?;
    assert!(run.workspace.is_absolute(), "run workspaces are absolute");
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&resolve::repo_ref(id)))
        .transpose()?;
    let mut name = naming::short_title(&item.title);
    if name.is_empty() {
        name = format!("item {}", crate::worktree::item_short(&item.id));
    }
    let labels = [
        ("archon.item", item.id.to_string()),
        ("archon.run", run.id.to_string()),
        ("archon.pipeline", run.pipeline.clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let identity = identity.unwrap_or_else(|| match repo {
        Some(repo) if repository_mode == RepositoryMode::Root && run.aether_run.is_none() => {
            RepositoryIdentity::Root(repo.path)
        }
        repo => RepositoryIdentity::Name(repo.map(|repo| repo.name)),
    });
    let (repository, repository_root) = identity.parts();
    assert!(
        repository_root
            .as_ref()
            .is_none_or(|root| root.is_absolute())
    );
    Ok(RunNodeRequest {
        host_context: Default::default(),
        idempotency_key: idempotency_key(run_id, generation),
        role: format!("archon-run:{}", run.pipeline),
        name,
        model: model.clone(),
        workspace: run.team_root.clone().unwrap_or(run.workspace),
        repository,
        repository_root,
        network,
        labels,
        attach_run: run.aether_run,
    })
}

/// What `retire` found to cancel.
enum Found {
    Agent(AetherAgentId),
    Nothing,
    /// Cleanup could not be confirmed; retain the entry for the next sweep.
    Unanswered,
}

/// Cancel and forget the node of a finished run whose stage agents have
/// all settled. Aether's cancel cascades through the node's live subtree,
/// so it waits while a stage agent is still ending its turn (bounded by
/// `TURN_END_GRACE_SECS`). A generation whose create was never answered is
/// replayed under its key first, so a node Aether created behind a lost
/// reply is found and cancelled rather than leaked. Waiting and an
/// unanswered call both leave the entry for the next sweep
/// (`runs_to_retire`).
pub(super) async fn retire(ctx: &DaemonContext, run: &PipelineRunId) -> Result<(), ArchonError> {
    let node = load_node(&*ctx.store.lock().await, run)?;
    if turn_still_ending(ctx, run, archon_core::now()).await? {
        return Ok(());
    }
    let agent = match find_agent(ctx, run, node).await? {
        Found::Agent(agent) => Some(agent),
        Found::Nothing => None,
        Found::Unanswered => return Ok(()),
    };
    if let Some(agent) = agent
        && (!cancel_node(ctx, run, &agent).await? || !subtree_stopped(ctx, &agent).await?)
    {
        return Ok(());
    }
    // Older node generations may no longer enumerate their stage agents.
    // Keep the durable node entry until every recorded builder is confirmed
    // stopped, including kept agents and those beyond the turn-end grace.
    let stages = ctx.store.lock().await.stage_runs(run)?;
    for stage in stages {
        if let Some(agent) = stage.aether_agent
            && !subtree_stopped(ctx, &agent).await?
        {
            return Ok(());
        }
    }
    ctx.store.lock().await.kv_delete(&node_key(run))
}

/// Cancellation is cooperative. An unanswered lookup keeps the durable
/// retirement obligation; neither a timeout nor acceptance proves a stop.
async fn subtree_stopped(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    let stopped = async {
        if ctx.backend.is_live(agent).await? {
            return Ok(false);
        }
        let descendants = ctx.backend.descendants(agent).await?;
        Ok(descendants.iter().all(|d| d.state != DescendantState::Live))
    }
    .await;
    match stopped {
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => {
            tracing::info!(%agent, %error, "cannot confirm builders stopped; retaining run node");
            Ok(false)
        }
        other => other,
    }
}

/// Whether a stage agent of the finished run is still live in Aether: it
/// settled its task mid-turn and has not ended the turn yet. Only agents
/// within the grace after their own settlement are asked, newest first,
/// so the usual case (the last stage still ending) costs one `get_agent`
/// before cancellation. An agent Aether cannot be
/// asked about counts as live, so the wait is repeated.
async fn turn_still_ending(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    let (stages, parked) = {
        let store = ctx.store.lock().await;
        let stages = store.stage_runs(run)?;
        let parked = resume::parked(&store, &stages)?;
        (stages, parked)
    };
    assert!(
        stages.iter().all(|s| s.state.is_terminal()),
        "only a run with no live stage retires its node"
    );
    // A kept agent waits on its inbox until the node's cancel ends it (§6.7).
    let ending = agents_within_grace(&stages, now)
        .into_iter()
        .filter(|(_, agent)| !parked.contains(*agent));
    for (stage, agent) in ending {
        match ctx.backend.is_live(agent).await {
            Ok(false) => continue,
            Ok(true) => {
                tracing::debug!(%run, %stage, %agent, "stage agent still ending its turn; node retired later");
                return Ok(true);
            }
            Err(ArchonError::Unavailable(why)) => {
                tracing::info!(%run, %agent, %why, "stage agent state unknown; node retired later");
                return Ok(true);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// The agents of stages settled within `TURN_END_GRACE_SECS` of `now`,
/// most recently settled first: the only ones that can still hold the
/// node.
fn agents_within_grace(stages: &[StageRun], now: Timestamp) -> Vec<(&StageRunId, &AetherAgentId)> {
    let grace = chrono::Duration::seconds(TURN_END_GRACE_SECS);
    let mut recent: Vec<(Timestamp, &StageRun)> = stages
        .iter()
        .filter_map(|s| {
            let settled = s
                .finished_at
                .expect("checked: a terminal stage run has finished_at");
            (settled + grace >= now).then_some((settled, s))
        })
        .collect();
    recent.sort_by_key(|(settled, _)| std::cmp::Reverse(*settled));
    debug_assert!(recent.windows(2).all(|w| w[0].0 >= w[1].0));
    recent
        .into_iter()
        .filter_map(|(_, s)| Some((&s.id, s.aether_agent.as_ref()?)))
        .collect()
}

async fn find_agent(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    node: StoredNode,
) -> Result<Found, ArchonError> {
    node.check();
    let request = match (node.agent, node.request) {
        (Some(agent), _) => return Ok(Found::Agent(agent)),
        (None, Some(request)) => request,
        (None, None) => return Ok(Found::Nothing),
    };
    let request = prepare_replay(ctx, run, node.generation, request).await?;
    assert_eq!(
        request.idempotency_key,
        idempotency_key(run, node.generation)
    );
    let key = request.idempotency_key.clone();
    match ctx.backend.create_run_node(request).await {
        Ok(created) => {
            index_node_agent(&mut *ctx.store.lock().await, run, &created.agent)?;
            Ok(Found::Agent(created.agent))
        }
        Err(ArchonError::Invalid(why)) => {
            tracing::warn!(%run, key, %why, "Aether refused the pending node replay; abandoning its key");
            Ok(Found::Nothing)
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::debug!(%run, key, %why, "no run node exists under the pending key");
            Ok(Found::Nothing)
        }
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%run, key, %why, "run node replay unanswered; retired on a later sweep");
            Ok(Found::Unanswered)
        }
        Err(error) => Err(error),
    }
}

/// `true` once the node is gone: cancelled now, or already ended.
async fn cancel_node(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    agent: &AetherAgentId,
) -> Result<bool, ArchonError> {
    let reason = format!("pipeline run {run} finished");
    match ctx.backend.cancel(agent, &reason).await {
        Ok(()) => Ok(true),
        Err(ArchonError::NotFound(why) | ArchonError::InvalidTransition(why)) => {
            tracing::debug!(%run, %agent, %why, "run node already gone");
            Ok(true)
        }
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%run, %agent, %why, "run node cancel unanswered; retired on a later sweep");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_keyed_by_run_and_generation() {
        let run = PipelineRunId::new();
        let key = idempotency_key(&run, 2);
        assert_eq!(key, format!("{run}:node:2"));
        assert!(
            key.bytes().all(|b| (0x21..=0x7E).contains(&b)),
            "Aether keys are printable ASCII"
        );
        assert_eq!(run_of_key(&node_key(&run)).unwrap(), run);
        assert!(run_of_key("spawn_request:x").is_err());
    }

    #[test]
    fn a_missing_node_starts_at_generation_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("archon.db")).unwrap();
        let run = PipelineRunId::new();
        assert_eq!(load_node(&store, &run).unwrap(), StoredNode::fresh());
        let agent = AetherAgentId::parse("agt_1").unwrap();
        let saved = StoredNode {
            agent: Some(agent),
            generation: 3,
            request: Some(request_for(&run, 3)),
            repository_identity: None,
        };
        save_node(&mut store, &run, &saved).unwrap();
        assert_eq!(load_node(&store, &run).unwrap(), saved);
        store.kv_delete(&node_key(&run)).unwrap();
        assert_eq!(load_node(&store, &run).unwrap(), StoredNode::fresh());
    }

    fn request_for(run: &PipelineRunId, generation: u32) -> RunNodeRequest {
        RunNodeRequest {
            host_context: Default::default(),
            idempotency_key: idempotency_key(run, generation),
            role: "archon-run:p".into(),
            name: "n".into(),
            model: "claude-code/claude-sonnet-5".parse().unwrap(),
            workspace: "/w".into(),
            repository: None,
            repository_root: None,
            network: NetworkPolicy::Denied,
            labels: Default::default(),
            attach_run: None,
        }
    }

    #[test]
    fn old_node_records_keep_their_stored_repository_name() {
        let run = PipelineRunId::new();
        let mut request = request_for(&run, 1);
        request.repository = Some("original-name".into());
        let raw = serde_json::json!({
            "agent": "agt_1", "generation": 1, "request": request,
        });
        assert!(raw["request"].get("repository_root").is_none());
        let stored: StoredNode = serde_json::from_value(raw).unwrap();
        stored.check();
        assert_eq!(stored.repository_identity, None);
        assert_eq!(
            stored.repository_identity(),
            Some(RepositoryIdentity::Name(Some("original-name".into())))
        );
        assert_eq!(
            stored.recorded().unwrap().repository.parts(),
            (Some("original-name".into()), None)
        );
    }

    #[test]
    fn a_stored_root_node_round_trips_through_json() {
        let run = PipelineRunId::new();
        let root = PathBuf::from("/registered/checkout");
        let mut request = request_for(&run, 2);
        request.repository_root = Some(root.clone());
        let stored = StoredNode {
            agent: Some(AetherAgentId::parse("agt_1").unwrap()),
            generation: 2,
            request: Some(request),
            repository_identity: Some(RepositoryIdentity::Root(root.clone())),
        };
        let raw = serde_json::to_string(&stored).unwrap();
        let restored: StoredNode = serde_json::from_str(&raw).unwrap();
        restored.check();
        assert_eq!(restored, stored);
        assert_eq!(
            restored.recorded().unwrap().repository.parts(),
            (None, Some(root))
        );
    }

    #[test]
    fn a_frozen_root_must_still_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        let identity = RepositoryIdentity::Root(root.clone());
        let missing = identity.check_checkout().unwrap_err().to_string();
        assert!(missing.contains(&root.display().to_string()), "{missing}");
        assert!(missing.contains("is missing"), "{missing}");
        assert!(
            missing.contains("restore it there and retry the item"),
            "{missing}"
        );
        assert!(missing.contains("a re-registered path applies to new runs"));
        std::fs::write(&root, "a file cannot be a checkout").unwrap();
        let file = identity.check_checkout().unwrap_err().to_string();
        assert!(file.contains("no longer a directory"), "{file}");
        assert!(
            RepositoryIdentity::Root(dir.path().to_owned())
                .check_checkout()
                .is_ok()
        );
        assert!(
            RepositoryIdentity::Name(Some("legacy".into()))
                .check_checkout()
                .is_ok()
        );
    }

    #[test]
    fn a_recorded_node_reports_the_network_its_request_stated() {
        let run = PipelineRunId::new();
        let agent = AetherAgentId::parse("agt_1").unwrap();
        let node = StoredNode {
            agent: Some(agent.clone()),
            generation: 1,
            request: Some(request_for(&run, 1)),
            repository_identity: None,
        };
        node.check();
        assert_eq!(
            node.recorded(),
            Some(RunNode {
                agent,
                network: NetworkPolicy::Denied,
                repository: RepositoryIdentity::Name(None),
            })
        );
        assert_eq!(StoredNode::fresh().recorded(), None);
    }

    fn settled_stage(stage: &str, agent: &str, finished: Timestamp) -> StageRun {
        StageRun {
            id: archon_core::id::StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: stage.to_owned(),
            kind: None,
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            choice: archon_core::model::ModelChoice {
                model: "claude-code/claude-sonnet-5".parse().unwrap(),
                effort: archon_core::model::Effort::Medium,
                tier: None,
            },
            aether_agent: Some(AetherAgentId::parse(agent).unwrap()),
            aether_task: None,
            state: archon_core::run::StageState::Completed,
            verdict: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at: finished - chrono::Duration::minutes(1),
            finished_at: Some(finished),
        }
    }

    /// Only the agents that settled within the grace are asked, the most
    /// recently settled first, so a long run's history is never queried.
    #[test]
    fn only_recently_settled_agents_are_asked_newest_first() {
        let now = archon_core::now();
        let seconds = chrono::Duration::seconds;
        let stages = vec![
            settled_stage(
                "implement",
                "agt_old",
                now - seconds(TURN_END_GRACE_SECS + 1),
            ),
            settled_stage("review", "agt_edge", now - seconds(TURN_END_GRACE_SECS)),
            settled_stage("ship", "agt_last", now - seconds(1)),
            settled_stage("test", "agt_mid", now - seconds(30)),
        ];
        let asked: Vec<&str> = agents_within_grace(&stages, now)
            .into_iter()
            .map(|(_, agent)| agent.as_str())
            .collect();
        assert_eq!(asked, ["agt_last", "agt_mid", "agt_edge"]);
        let later = now + seconds(TURN_END_GRACE_SECS + 2);
        assert!(agents_within_grace(&stages, later).is_empty());
    }

    #[test]
    #[should_panic(expected = "another generation")]
    fn a_request_must_match_its_generation() {
        let run = PipelineRunId::new();
        StoredNode {
            agent: None,
            generation: 2,
            request: Some(request_for(&run, 1)),
            repository_identity: None,
        }
        .check();
    }

    #[test]
    #[should_panic(expected = "keeps the request")]
    fn a_recorded_node_without_its_request_is_a_bug() {
        StoredNode {
            agent: Some(AetherAgentId::parse("agt_1").unwrap()),
            generation: 1,
            request: None,
            repository_identity: None,
        }
        .check();
    }
}
