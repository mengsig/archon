//! Daemon control: status, pause and resume, caps, the catalog, the model
//! policy and the orchestrator.

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::protocol::{
    AetherLinkStatus, ClientKind, DaemonStatus, DiskStatus, ItemQuery, OrchestratorInfo,
    PauseTarget, Reply, RepoRef,
};
use archon_core::queue::{Caps, PauseState};
use archon_core::run::StageState;
use archon_queue::store::keys;

use crate::context::DaemonContext;
use crate::resolve::{self, RepoEnv};
use crate::{dispatcher, github, ops, supervisor, triggers};

/// Set while `pause --hard` holds running stage agents paused in Aether.
const HARD_PAUSE_KEY: &str = "global_pause_hard";

pub(super) async fn status(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    let caps = resolve::caps(ctx).await;
    let autonomy = RepoEnv::new(None)?.autonomy(&ctx.config);
    let (snapshot, blocked, open_questions, directives, open_gates) = {
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
        let open_gates = store.open_gates()?.len();
        let questions = store.questions(true)?.len();
        (
            snapshot,
            blocked.len(),
            questions,
            store.directives(now)?,
            open_gates,
        )
    };
    let waiting_items = super::item_views::read_summaries(
        ctx,
        &ItemQuery {
            states: vec!["queued".into(), "blocked".into(), "running".into()],
            limit: Some(usize::MAX),
            ..ItemQuery::default()
        },
    )
    .await?
    .into_iter()
    .filter(|row| row.waiting_reason.is_some())
    .collect();
    let status = DaemonStatus {
        waiting_items,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        started_at: ctx.started_at,
        aether: link_status(ctx),
        orchestrator: orchestrator_info(ctx).await?,
        caps,
        pause: snapshot.pause,
        running_agents: snapshot.running_total,
        active_runs: snapshot.active_runs,
        queued_items: count(snapshot.queued.len()),
        blocked_items: count(blocked),
        open_questions: count(open_questions),
        spend_today_usd: snapshot.spend_today_usd,
        spend_total_usd: snapshot.spend_total_usd,
        lavish_available: ctx.lavish.is_some(),
        autonomy,
        directives,
        open_gates: count(open_gates),
        held_news: count(triggers::held_news(ctx).await?),
        mcp: Some(supervisor::mcp_status(ctx).await),
        github: github::status(ctx).await,
        disk: Some(disk_status(ctx).await?),
        versions: Some(ops::versions(ctx).await),
        upgrade: ops::phase(ctx).await,
    };
    assert!(
        status.spend_today_usd <= status.spend_total_usd + 1e-9,
        "today is part of the total"
    );
    Ok(Reply::Status(Box::new(status)))
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

/// The recorded orchestrator with Aether's view of it; `None` before it
/// was ever created. `restarts` counts generations after the first.
async fn orchestrator_info(ctx: &DaemonContext) -> Result<Option<OrchestratorInfo>, ArchonError> {
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
    let (model, status) = match ctx.aether.client() {
        Ok(client) => match client.get_agent(&agent).await {
            Ok(found) => (found.model, status_kind(&found.status)),
            Err(error) => (
                ctx.config.orchestrator.model.clone(),
                format!("unknown ({error})"),
            ),
        },
        Err(_) => (
            ctx.config.orchestrator.model.clone(),
            "unknown (aether disconnected)".into(),
        ),
    };
    Ok(Some(OrchestratorInfo {
        agent,
        model,
        effort: ctx.config.orchestrator.effort,
        status,
        restarts: u32::try_from(generation.saturating_sub(1)).unwrap_or(u32::MAX),
    }))
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

/// `SetCaps`: a runtime override of `[limits]` until the daemon restarts.
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
    tracing::info!(?caps, "caps overridden");
    *ctx.caps_override.lock().await = Some(caps);
    ctx.dispatch.notify_one();
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
