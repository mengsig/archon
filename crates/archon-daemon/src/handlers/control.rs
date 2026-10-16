//! Daemon control: status, pause and resume, caps, the catalog, the model
//! policy and the orchestrator.

use std::collections::BTreeSet;

use archon_aether::wire::Agent;
use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::Directive;
use archon_core::config::{AetherMode, RepoConfig};
use archon_core::error::ArchonError;
use archon_core::github::GithubWatch;
use archon_core::protocol::{
    AetherLinkStatus, ClientKind, DaemonStatus, DiskStatus, ItemQuery, ItemSummary,
    MAX_WAITING_PAGE, McpHealth, McpStatus, OrchestratorInfo, PauseTarget, Reply, RepoRef,
    StatusHealth, StatusQuery, WAITING_PAGE,
};
use archon_core::queue::{Caps, PauseState};
use archon_core::run::StageState;
use archon_queue::store::keys;

use crate::context::DaemonContext;
use crate::resolve::{self, RepoEnv};
use crate::status_health::{Probe, Targets};
use crate::{dispatcher, github, ops, supervisor, triggers};

/// Set while `pause --hard` holds running stage agents paused in Aether.
const HARD_PAUSE_KEY: &str = "global_pause_hard";

pub(super) async fn status(ctx: &DaemonContext, query: StatusQuery) -> Result<Reply, ArchonError> {
    let waiting = waiting_page(ctx, query).await?;
    let caps = resolve::caps(ctx).await;
    let autonomy = RepoEnv::new(None)?.autonomy(ctx);
    let counts = StoreCounts::read(ctx).await?;
    let aether = AetherSections::read(ctx).await?;
    let snapshot = counts.snapshot;
    let status = DaemonStatus {
        waiting_items: waiting.items,
        waiting_total: count(waiting.total),
        waiting_next_offset: waiting.next_offset.map(count),
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        started_at: ctx.started_at,
        aether: link_status(ctx),
        orchestrator: aether.orchestrator,
        caps,
        pause: snapshot.pause,
        running_agents: snapshot.running_total,
        active_runs: snapshot.active_runs,
        queued_items: count(snapshot.queued.len()),
        blocked_items: count(counts.blocked),
        open_questions: count(counts.open_questions),
        spend_today_usd: snapshot.spend_today_usd,
        spend_total_usd: snapshot.spend_total_usd,
        lavish_available: ctx.lavish.is_some(),
        autonomy,
        directives: counts.directives,
        open_gates: count(counts.open_gates),
        held_news: count(triggers::held_news(ctx).await?),
        mcp: Some(aether.mcp),
        github: aether.github,
        disk: Some(disk_status(ctx).await?),
        versions: Some(ops::versions(ctx).await),
        upgrade: ops::phase(ctx).await,
        merges_waiting: counts.merges.waiting,
        merge_granted: counts.merges.granted,
        health: Some(aether.health),
    };
    assert!(
        status.spend_today_usd <= status.spend_total_usd + 1e-9,
        "today is part of the total"
    );
    Ok(Reply::Status(Box::new(status)))
}

/// What `status` counts in the store, under one lock.
struct StoreCounts {
    snapshot: archon_queue::dispatch::DispatchSnapshot,
    blocked: usize,
    open_questions: usize,
    directives: Vec<Directive>,
    open_gates: usize,
    merges: super::autonomy::MergesNow,
}

impl StoreCounts {
    async fn read(ctx: &DaemonContext) -> Result<Self, ArchonError> {
        let store = ctx.store.lock().await;
        let now = archon_core::now();
        let mut snapshot = store.dispatch_snapshot(now)?;
        if !ctx.config.limits.count_estimated_cost {
            (snapshot.spend_today_usd, snapshot.spend_total_usd) =
                store.spend_totals(now, false)?;
        }
        let blocked = store.items(&ItemQuery {
            states: vec!["blocked".into()],
            limit: Some(usize::MAX),
            ..ItemQuery::default()
        })?;
        Ok(Self {
            snapshot,
            blocked: blocked.len(),
            open_questions: store.questions(true)?.len(),
            directives: store.directives(now)?,
            open_gates: store.open_gates()?.len(),
            merges: super::autonomy::merges_now(&store)?,
        })
    }
}

