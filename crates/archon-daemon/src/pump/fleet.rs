//! Fleet events: stage agents settling, spend, questions from stage agents
//! and their children, and the policy watchdog for new children.

use std::time::Duration;

use archon_aether::AetherEvent;
use archon_aether::events::{EventKind, UsageTotals};
use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef, AetherRunId};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::run::{StageOutcome, StageRun};
use archon_queue::store::SpendRow;
use serde_json::Value;

use super::watchdog;
use crate::context::DaemonContext;
use crate::{executor, feed, inbox_health};

/// How long a settle for an unknown agent that could be a spawning stage's
/// waits for its spawn reply to be recorded (the reply and the first
/// events can cross). The post-spawn `check_outcome` settles it otherwise.
const SPAWN_GRACE: Duration = Duration::from_secs(5);
/// The wait ends when a spawn is recorded. A stage that stops spawning any
/// other way (a sweep, a store written elsewhere) is seen this often.
const SPAWN_RECHECK: Duration = Duration::from_secs(1);
const _: () = assert!(SPAWN_RECHECK.as_millis() <= SPAWN_GRACE.as_millis());

/// Who an Archon-managed agent works for.
#[derive(Debug)]
pub(super) enum Owner {
    Stage(Box<StageRun>),
    /// A child a stage agent or the orchestrator delegated to, at any depth.
    Child {
        item: Option<QueueItemId>,
    },
    /// A stage agent Aether created under its run's node whose spawn reply
    /// is not recorded yet.
    StageAgent {
        item: QueueItemId,
    },
}

pub(super) async fn handle(ctx: &DaemonContext, event: &AetherEvent) -> Result<(), ArchonError> {
    let Some(agent) = &event.agent else {
        return Ok(());
    };
    ctx.activity.saw(agent, archon_core::now());
    match &event.kind {
        EventKind::InboxDelivered { seq, message, .. } => {
            if let Some(message) = message {
                crate::agent_messages::inbox(ctx, agent, *seq, message).await?;
            }
            inbox_delivered(ctx, event, agent, *seq).await
        }
        EventKind::InboxConsumed { up_to } => inbox_health::consumed(ctx, agent, *up_to).await,
        EventKind::AgentCreated { model, .. } => {
            watchdog::agent_created(ctx, event, agent, model).await
        }
        EventKind::TaskCompleted { .. }
        | EventKind::TaskFailed { .. }
        | EventKind::TaskCancelled { .. } => task_settled(ctx, event, agent).await,
        EventKind::AgentFailed { .. } | EventKind::AgentCancelled { .. } => {
            inbox_health::ended(ctx, agent).await?;
            agent_died(ctx, event, agent).await?;
            turn_ended(ctx);
            Ok(())
        }
        EventKind::AgentCompleted => {
            inbox_health::ended(ctx, agent).await?;
            turn_ended(ctx);
            Ok(())
        }
        EventKind::AgentTurnCompleted { .. } => executor::turn_ended(ctx, agent).await,
        EventKind::AgentUsageUpdated { usage } => record_usage(ctx, event, agent, usage).await,
        EventKind::QuestionAsked { question } => question_asked(ctx, event, agent, question).await,
        _ => Ok(()),
    }
}

/// Inbox items of agents that work for an item are watched until taken.
async fn inbox_delivered(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    seq: u64,
) -> Result<(), ArchonError> {
    let Some(owner) = owner(ctx, agent).await? else {
        return Ok(());
    };
    let Some(item) = item_of(ctx, &owner).await? else {
        return Ok(());
    };
    inbox_health::delivered(ctx, agent, item, seq, event.at).await
}

/// An agent ended for good: the stop of what it delegated, which waited for
/// its turn to end, and the spawns behind it need not wait for the next tick.
fn turn_ended(ctx: &DaemonContext) {
    ctx.dispatch.notify_one();
}

