//! Policy for agents Archon did not create itself: children delegated by a
//! stage agent, the orchestrator, or another such child. A child outside the
//! model policy, delegated too deep, or (a helper of the orchestrator) placed
//! in a repository or worktree is cancelled and reported (ARCHITECTURE §7.4).
//! A child is owned before it is checked, so its spend, and its subtree's,
//! never depends on the check succeeding. A watched child is forgotten once
//! none of its events can still arrive.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use archon_aether::AetherEvent;
use archon_aether::wire::Command;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::model::{Effort, ModelChoice, ModelRef};
use archon_core::notify::NotificationKind;
use archon_core::policy::ModelPolicy;
use archon_core::protocol::RepoRef;
use archon_queue::Store;
use serde_json::Value;

use super::fleet;
use crate::context::DaemonContext;
use crate::mirror::{self, Reach};
use crate::triggers::{self, TriggerEvent};
use crate::{executor, feed};

const CHILD_KEY_PREFIX: &str = "link_child_";
const STAGE_AGENT_KEY_PREFIX: &str = "link_stage_agent_";
/// Aether calls one pruning pass may make about the orchestrator's children.
const MAX_LIVENESS_CHECKS: usize = 32;

/// `kv`: a child Archon watches, holding its item as JSON.
pub(super) fn child_key(agent: &AetherAgentId) -> String {
    format!("{CHILD_KEY_PREFIX}{agent}")
}

/// `kv`: when a pruning pass first found an orchestrator's child ended.
fn ended_key(agent: &AetherAgentId) -> String {
    format!("link_ended_{agent}")
}

/// `kv`: set with `child_key` and cleared once the child's verdict is
/// reached, so a retried `agent.created` checks it again.
fn unchecked_key(agent: &AetherAgentId) -> String {
    format!("link_unchecked_{agent}")
}

/// `kv`: set once a child was cancelled for its model, so a replay does not
/// report it twice.
fn cancelled_key(agent: &AetherAgentId) -> String {
    format!("link_cancelled_{agent}")
}

/// `kv`: how many delegations a watched child is below a stage agent or the
/// orchestrator, neither of which has an entry.
fn depth_key(agent: &AetherAgentId) -> String {
    format!("link_depth_{agent}")
}

/// `kv`: an agent Aether created under a run node, which is a stage agent,
/// holding its item. Its children are placed through it before the spawn
/// reply that records the stage agent has been handled.
pub(super) fn stage_agent_key(agent: &AetherAgentId) -> String {
    format!("{STAGE_AGENT_KEY_PREFIX}{agent}")
}

pub(super) async fn agent_created(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    model: &ModelRef,
) -> Result<(), ArchonError> {
    let Some(parent) = &event.parent else {
        return Ok(());
    };
    assert_ne!(parent, agent, "an agent is not its own parent");
    let node_run = executor::node_run(&*ctx.store.lock().await, parent)?;
    if let Some(run) = node_run {
        // Archon's own stage agent: its model was checked at spawn.
        return record_stage_agent(ctx, agent, &run).await;
    }
    let Some(item) = parent_item(ctx, parent).await? else {
        return Ok(());
    };
    {
        let mut store = ctx.store.lock().await;
        if let Some(run) = store.agent_run(parent)? {
            store.link_agent_run(agent, &run)?;
            // Its stage's cost includes it; the effort is read when the stage ends.
            store.record_delegated(agent, parent, model, None)?;
        }
    }
    if already_checked(ctx, agent).await? {
        return Ok(());
    }
    let depth = adopt(ctx, agent, parent, item.as_ref()).await?;
    let most = ctx.config.limits.max_delegation_depth;
    let refusal = match depth_refusal(depth, most, item.is_none()) {
        Some(refusal) => Some(refusal),
        None => violation(ctx, agent, model, item.as_ref()).await?,
    };
    match refusal {
        Some(refusal) => cancel_child(ctx, agent, model, item, &refusal).await,
        None => ctx.store.lock().await.kv_delete(&unchecked_key(agent)),
    }
}

/// Every stage agent's `agent.created` precedes its children's in the
/// stream, so recording it here places them even when they are created
/// before `spawned_ok` records the stage.
async fn record_stage_agent(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    run: &PipelineRunId,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let item = store.run(run)?.item;
    store.link_agent_run(agent, run)?;
    let key = stage_agent_key(agent);
    if let Some(recorded) = store.kv_get(&key)? {
        assert_eq!(recorded, item.as_str(), "a stage agent works for one item");
        return Ok(());
    }
    store.kv_set(&key, item.as_str())
}