/// One page of the items with a waiting reason.
struct WaitingPage {
    items: Vec<ItemSummary>,
    total: usize,
    next_offset: Option<usize>,
}

async fn waiting_page(ctx: &DaemonContext, query: StatusQuery) -> Result<WaitingPage, ArchonError> {
    let limit = query.waiting_limit.unwrap_or(WAITING_PAGE);
    if !(1..=MAX_WAITING_PAGE).contains(&limit) {
        return Err(ArchonError::Invalid(format!(
            "waiting_limit must be 1 to {MAX_WAITING_PAGE}, not {limit}"
        )));
    }
    let candidates = ItemQuery {
        states: vec!["queued".into(), "blocked".into(), "running".into()],
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    let waiting: Vec<ItemSummary> = super::item_views::read_summaries(ctx, &candidates)
        .await?
        .into_iter()
        .filter(|row| row.waiting_reason.is_some())
        .collect();
    let total = waiting.len();
    let items: Vec<ItemSummary> = waiting
        .into_iter()
        .skip(query.waiting_offset)
        .take(limit)
        .collect();
    let end = query.waiting_offset.saturating_add(items.len());
    assert!(items.len() <= limit, "a page holds at most its limit");
    assert!(
        items.is_empty() || end <= total,
        "a page ends within the list"
    );
    Ok(WaitingPage {
        items,
        total,
        next_offset: (end < total).then_some(end),
    })
}

/// The sections `status` reads from Aether's latest answers, never live.
struct AetherSections {
    orchestrator: Option<OrchestratorInfo>,
    mcp: McpStatus,
    github: Option<GithubWatch>,
    health: StatusHealth,
}

impl AetherSections {
    async fn read(ctx: &DaemonContext) -> Result<Self, ArchonError> {
        let recorded = recorded_orchestrator(ctx).await?;
        let targets = Targets {
            orchestrator: recorded.as_ref().map(|(agent, _)| agent.clone()),
            github: ctx.config.aether.mode == AetherMode::Dedicated,
        };
        let answers = ctx
            .status_probes
            .read(ctx.aether.client(), targets.clone())
            .await;
        let orchestrator = recorded.map(|(agent, generation)| {
            let found = cached_agent(&answers.aether, &agent);
            orchestrator_view(ctx, agent, generation, found)
        });
        let mcp_health = match (&answers.mcp.last_error, &answers.mcp.value) {
            (Some(error), _) => McpHealth::Unreachable {
                error: error.clone(),
            },
            (None, Some(health)) => health.clone(),
            (None, None) => McpHealth::Unreachable {
                error: NOT_ANSWERED.to_owned(),
            },
        };
        Ok(Self {
            orchestrator,
            mcp: supervisor::heal_status(ctx, mcp_health).await,
            github: github::status(ctx, &answers.github).await?,
            health: answers.health(targets.github),
        })
    }
}

/// Said for a section before Aether ever answered for it.
const NOT_ANSWERED: &str = "Aether has not answered yet";

/// The orchestrator's record as Aether last gave it, or why it is unknown.
fn cached_agent(reading: &Probe<Option<Agent>>, agent: &AetherAgentId) -> Result<Agent, String> {
    if let Some(error) = &reading.last_error {
        return Err(error.clone());
    }
    match &reading.value {
        Some(Some(found)) if found.id == *agent => Ok(found.clone()),
        _ => Err(NOT_ANSWERED.to_owned()),
    }
}

/// Free space where archond writes, read now, with the disk hold.
async fn disk_status(ctx: &DaemonContext) -> Result<DiskStatus, ArchonError> {
    let (path, free_bytes, error) = match dispatcher::read_disk(ctx).await {
        Ok(reading) => (reading.path, Some(reading.free_bytes), None),
        Err(error) => (ctx.paths.data_dir.clone(), None, Some(error.to_string())),
    };
    let status = DiskStatus {
        path,
        free_bytes,
        error,
        min_free_bytes: ctx.config.limits.min_free_bytes(),
        held: ctx.disk.holding(),
        disk_full: ctx.disk.disk_full(),
        standby: ctx.standby.get().cloned(),
        gc: crate::gc::last_pass(&*ctx.store.lock().await)?,
        storage: crate::storage::usage(ctx).await?,
    };
    assert_ne!(status.free_bytes.is_some(), status.error.is_some());
    assert!(
        !status.path.as_os_str().is_empty(),
        "the status names a path"
    );
    Ok(status)
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn link_status(ctx: &DaemonContext) -> AetherLinkStatus {
    match ctx.aether.client() {
        Ok(client) => {
            let welcome = client.welcome();
            AetherLinkStatus::Connected {
                daemon_version: welcome.daemon_version.clone(),
                protocol_version: welcome.protocol_version,
                instance_id: welcome.instance_id.clone(),
            }
        }
        Err(error) => AetherLinkStatus::Disconnected {
            error: error.to_string(),
            retry_at: None,
        },
    }
}

/// The recorded orchestrator and its generation; `None` before it was
/// ever created.
async fn recorded_orchestrator(
    ctx: &DaemonContext,
) -> Result<Option<(AetherAgentId, u64)>, ArchonError> {
    let (agent, generation) = {
        let store = ctx.store.lock().await;
        let agent = store.kv_get(keys::ORCHESTRATOR_AGENT)?;
        let generation = store.kv_get(keys::ORCHESTRATOR_GENERATION)?;
        (agent, generation)
    };
    let Some(agent) = agent.filter(|a| !a.is_empty()) else {
        return Ok(None);
    };
    let agent = AetherAgentId::parse(&agent)
        .map_err(|e| ArchonError::Store(format!("stored orchestrator id {agent:?}: {e}")))?;
    let generation: u64 = generation
        .as_deref()
        .unwrap_or("0")
        .parse()
        .map_err(|e| ArchonError::Store(format!("stored orchestrator generation: {e}")))?;
    Ok(Some((agent, generation)))
}

/// The recorded orchestrator with Aether's view of it, asked now; `None`
/// before it was ever created.
async fn orchestrator_info(ctx: &DaemonContext) -> Result<Option<OrchestratorInfo>, ArchonError> {
    let Some((agent, generation)) = recorded_orchestrator(ctx).await? else {
        return Ok(None);
    };
    let found = match ctx.aether.client() {
        Ok(client) => client.get_agent(&agent).await.map_err(|e| e.to_string()),
        Err(_) => Err("aether disconnected".to_owned()),
    };
    Ok(Some(orchestrator_view(ctx, agent, generation, found)))
}

/// `agent` as `found` in Aether, or with the configured model and why its
/// status is unknown. `restarts` counts generations after the first.
fn orchestrator_view(
    ctx: &DaemonContext,
    agent: AetherAgentId,
    generation: u64,
    found: Result<Agent, String>,
) -> OrchestratorInfo {
    let (model, status) = match found {
        Ok(found) => {
            assert_eq!(found.id, agent, "Aether's record is the orchestrator's");
            (found.model, status_kind(&found.status))
        }
        Err(why) => (
            ctx.config.orchestrator.model.clone(),
            format!("unknown ({why})"),
        ),
    };
    OrchestratorInfo {
        agent,
        model,
        effort: ctx.config.orchestrator.effort,
        status,
        restarts: u32::try_from(generation.saturating_sub(1)).unwrap_or(u32::MAX),
    }
}

/// Aether's status kind (`{"state":"ready"}` → `ready`).
fn status_kind(status: &serde_json::Value) -> String {
    let kind = status
        .get("state")
        .and_then(|s| s.as_str())
        .or_else(|| status.as_str())
        .unwrap_or("unknown");
    kind.to_owned()
}

pub(super) async fn orchestrator_status(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    let info = orchestrator_info(ctx)
        .await?
        .ok_or_else(|| ArchonError::NotFound("the orchestrator has not been created yet".into()))?;
    Ok(Reply::Orchestrator(info))
}

/// Cancel the current orchestrator and start a fresh generation; the reply
/// describes the agent now in charge. This must go through the
/// supervisor, which also ends a streak of failed revivals.
pub(super) async fn orchestrator_restart(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    let agent = supervisor::restart_orchestrator(ctx).await?;
    let info = orchestrator_info(ctx).await?;
    assert!(
        info.as_ref().is_some_and(|i| i.agent == agent),
        "restart_orchestrator records its agent"
    );
    Ok(Reply::Orchestrator(info.expect("checked above")))
}

/// `SetCaps`: persisted in the global settings overlay, so it outlives a
/// restart; `archon settings reset` returns to `[limits]`.
pub(super) async fn set_caps(ctx: &DaemonContext, caps: Caps) -> Result<Reply, ArchonError> {
    let budgets = [
        ("budget_usd_total", caps.budget_usd_total),
        ("budget_usd_daily", caps.budget_usd_daily),
        ("budget_usd_per_item", caps.budget_usd_per_item),
    ];
    for (name, usd) in budgets {
        if !usd.is_finite() || usd < 0.0 {
            return Err(ArchonError::Invalid(format!(
                "{name} must be a finite amount of at least 0 (0 = none), not {usd}"
            )));
        }
    }
    if let Some(name) = caps.max_per_pipeline.keys().find(|n| n.trim().is_empty()) {
        return Err(ArchonError::Invalid(format!(
            "max_per_pipeline has an empty name {name:?}"
        )));
    }
    let mut global = crate::settings::read(ctx, None).await?;
    global.set_caps(&caps);
    crate::settings::write(ctx, None, global).await?;
    Ok(Reply::Ok)
}

/// The catalog, or only `pipeline` in it when one is named.
pub(super) async fn list_pipelines(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
    pipeline: Option<&str>,
) -> Result<Reply, ArchonError> {
    let env = repo_env(ctx, repo).await?;
    let mut listing = env.catalog()?.listing();
    let Some(name) = pipeline else {
        return Ok(Reply::Catalog(listing));
    };
    let known: Vec<String> = listing.pipelines.iter().map(|p| p.name.clone()).collect();
    listing.pipelines.retain(|p| p.name == name);
    listing.problems.retain(|p| p.name == name);
    if listing.pipelines.is_empty() && listing.problems.is_empty() {
        return Err(ArchonError::NotFound(format!(
            "no pipeline {name:?}; known: {}",
            known.join(", ")
        )));
    }
    assert!(listing.pipelines.len() <= 1, "pipeline names are unique");
    Ok(Reply::Catalog(listing))
}

pub(super) async fn model_policy(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
) -> Result<Reply, ArchonError> {
    let env = repo_env(ctx, repo).await?;
    Ok(Reply::ModelPolicy(env.policy(&ctx.config)))
}

async fn repo_env(ctx: &DaemonContext, repo: Option<&RepoRef>) -> Result<RepoEnv, ArchonError> {
    let repo = match repo {
        Some(reference) => Some(ctx.store.lock().await.repo(reference)?),
        None => None,
    };
    RepoEnv::new(repo)
}

/// Pause or resume a scope. `pause --hard` also pauses running stage
/// agents in Aether; resuming the global scope releases them, which only
/// the operator may do (a hard pause is operator-only).
pub(super) async fn pause(
    ctx: &DaemonContext,
    client: ClientKind,
    target: PauseTarget,
    paused: bool,
) -> Result<Reply, ArchonError> {
    let hard_pause = matches!(target, PauseTarget::Global { hard: true }) && paused;
    // Resuming skips the check: a pipeline removed since it was paused
    // must still be resumable.
    if let (PauseTarget::Pipeline { name }, true) = (&target, paused) {
        check_pipeline_known(ctx, name).await?;
    }
    let release = {
        let mut store = ctx.store.lock().await;
        let was_hard = store.kv_get(HARD_PAUSE_KEY)?.is_some_and(|v| v == "1");
        let global = matches!(target, PauseTarget::Global { .. });
        let release = global && !paused && was_hard;
        if release && client == ClientKind::ArchonMcp {
            return Err(ArchonError::Forbidden(
                "the operator's hard pause is lifted only from the archon CLI".into(),
            ));
        }
        let mut state = store.pause_state()?;
        apply_pause(&store, &mut state, &target, paused)?;
        store.set_pause_state(&state)?;
        if hard_pause && paused {
            store.kv_set(HARD_PAUSE_KEY, "1")?;
        } else if hard_pause || release {
            store.kv_delete(HARD_PAUSE_KEY)?;
        }
        release
    };
    if !paused {
        ctx.dispatch.notify_one();
    }
    if hard_pause || release {
        pause_stage_agents(ctx, paused).await?;
    }
    Ok(Reply::Ok)
}

/// Built-in, user and every registered repository's pipelines count,
/// including disabled and broken ones: pausing those is harmless.
async fn check_pipeline_known(ctx: &DaemonContext, name: &str) -> Result<(), ArchonError> {
    let repos = ctx.store.lock().await.repos()?;
    let envs = std::iter::once(None)
        .chain(repos.into_iter().map(Some))
        .map(|repo| RepoEnv {
            repo,
            config: RepoConfig::default(),
        });
    let mut known = BTreeSet::new();
    for env in envs {
        let catalog = env.catalog()?;
        known.extend(catalog.names().map(str::to_owned));
        known.extend(catalog.problems().iter().map(|p| p.name.clone()));
    }
    assert!(!known.is_empty(), "the built-in pipelines are always known");
    if known.contains(name) {
        return Ok(());
    }
    let known: Vec<String> = known.into_iter().collect();
    Err(ArchonError::Invalid(format!(
        "no pipeline is named {name:?}; known pipelines: {}",
        known.join(", ")
    )))
}

fn apply_pause(
    store: &archon_queue::Store,
    state: &mut PauseState,
    target: &PauseTarget,
    paused: bool,
) -> Result<(), ArchonError> {
    match target {
        PauseTarget::Global { .. } => state.global = paused,
        PauseTarget::Repo { name } => {
            let id = store.repo(name)?.id;
            toggle(&mut state.repos, id, paused);
        }
        PauseTarget::Pipeline { name } => {
            if name.trim().is_empty() {
                return Err(ArchonError::Invalid("name the pipeline to pause".into()));
            }
            toggle(&mut state.pipelines, name.clone(), paused);
        }
    }
    Ok(())
}

fn toggle<T: PartialEq>(list: &mut Vec<T>, value: T, present: bool) {
    let before = list.len();
    list.retain(|v| *v != value);
    assert!(
        before - list.len() <= 1,
        "pause lists never repeat an entry"
    );
    if present {
        list.push(value);
    }
}

/// Pause or unpause every running stage agent; every agent is tried, and
/// the failures are reported together. Dispatch is already updated.
async fn pause_stage_agents(ctx: &DaemonContext, paused: bool) -> Result<(), ArchonError> {
    let agents: Vec<AetherAgentId> = {
        let store = ctx.store.lock().await;
        store
            .live_stage_runs()?
            .into_iter()
            .filter(|s| s.state == StageState::Running)
            .filter_map(|s| s.aether_agent)
            .collect()
    };
    if agents.is_empty() {
        return Ok(());
    }
    let client = match ctx.aether.client() {
        Ok(client) => client,
        Err(error) => {
            let (all, why) = (agents.len(), error.to_string());
            return Err(stage_agents_untouched(paused, all, all, &why));
        }
    };
    let mut failures = Vec::new();
    for agent in &agents {
        if let Err(error) = client.pause(agent, paused).await {
            failures.push(format!("{agent}: {error}"));
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    let why = failures.join("; ");
    Err(stage_agents_untouched(
        paused,
        failures.len(),
        agents.len(),
        &why,
    ))
}

fn stage_agents_untouched(paused: bool, failed: usize, total: usize, why: &str) -> ArchonError {
    assert!(failed > 0 && failed <= total, "{failed} of {total} failed");
    assert!(!why.is_empty(), "a failure says why");
    let verb = if paused { "paused" } else { "resumed" };
    ArchonError::Unavailable(format!(
        "dispatch is {verb}, but {failed} of {total} running stage agent(s) could not be {verb} \
         in Aether: {why}"
    ))
}