/// The owner of an agent Archon manages; `None` for anyone else's agent
/// (the orchestrator has its own stream).
pub(super) async fn owner(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<Owner>, ArchonError> {
    let store = ctx.store.lock().await;
    if let Some(stage) = store.stage_run_by_agent(agent)? {
        return Ok(Some(Owner::Stage(Box::new(stage))));
    }
    let key = watchdog::child_key(agent);
    if let Some(raw) = store.kv_get(&key)? {
        let item = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
        return Ok(Some(Owner::Child { item }));
    }
    let key = watchdog::stage_agent_key(agent);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let item = QueueItemId::parse(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
    Ok(Some(Owner::StageAgent { item }))
}

pub(super) async fn item_of(
    ctx: &DaemonContext,
    owner: &Owner,
) -> Result<Option<QueueItemId>, ArchonError> {
    match owner {
        Owner::Stage(stage) => Ok(Some(ctx.store.lock().await.run(&stage.run)?.item)),
        Owner::Child { item } => Ok(item.clone()),
        Owner::StageAgent { item } => Ok(Some(item.clone())),
    }
}

async fn task_settled(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    let Some(outcome) = event.kind.stage_outcome() else {
        tracing::warn!(kind = ?event.kind, %agent, "a task event without a stage outcome is ignored");
        return Ok(());
    };
    let Some(stage) = stage_awaiting_spawn(ctx, event, agent).await? else {
        return child_settled(ctx, event, agent).await;
    };
    let same_task = match (&stage.aether_task, &event.task) {
        (Some(ours), Some(theirs)) => ours == theirs,
        // Resumed on a kept agent, whose task settled with an earlier
        // attempt: a replayed event of that task is not this one's (§6.7).
        (None, _) => false,
        (Some(_), None) => false,
    };
    if !same_task || stage.state.is_terminal() {
        return Ok(());
    }
    executor::stage_settled(ctx, agent, outcome).await
}

/// A completed task of an agent a stage agent delegated to may be a reviewer's
/// verdict, which can make the stage's review-fix loop long enough to tell the operator.
async fn child_settled(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    match (&event.kind, &event.parent) {
        (EventKind::TaskCompleted { .. }, Some(lead)) => {
            executor::reviewer_settled(ctx, lead, agent).await
        }
        _ => Ok(()),
    }
}

/// A stage agent that died without its task settling (engine crash, operator
/// cancel in Aether) still ends its stage.
async fn agent_died(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
) -> Result<(), ArchonError> {
    let Some(stage) = stage_awaiting_spawn(ctx, event, agent).await? else {
        return Ok(());
    };
    if stage.state.is_terminal() {
        return Ok(());
    }
    if ended_an_earlier_run(ctx, &stage, event.at).await? {
        tracing::info!(stage = %stage.id, %agent, at = %event.at, "an earlier run's end replayed; ignored");
        return Ok(());
    }
    let outcome = match &event.kind {
        EventKind::AgentFailed { .. } if failed_past_deadline(ctx, &stage, event.at).await? => {
            tracing::info!(stage = %stage.id, at = %event.at, "agent failed at its deadline; timed out");
            StageOutcome::TimedOut
        }
        EventKind::AgentFailed { error } => StageOutcome::Failed {
            error: error.clone(),
            infra: true,
        },
        EventKind::AgentCancelled { reason } => StageOutcome::Cancelled {
            reason: reason.clone(),
        },
        other => {
            tracing::warn!(kind = ?other, %agent, "an event that does not end an agent is ignored");
            return Ok(());
        }
    };
    executor::stage_settled(ctx, agent, outcome).await
}

/// Whether an agent's end at `at` came before resumed or woken `stage` took it
/// over: it ended the run the agent was taken from, already settled. A
/// later end the sweep reads from Aether, should one come before `at`.
async fn ended_an_earlier_run(
    ctx: &DaemonContext,
    stage: &StageRun,
    at: Timestamp,
) -> Result<bool, ArchonError> {
    assert!(!stage.state.is_terminal(), "only a live stage run settles");
    let store = ctx.store.lock().await;
    let taken_over = match executor::load_spawn(&store, &stage.id) {
        Ok(spawn) => spawn.resume.is_some(),
        Err(ArchonError::NotFound(_)) => false,
        Err(e) => return Err(e),
    };
    Ok(taken_over && at < stage.started_at)
}

/// Whether the agent failed at or past its stage's deadline: Aether stops it
/// then, and an infra re-spawn would redo the work from nothing. `at` is when
/// it failed, not when Archon heard, so a replayed earlier crash stays infra.
async fn failed_past_deadline(
    ctx: &DaemonContext,
    stage: &StageRun,
    at: Timestamp,
) -> Result<bool, ArchonError> {
    let store = ctx.store.lock().await;
    let deadline = match executor::load_spawn(&store, &stage.id) {
        Ok(spawn) => spawn.request.deadline,
        // As in the sweep: a stage run without its request has no deadline.
        Err(ArchonError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    Ok(deadline.is_some_and(|deadline| at >= deadline))
}

/// The outcome a dead stage agent's `status` implies; `None` while alive.
pub(super) fn dead_agent_outcome(status: &Value) -> Option<StageOutcome> {
    let state = status.get("state").and_then(Value::as_str)?;
    let error = status.get("error").and_then(Value::as_str);
    match state {
        "failed" => Some(StageOutcome::Failed {
            error: error.unwrap_or("the stage agent failed").to_owned(),
            infra: true,
        }),
        "cancelled" => Some(StageOutcome::Cancelled {
            reason: status
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "completed" | "imported" => Some(StageOutcome::Failed {
            error: format!("the stage agent is {state} but its task never settled"),
            infra: true,
        }),
        _ => None,
    }
}

/// The stage run for `agent`. An agent Archon does not know may be a
/// spawning stage's whose reply is not recorded yet, so an event that
/// could be one waits briefly for it. Run nodes, watched children and
/// agents in other Aether runs never hold up the stream.
async fn stage_awaiting_spawn(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
) -> Result<Option<StageRun>, ArchonError> {
    let deadline = tokio::time::Instant::now() + SPAWN_GRACE;
    loop {
        // Listening before the store is read: a spawn recorded between the
        // read and the wait still ends the wait.
        let recorded = ctx.spawn_recorded.notified();
        tokio::pin!(recorded);
        recorded.as_mut().enable();
        match look_up(ctx, event, agent).await? {
            Lookup::Stage(stage) => return Ok(Some(*stage)),
            Lookup::NotStage => return Ok(None),
            Lookup::MaybeSpawning => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::debug!(%agent, seq = event.seq, "no spawning stage claimed the agent");
            return Ok(None);
        }
        tokio::select! {
            () = &mut recorded => {}
            () = tokio::time::sleep_until(deadline.min(now + SPAWN_RECHECK)) => {}
        }
    }
}

/// What the store says about the agent of a settling event.
enum Lookup {
    Stage(Box<StageRun>),
    NotStage,
    /// Unknown, and in an Aether run a spawning stage's agent may join.
    MaybeSpawning,
}

async fn look_up(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
) -> Result<Lookup, ArchonError> {
    assert_eq!(event.agent.as_ref(), Some(agent), "the event's own agent");
    let store = ctx.store.lock().await;
    if let Some(stage) = store.stage_run_by_agent(agent)? {
        return Ok(Lookup::Stage(Box::new(stage)));
    }
    let known = executor::is_node_agent(&store, agent)?
        || store.kv_get(&watchdog::child_key(agent))?.is_some();
    if known {
        return Ok(Lookup::NotStage);
    }
    let awaited = executor::runs_awaiting_spawn(&store)?;
    if could_be_spawning(&awaited, event.run.as_ref()) {
        return Ok(Lookup::MaybeSpawning);
    }
    Ok(Lookup::NotStage)
}

/// Whether an event in `run` could be from the agent of a stage awaiting
/// its spawn reply, given the Aether runs those agents join. An unknown
/// run on either side could be any.
fn could_be_spawning(awaited: &[Option<AetherRunId>], run: Option<&AetherRunId>) -> bool {
    awaited.iter().any(|joins| match (joins, run) {
        (Some(joins), Some(run)) => joins == run,
        _ => true,
    })
}

async fn record_usage(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    usage: &UsageTotals,
) -> Result<(), ArchonError> {
    let Some(owner) = owner(ctx, agent).await? else {
        return retired_orchestrator_usage(ctx, event, agent, usage).await;
    };
    let item = item_of(ctx, &owner).await?;
    record_spend(ctx, event, agent, item.clone(), usage).await?;
    if let Some(item) = item {
        let has_usage = usage.input_tokens > 0 || usage.output_tokens > 0;
        let has_cost = usage.cost_complete == Some(true)
            || (usage.cost_complete.is_none() && usage.cost_usd > 0.0);
        let has_estimate =
            ctx.config.limits.count_estimated_cost && usage.estimated_cost_usd.is_some();
        let stage = match &owner {
            Owner::Stage(stage) => Some(&stage.id),
            _ => None,
        };
        executor::check_item_budget(ctx, &item, has_usage && !has_cost && !has_estimate, stage)
            .await?;
    }
    recheck_global_budgets(ctx).await;
    Ok(())
}

/// Spend changes what the dispatcher plans only by reaching the total or
/// the daily budget (`plan_dispatch`), so only then does new spend wake it.
async fn recheck_global_budgets(ctx: &DaemonContext) {
    let caps = crate::resolve::caps(ctx).await;
    if caps.budgets_enabled && (caps.budget_usd_total > 0.0 || caps.budget_usd_daily > 0.0) {
        ctx.dispatch.notify_one();
    }
}

/// The orchestrator's own stream follows only the agent in charge, so the
/// last spend of one it replaced is counted from the fleet's.
async fn retired_orchestrator_usage(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    usage: &UsageTotals,
) -> Result<(), ArchonError> {
    if !crate::supervisor::is_retired_orchestrator(ctx, agent).await? {
        return Ok(());
    }
    record_spend(ctx, event, agent, None, usage).await?;
    recheck_global_budgets(ctx).await;
    Ok(())
}

/// `usage` is a running total, so a replay or a late event is harmless.
pub(super) async fn record_spend(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    item: Option<QueueItemId>,
    usage: &UsageTotals,
) -> Result<(), ArchonError> {
    let estimate = usage.estimated_cost_usd.unwrap_or(0.0);
    if !(usage.cost_usd >= 0.0 && estimate >= 0.0) {
        return Err(ArchonError::Protocol(format!(
            "agent {agent} reported negative or NaN cost {} / {estimate}",
            usage.cost_usd
        )));
    }
    let row = SpendRow {
        agent: agent.clone(),
        item,
        cost_usd: usage.cost_usd,
        cost_basis: usage.cost_basis,
        cost_complete: usage.cost_complete,
        estimated_cost_usd: estimate,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        at: event.at,
    };
    ctx.store.lock().await.record_spend(&row)
}

async fn question_asked(
    ctx: &DaemonContext,
    event: &AetherEvent,
    agent: &AetherAgentId,
    question: &str,
) -> Result<(), ArchonError> {
    let Some(owner) = owner(ctx, agent).await? else {
        return Ok(());
    };
    let item = item_of(ctx, &owner).await?;
    let aether = AetherQuestionRef {
        agent: agent.clone(),
        seq: event.seq,
    };
    feed::aether_question(ctx, aether, question, item)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> AetherRunId {
        AetherRunId::parse(id).unwrap()
    }

    #[test]
    fn only_an_event_in_a_spawning_stages_run_could_be_its_agents() {
        let (ours, theirs) = (run("run_0001"), run("run_0002"));
        assert!(!could_be_spawning(&[], Some(&ours)), "nothing is spawning");
        assert!(!could_be_spawning(&[], None));
        let awaited = [Some(ours.clone())];
        assert!(could_be_spawning(&awaited, Some(&ours)));
        assert!(!could_be_spawning(&awaited, Some(&theirs)));
        assert!(
            could_be_spawning(&awaited, None),
            "a runless event could be"
        );
        let unknown = [Some(theirs.clone()), None];
        assert!(
            could_be_spawning(&unknown, Some(&ours)),
            "a root's run is unknown"
        );
    }
}