/// The item a new child works for, when its parent is Archon's: a stage agent,
/// a watched child, or an orchestrator (no item), also one since replaced.
async fn parent_item(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<Option<QueueItemId>>, ArchonError> {
    if let Some(owner) = fleet::owner(ctx, parent).await? {
        return Ok(Some(fleet::item_of(ctx, &owner).await?));
    }
    let orchestrator = crate::supervisor::is_own_orchestrator(ctx, parent).await?;
    Ok(orchestrator.then_some(None))
}

/// Stage agents were checked when Archon spawned them; a replayed
/// `agent.created` finds its first verdict. A child still `unchecked` has
/// none yet: its check failed with Aether unreachable and is retried.
async fn already_checked(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    let store = ctx.store.lock().await;
    if store.stage_run_by_agent(agent)?.is_some() || store.kv_get(&cancelled_key(agent))?.is_some()
    {
        return Ok(true);
    }
    let owned = store.kv_get(&child_key(agent))?.is_some();
    Ok(owned && store.kv_get(&unchecked_key(agent))?.is_none())
}

/// Record the child of `parent` as Archon's, pending its verdict. Returns how
/// many delegations it is below a stage agent or the orchestrator.
async fn adopt(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    parent: &AetherAgentId,
    item: Option<&QueueItemId>,
) -> Result<u32, ArchonError> {
    let raw = serde_json::to_string(&item)?;
    let mut store = ctx.store.lock().await;
    if let Some(owned) = store.kv_get(&child_key(agent))? {
        assert_eq!(owned, raw, "a child works for one item");
    }
    let depth = depth_of(&store, parent)?.saturating_add(1);
    assert!(depth >= 1, "a delegated child is below its parent");
    store.kv_set(&child_key(agent), &raw)?;
    store.kv_set(&depth_key(agent), &depth.to_string())?;
    store.kv_set(&unchecked_key(agent), "1")?;
    Ok(depth)
}

/// 0 for a stage agent or an orchestrator: neither is a watched child.
fn depth_of(store: &Store, agent: &AetherAgentId) -> Result<u32, ArchonError> {
    let key = depth_key(agent);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(0);
    };
    raw.parse()
        .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))
}

/// Why a child `depth` delegations down may not run, if it is too deep.
fn depth_refusal(depth: u32, most: u32, helper: bool) -> Option<String> {
    assert!(depth >= 1, "a watched child is below its top agent");
    assert!(most >= 1, "validated with the config");
    if depth <= most {
        return None;
    }
    let top = if helper {
        "the orchestrator"
    } else {
        "its stage agent"
    };
    Some(format!(
        "it is {depth} delegations below {top}; `[limits] max_delegation_depth` allows {most} \
         (an agent {most} down is delegated with `leaf: true`)"
    ))
}

/// Why the child may not run: it breaks its model policy, or it is the
/// orchestrator's helper (no `item`) placed inside a repository.
async fn violation(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    model: &ModelRef,
    item: Option<&QueueItemId>,
) -> Result<Option<String>, ArchonError> {
    let Some(item) = item else {
        return helper_violation(ctx, agent, model).await;
    };
    let policy = item_policy(ctx, item).await?;
    let reviewers = ctx.store.lock().await.item(item)?.model_plan;
    let Some(named) = reviewers.reviewer_choices() else {
        return policy_violation(ctx, agent, model, &policy).await;
    };
    let held = held_by_aether(ctx, agent).await?;
    let refusal = match &held {
        Some(held) => policy_refusal(&policy, model, held),
        None => model_refusal(&policy, model),
    };
    Ok(match refusal {
        Some(refusal) => Some(refusal.to_string()),
        None => held.and_then(|held| reviewer_refusal(named, model, &held)),
    })
}

/// Why a child of an item with no reviewer plan breaks the model policy.
async fn policy_violation(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    model: &ModelRef,
    policy: &ModelPolicy,
) -> Result<Option<String>, ArchonError> {
    let of_model = || model_refusal(policy, model).map(|refusal| refusal.to_string());
    if policy.max_effort.is_none() {
        return Ok(of_model());
    }
    let Some(held) = held_by_aether(ctx, agent).await? else {
        return Ok(of_model());
    };
    Ok(policy_refusal(policy, model, &held).map(|refusal| refusal.to_string()))
}

