//! The Aether event pump (ARCHITECTURE §4.6, §16). Connects
//! with backoff, installs the client in `AetherLink`, reconciles after a
//! (re)connect, runs the `orchestrator` and `fleet` subscriptions, and
//! persists each cursor after its event is handled (at-least-once).

mod aetherd;
mod fleet;
mod orchestrator;
mod stream;
mod watchdog;

use std::sync::Arc;
use std::time::Duration;

use archon_aether::backend::task_outcome;
use archon_aether::client::Endpoint;
use archon_aether::{AetherClient, AetherEvent};
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::run::{StageOutcome, StageRun};
use archon_queue::store::keys;
use tokio::time::Instant;

use crate::context::DaemonContext;
use crate::feed::{self, REPLY_STATE_KEY};
use crate::{executor, github, reports, supervisor};

pub use aetherd::AetherStarter;
use aetherd::OwnAetherd;
pub(crate) use watchdog::{prune_children, prune_stage_agents};

pub const BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);
pub const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

pub const ORCHESTRATOR_EVENT_TYPES: &[&str] = &[
    "agent.message",
    "agent.turn_completed",
    // Whether the turn's wait ends by itself, so that the turn after it
    // continues the same answer (see `ReplyTracker`).
    "agent.waiting",
    "agent.started",
    "question.asked",
    "agent.failed",
    "agent.cancelled",
    "agent.completed",
    "agent.usage_updated",
    // Not in ARCHITECTURE §4.6: which turn reads an operator message, in
    // stream order (see `ReplyTracker`).
    "inbox.delivered",
    "inbox.consumed",
    // Aether restarted the orchestrator into a new agent (`aether restart`).
    "agent.restarted",
    // Its older context became a summary: the directives are stated again.
    "compaction.completed",
];

pub const FLEET_EVENT_TYPES: &[&str] = &[
    "inbox.delivered",
    "agent.created",
    "task.completed",
    "task.failed",
    "task.cancelled",
    "agent.failed",
    "agent.cancelled",
    // A settled stage agent ended its turn: what it delegated is stopped (§6.3).
    "agent.completed",
    // A kept agent resumed for a fix answers by ending its turn (§6.7).
    "agent.turn_completed",
    "agent.usage_updated",
    "question.asked",
];

/// What the fleet's records say an agent other than the orchestrator is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FleetRole {
    /// A stage agent of this item.
    Stage(QueueItemId),
    /// Delegated to by a stage agent of this item, at any depth.
    StageChild(QueueItemId),
    /// Delegated to by an orchestrator, at any depth.
    OrchestratorChild,
}