/// Why a reviewer child runs on neither a model nor an effort the plan names
/// for reviewers. A child whose role is no reviewer's is not held to it.
fn reviewer_refusal(named: &[ModelChoice], model: &ModelRef, held: &Value) -> Option<String> {
    let role = held.get("role").and_then(Value::as_str).unwrap_or_default();
    if !role.to_ascii_lowercase().contains("review") {
        return None;
    }
    let effort = match child_effort(held) {
        ChildEffort::Known(effort) => Some(effort),
        ChildEffort::Unknown | ChildEffort::Unrecognised(_) => None,
    };
    let allowed = named
        .iter()
        .any(|c| c.model == *model && effort.is_none_or(|e| e <= c.effort));
    if allowed {
        return None;
    }
    let listed: Vec<String> = named
        .iter()
        .map(|c| format!("{} at effort up to {}", c.model, c.effort))
        .collect();
    Some(format!(
        "reviewer on {model} breaks the item's reviewer plan; reviewers run on: {}",
        listed.join(", ")
    ))
}

/// Global policy narrowed by the item's repository, as for its stages.
async fn item_policy(ctx: &DaemonContext, item: &QueueItemId) -> Result<ModelPolicy, ArchonError> {
    let repo = {
        let store = ctx.store.lock().await;
        match store.item(item)?.repo {
            Some(id) => Some(store.repo(&RepoRef(id.as_str().to_owned()))?),
            None => None,
        }
    };
    let global = ctx.config.models.policy();
    let Some(repo) = repo else {
        return Ok(global);
    };
    match RepoConfig::load(&repo.path) {
        Ok(config) => Ok(global.narrowed_by(&config.models.policy())),
        // The dispatcher refuses such a repo's new work; keep watching
        // without its narrowing rather than stop watching.
        Err(error) => {
            tracing::warn!(repo = %repo.name, %error, "repo config unreadable; its policy is not applied");
            Ok(global)
        }
    }
}

/// Why an orchestrator's helper may not run: its scope leads into a repository
/// or worktree, or it breaks the policy as the repositories it reads narrow it.
async fn helper_violation(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    model: &ModelRef,
) -> Result<Option<String>, ArchonError> {
    let held = held_by_aether(ctx, agent).await?;
    let roots = held.as_ref().map(scope_roots).unwrap_or_default();
    let reach = mirror_reach(ctx, roots.clone()).await?;
    let policy = mirror::helper_policy(ctx, &reach).await?;
    let refusal = match &held {
        Some(held) => policy_refusal(&policy, model, held),
        None => model_refusal(&policy, model),
    };
    if let Some(refusal) = refusal {
        return Ok(Some(format!(
            "{refusal}. A helper follows the policy of each repository whose mirror its \
             workspace holds, here {}; `workspace_root` of one mirror, or of `scratch`, \
             narrows that",
            reach.describe()
        )));
    }
    placed_in_a_repository(ctx, roots).await
}

/// Which mirrors the scope `roots` read. No root (the agent is gone, or
/// Aether names none) reads every mirror, as far as Archon can tell.
async fn mirror_reach(ctx: &DaemonContext, roots: Vec<PathBuf>) -> Result<Reach, ArchonError> {
    let workspace = ctx.paths.orchestrator_workspace.clone();
    let git_dirs = mirror::git_dirs_root(&ctx.paths.data_dir);
    tokio::task::spawn_blocking(move || mirror::reach(&workspace, &git_dirs, &roots))
        .await
        .map_err(|e| ArchonError::io("the mirror reach check", std::io::Error::other(e)))
}

/// Why the child's model or effort breaks `policy`, if either does.
fn policy_refusal(policy: &ModelPolicy, model: &ModelRef, held: &Value) -> Option<ArchonError> {
    let Some(cap) = policy.max_effort else {
        return model_refusal(policy, model);
    };
    let effort = match child_effort(held) {
        ChildEffort::Known(effort) => effort,
        ChildEffort::Unknown => return model_refusal(policy, model),
        // A cap the effort cannot be compared with is not met.
        ChildEffort::Unrecognised(raw) => {
            let refusal = ArchonError::Policy(format!(
                "effort {raw:?} is not one Archon can compare with the configured maximum {cap}"
            ));
            return model_refusal(policy, model).or(Some(refusal));
        }
    };
    let choice = ModelChoice {
        model: model.clone(),
        effort,
    };
    policy.check(&choice).err()
}

fn model_refusal(policy: &ModelPolicy, model: &ModelRef) -> Option<ArchonError> {
    if policy.permits_model(model) {
        return None;
    }
    Some(ArchonError::Policy(format!(
        "model {model} is not allowed; allowed: {}",
        policy.describe_allowed()
    )))
}

/// What the child's config says about its effort.
#[derive(Debug, PartialEq)]
enum ChildEffort {
    Known(Effort),
    /// No effort in its config: only its model is checked.
    Unknown,
    Unrecognised(String),
}

/// The agent as Aether holds it. `None` when it is gone already (it ended,
/// or this is a replay after it was reaped): only its model is checked.
async fn held_by_aether(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<Value>, ArchonError> {
    let client = ctx.aether.client()?;
    let command = Command::GetAgent { id: agent.clone() };
    match client.request(command, "agent").await {
        Ok(held) => Ok(Some(held)),
        Err(ArchonError::NotFound(why)) => {
            tracing::info!(%agent, %why, "child gone before Aether was asked about it; checking its model only");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn child_effort(held: &Value) -> ChildEffort {
    let Some(raw) = held.pointer("/config/effort").and_then(Value::as_str) else {
        return ChildEffort::Unknown;
    };
    match Effort::from_str(raw) {
        Ok(effort) => ChildEffort::Known(effort),
        Err(_) => ChildEffort::Unrecognised(raw.to_owned()),
    }
}

/// The refusal of an orchestrator's helper (§4.4) whose workspace or allowed
/// paths lead into a repository or worktree: about its place, not its model.
/// Its mirrors are inside the orchestrator's workspace, which is its.
async fn placed_in_a_repository(
    ctx: &DaemonContext,
    roots: Vec<PathBuf>,
) -> Result<Option<String>, ArchonError> {
    if roots.is_empty() {
        return Ok(None);
    }
    let mut closed = vec![(
        "the items' worktrees".to_owned(),
        ctx.paths.worktrees.clone(),
    )];
    for repo in ctx.store.lock().await.repos()? {
        closed.push((format!("repository `{}`", repo.name), repo.path));
    }
    let own = ctx.paths.orchestrator_workspace.clone();
    let found = tokio::task::spawn_blocking(move || closed_place_holding(&roots, &own, &closed))
        .await
        .map_err(|e| ArchonError::io("the workspace check", std::io::Error::other(e)))?;
    Ok(found.map(|(root, place)| {
        format!(
            "it was given {}, which is inside {place}; only a queued item's agents work \
             there. A helper reads a repository in its mirror under `repos/`",
            root.display()
        )
    }))
}

/// Every directory the agent's scope lets its tools act in.
fn scope_roots(held: &Value) -> Vec<PathBuf> {
    let path = |value: &Value| value.as_str().map(PathBuf::from);
    let root = held.pointer("/scope/workspace_root").and_then(path);
    let allowed = held.pointer("/scope/allowed_paths");
    let allowed = allowed.and_then(Value::as_array).into_iter().flatten();
    root.into_iter().chain(allowed.filter_map(path)).collect()
}

/// The first of `roots` that leads into a `closed` place, with its name. Links
/// are followed; what stays inside `own`, the orchestrator's workspace, is its.
fn closed_place_holding(
    roots: &[PathBuf],
    own: &Path,
    closed: &[(String, PathBuf)],
) -> Option<(PathBuf, String)> {
    // A path that does not exist leads nowhere else: compared as written.
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let own = resolved(own);
    let closed: Vec<(&String, PathBuf)> = closed.iter().map(|(n, p)| (n, resolved(p))).collect();
    for root in roots {
        assert!(root.is_absolute(), "Aether scopes are absolute: {root:?}");
        let at = resolved(root);
        if at.starts_with(&own) {
            continue;
        }
        if let Some((name, _)) = closed.iter().find(|(_, place)| at.starts_with(place)) {
            return Some((root.clone(), (*name).clone()));
        }
    }
    None
}

async fn cancel_child(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    model: &ModelRef,
    item: Option<QueueItemId>,
    refusal: &str,
) -> Result<(), ArchonError> {
    let client = ctx.aether.client()?;
    match client
        .cancel(agent, &format!("archon policy: {refusal}"))
        .await
    {
        Ok(()) | Err(ArchonError::NotFound(_) | ArchonError::InvalidTransition(_)) => {}
        Err(error) => return Err(error),
    }
    let text = format!("Cancelled agent {agent}: {refusal}");
    feed::notify(ctx, NotificationKind::Blocker, text, item, None).await?;
    let violation = TriggerEvent::PolicyViolation {
        agent: agent.to_string(),
        model: model.to_string(),
        action: "cancelled".into(),
    };
    triggers::push(ctx, violation).await?;
    // Still Archon's (`adopt`): its spend so far, and its subtree's, counts.
    let mut store = ctx.store.lock().await;
    store.kv_set(&cancelled_key(agent), "1")?;
    store.kv_delete(&unchecked_key(agent))
}

/// Forget watched children whose events can no longer arrive: those of an
/// item that finished before `cutoff`, and the orchestrator's once Aether
/// reported them ended before `cutoff`. A child's entry attributes its
/// spend and questions and lets its own children be checked, so it is kept
/// through the retention window. Returns how many were forgotten.
pub(crate) async fn prune_children(
    ctx: &DaemonContext,
    now: Timestamp,
    cutoff: Timestamp,
) -> Result<usize, ArchonError> {
    assert!(cutoff <= now, "the cutoff lies in the past");
    let mut checks_left = MAX_LIVENESS_CHECKS;
    let mut pruned = 0;
    for (agent, item) in watched_children(ctx).await? {
        let done = match &item {
            Some(item) => item_finished_before(ctx, item, cutoff).await?,
            None => ended_before(ctx, &agent, now, cutoff, &mut checks_left).await?,
        };
        if done {
            forget_child(ctx, &agent).await?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

async fn watched_children(
    ctx: &DaemonContext,
) -> Result<Vec<(AetherAgentId, Option<QueueItemId>)>, ArchonError> {
    let store = ctx.store.lock().await;
    let mut children = Vec::new();
    for (key, raw) in store.kv_prefix(CHILD_KEY_PREFIX)? {
        let id = key
            .strip_prefix(CHILD_KEY_PREFIX)
            .expect("ranged by prefix");
        let agent = AetherAgentId::parse(id)
            .map_err(|e| ArchonError::Store(format!("kv key {key}: {e}")))?;
        let item = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))?;
        children.push((agent, item));
    }
    Ok(children)
}

/// The item finished before `cutoff`, or no longer exists.
async fn item_finished_before(
    ctx: &DaemonContext,
    item: &QueueItemId,
    cutoff: Timestamp,
) -> Result<bool, ArchonError> {
    match ctx.store.lock().await.item(item) {
        Ok(found) => Ok(found.state.is_terminal() && found.updated_at < cutoff),
        Err(ArchonError::NotFound(_)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether Aether reported the orchestrator's child ended before `cutoff`.
/// The first pass to find it ended records `now`; an unanswered question
/// is asked again by a later pass.
async fn ended_before(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    now: Timestamp,
    cutoff: Timestamp,
    checks_left: &mut usize,
) -> Result<bool, ArchonError> {
    let key = ended_key(agent);
    if let Some(raw) = ctx.store.lock().await.kv_get(&key)? {
        let ended: Timestamp = raw
            .parse()
            .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))?;
        return Ok(ended < cutoff);
    }
    if *checks_left == 0 {
        return Ok(false);
    }
    *checks_left -= 1;
    match ctx.backend.is_live(agent).await {
        Ok(true) => {}
        Ok(false) => ctx.store.lock().await.kv_set(&key, &now.to_rfc3339())?,
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::info!(%agent, %error, "cannot ask Aether about a watched child; asking next pass");
            *checks_left = 0;
        }
    }
    Ok(false)
}

async fn forget_child(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    store.kv_delete(&child_key(agent))?;
    store.kv_delete(&unchecked_key(agent))?;
    store.kv_delete(&cancelled_key(agent))?;
    store.kv_delete(&depth_key(agent))?;
    store.kv_delete(&ended_key(agent))
}

/// Forget stage agents recorded at `agent.created` whose item finished
/// before `cutoff`. A recorded stage agent is also found through its stage
/// run, so the entry only matters while the item's events may still arrive.
/// Returns how many were forgotten.
pub(crate) async fn prune_stage_agents(
    ctx: &DaemonContext,
    cutoff: Timestamp,
) -> Result<usize, ArchonError> {
    let entries = ctx.store.lock().await.kv_prefix(STAGE_AGENT_KEY_PREFIX)?;
    let mut pruned = 0;
    for (key, raw) in entries {
        let id = key
            .strip_prefix(STAGE_AGENT_KEY_PREFIX)
            .expect("ranged by prefix");
        AetherAgentId::parse(id).map_err(|e| ArchonError::Store(format!("kv key {key}: {e}")))?;
        let item = QueueItemId::parse(&raw)
            .map_err(|e| ArchonError::Store(format!("kv {key} = {raw:?}: {e}")))?;
        if item_finished_before(ctx, &item, cutoff).await? {
            ctx.store.lock().await.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}