/// The recorded role of `agent`; `None` for an agent Archon does not manage.
pub(crate) async fn fleet_role(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<FleetRole>, ArchonError> {
    let Some(owner) = fleet::owner(ctx, agent).await? else {
        return Ok(None);
    };
    Ok(Some(match owner {
        fleet::Owner::Stage(stage) => {
            FleetRole::Stage(ctx.store.lock().await.run(&stage.run)?.item)
        }
        fleet::Owner::StageAgent { item } => FleetRole::Stage(item),
        fleet::Owner::Child { item: Some(item) } => FleetRole::StageChild(item),
        fleet::Owner::Child { item: None } => FleetRole::OrchestratorChild,
    }))
}

/// An outage shorter than this stays out of the operator's chat.
const QUIET_OUTAGE: Duration = Duration::from_secs(30);

/// Runs until shutdown; never returns `Ok` early. Connection failures are
/// retried with `BACKOFF_MIN..BACKOFF_MAX`; only a store failure is fatal.
/// With a `starter` (dedicated mode) an aetherd that is gone is started again.
/// Also supervises the Lavish report polls, the orchestrator's deferred
/// revivals and owed sweeps, the `archon` MCP server's health and the
/// GitHub watcher's target, which need an `Arc` context.
pub async fn run(
    ctx: Arc<DaemonContext>,
    endpoint: Endpoint,
    starter: Option<Arc<dyn AetherStarter>>,
) -> Result<(), ArchonError> {
    let saved = ctx.store.lock().await.kv_get(REPLY_STATE_KEY)?;
    *ctx.replies.lock().await = feed::restore_replies(saved.as_deref())?;
    tokio::try_join!(
        link_loop(ctx.clone(), endpoint, OwnAetherd::new(starter)),
        reports::supervise(ctx.clone()),
        supervisor::revive_when_due(ctx.clone()),
        supervisor::sweep_when_owed(ctx.clone()),
        supervisor::watch_mcp(ctx.clone()),
        github::watch_github(ctx.clone()),
        crate::handlers::deliver_memory_feedback(ctx.clone())
    )?;
    Ok(())
}

/// Connect, serve one session until the connection drops, and repeat.
async fn link_loop(
    ctx: Arc<DaemonContext>,
    endpoint: Endpoint,
    mut own: OwnAetherd,
) -> Result<(), ArchonError> {
    let mut backoff = BACKOFF_MIN;
    let mut outage = Outage::started();
    let mut warned = Warned::default();
    loop {
        match AetherClient::connect(&endpoint).await {
            Ok(client) => {
                let client = Arc::new(client);
                warned.warn_once(&ctx, &client);
                announce_connected(&ctx, &client, &outage).await?;
                own.answered();
                backoff = BACKOFF_MIN;
                ctx.aether.replace(Some(client.clone()))?;
                ctx.memory_feedback_ready.notify_one();
                supervisor::check_mcp_soon(&ctx);
                supervisor::sweep_soon(&ctx);
                github::check_github_soon(&ctx);
                let served = session(&ctx, &client).await;
                ctx.aether.replace(None)?;
                served?;
                let reason = client.disconnect_reason().unwrap_or_default();
                tracing::warn!(%reason, "lost the aether connection");
                outage = Outage::started();
            }
            Err(error) => {
                let absent = nothing_listens(&endpoint, &error);
                outage.log(&error, absent, backoff);
                outage.note(&ctx, &error).await?;
                // No daemon is burdened by being asked again soon, and one
                // that starts is found within a second.
                if absent {
                    own.absent(&ctx).await?;
                    backoff = BACKOFF_MIN;
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                own.listening();
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// What the link loop warned about once already, whatever the reconnects.
#[derive(Debug, Default)]
struct Warned {
    host_context: bool,
    notes: bool,
}

impl Warned {
    fn warn_once(&mut self, ctx: &DaemonContext, client: &AetherClient) {
        let welcome = client.welcome();
        let host_context = ctx.config.aether.host_context;
        if host_context == archon_core::config::HostContext::Lean
            && welcome.effective_host_context(host_context).is_full()
            && !self.host_context
        {
            tracing::warn!(
                "Aether lacks host_context; new and pending lean creations fall back to full; existing agents keep their context"
            );
            self.host_context = true;
        }
        if !self.notes {
            crate::command_notice::warn_if_unsupported(welcome);
            self.notes = true;
        }
    }
}

/// Whether `error` says no daemon holds the local socket at `endpoint`: it
/// is missing, or left behind by an aetherd that has exited.
fn nothing_listens(endpoint: &Endpoint, error: &ArchonError) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    let ArchonError::Io { cause, .. } = error else {
        return false;
    };
    matches!(endpoint, Endpoint::Unix(_)) && matches!(cause.kind(), NotFound | ConnectionRefused)
}

/// Tracks one stretch without Aether so the operator hears about it once,
/// and only when it lasts.
#[derive(Debug)]
struct Outage {
    since: Instant,
    announced: bool,
    /// The log already says that nothing listens.
    absence_logged: bool,
}

impl Outage {
    fn started() -> Self {
        Self {
            since: Instant::now(),
            announced: false,
            absence_logged: false,
        }
    }

    /// Logs a failed connection. `absent` attempts repeat every second, so
    /// only the first of a stretch is a warning.
    fn log(&mut self, error: &ArchonError, absent: bool, backoff: Duration) {
        if absent && self.absence_logged {
            tracing::debug!(%error, "still nothing listens for aetherd");
            return;
        }
        self.absence_logged = absent;
        let retry = if absent { BACKOFF_MIN } else { backoff };
        tracing::warn!(%error, ?retry, "cannot reach aetherd");
    }

    async fn note(&mut self, ctx: &DaemonContext, error: &ArchonError) -> Result<(), ArchonError> {
        if self.announced || self.since.elapsed() < QUIET_OUTAGE {
            return Ok(());
        }
        self.announced = true;
        let text = format!(
            "Aether is unreachable ({error}); Archon keeps retrying. Nothing new is dispatched meanwhile."
        );
        feed::system(ctx, text).await.map(drop)
    }
}

/// Record the instance id; tell the operator about a restart of Aether or
/// the end of an outage they were told about.
async fn announce_connected(
    ctx: &DaemonContext,
    client: &AetherClient,
    outage: &Outage,
) -> Result<(), ArchonError> {
    let instance = &client.welcome().instance_id;
    assert!(!instance.is_empty(), "aether always names its instance");
    let previous = {
        let mut store = ctx.store.lock().await;
        let previous = store.kv_get(keys::AETHER_INSTANCE)?;
        store.kv_set(keys::AETHER_INSTANCE, instance)?;
        previous
    };
    let restarted = previous.as_ref().is_some_and(|p| p != instance);
    if restarted {
        feed::system(
            ctx,
            "Aether restarted; Archon reconnected and is catching up.",
        )
        .await?;
    } else if outage.announced {
        feed::system(ctx, "Reconnected to Aether.").await?;
    }
    Ok(())
}

/// One connection's worth of work: reconcile, then follow both streams
/// until the connection is lost. `Err` only for a fatal (store) failure.
async fn session(ctx: &DaemonContext, client: &AetherClient) -> Result<(), ArchonError> {
    match reconcile(ctx).await {
        Ok(()) => {}
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            tracing::warn!(%error, "reconcile after connect failed");
            if !client.is_connected() {
                return Ok(());
            }
        }
    }
    ctx.dispatch.notify_one();
    tokio::try_join!(
        stream::follow_fleet(ctx, client),
        stream::follow_orchestrator(ctx, client)
    )?;
    Ok(())
}

/// After every (re)connect: sync every live stage agent and the orchestrator
/// from `get_task`/`get_agent`, feeding settled outcomes to the executor.
pub async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let client = ctx.aether.client()?;
    let stages = ctx.store.lock().await.live_stage_runs()?;
    for stage in stages {
        match reconcile_stage(ctx, &client, &stage).await {
            Ok(()) => {}
            Err(e) if e.is_store() || matches!(e, ArchonError::Unavailable(_)) => return Err(e),
            Err(error) => {
                tracing::warn!(stage = %stage.id, %error, "could not reconcile a stage");
                let text = format!("Could not check stage {} with Aether: {error}", stage.id);
                feed::system(ctx, text).await?;
            }
        }
    }
    match supervisor::ensure_orchestrator(ctx).await {
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "orchestrator unavailable; the queue runs without it");
            Ok(())
        }
        Ok(_) => Ok(()),
    }
}

/// Settle a stage whose task finished (or whose agent died) while Archon
/// was not listening. A stage still spawning is the executor's recovery to retry.
async fn reconcile_stage(
    ctx: &DaemonContext,
    client: &AetherClient,
    stage: &StageRun,
) -> Result<(), ArchonError> {
    assert!(
        !stage.state.is_terminal(),
        "only live stages are reconciled"
    );
    let (Some(agent), Some(task)) = (&stage.aether_agent, &stage.aether_task) else {
        return Ok(());
    };
    let task = match client.get_task(task).await {
        Ok(task) => task,
        Err(ArchonError::NotFound(why)) => return settle_forgotten(ctx, agent, &why).await,
        Err(error) => return Err(error),
    };
    if let Some(outcome) = task_outcome(&task) {
        return executor::stage_settled(ctx, agent, outcome).await;
    }
    let status = match client.get_agent(agent).await {
        Ok(found) => found.status,
        Err(ArchonError::NotFound(why)) => return settle_forgotten(ctx, agent, &why).await,
        Err(error) => return Err(error),
    };
    match fleet::dead_agent_outcome(&status) {
        Some(outcome) => executor::stage_settled(ctx, agent, outcome).await,
        None => Ok(()),
    }
}

/// Aether no longer knows the stage's task or agent (its store was reset),
/// so nothing will ever settle it: it fails as infrastructure.
async fn settle_forgotten(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    why: &str,
) -> Result<(), ArchonError> {
    tracing::warn!(%agent, %why, "Aether forgot a live stage; failing it");
    let outcome = StageOutcome::Failed {
        error: format!("Aether no longer knows this stage ({why})"),
        infra: true,
    };
    executor::stage_settled(ctx, agent, outcome).await
}

/// Stage settlement → `executor::stage_settled`; usage → spend ledger;
/// `question.asked` → Question + feed; `agent.created` → policy watchdog.
pub async fn handle_fleet_event(
    ctx: &DaemonContext,
    event: &AetherEvent,
) -> Result<(), ArchonError> {
    fleet::handle(ctx, event).await
}

/// Chat replies via `ReplyTracker`, questions, and orchestrator failure →
/// `supervisor::ensure_orchestrator`.
pub async fn handle_orchestrator_event(
    ctx: &DaemonContext,
    event: &AetherEvent,
) -> Result<(), ArchonError> {
    orchestrator::handle(ctx, event).await
}
