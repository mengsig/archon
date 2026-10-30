//! One agent through a fix loop (ARCHITECTURE §6.7). The agent of a work
//! stage that a later stage can send back is created long-lived: it outlives
//! its task, and the next attempt of its slot hands it the findings instead
//! of starting a cold agent. An attempt resumed this way has the agent and
//! no task of its own; it settles on what the agent says when its turn ends.
//! An agent that failed for infrastructure is woken in place the same way
//! (§6.3): its replacement attempt resumes it before any fresh agent.

use std::collections::BTreeSet;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::ports::{AgentUpdate, SpawnRequest};
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_pipeline::contract::SHIP_RECORD_JSON;
use archon_pipeline::{ShipRecord, SpawnPlan};
use archon_queue::Store;

use super::spawn::{self, KeptAgent};
use super::{FollowUp, load_spawn, run_node, settle, ship, stall, steer, sweep};
use crate::context::DaemonContext;

/// How long a kept agent may wait for findings before Aether ends it. Its
/// run ends it sooner: it is a child of the run's node.
const PARKED_DAYS: i64 = 7;
const REPLACED: &str = "a fresh agent takes over its stage";
const ENDED: &str = "its stage attempt ended";

/// What becomes of a kept agent once an attempt of its settled.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kept {
    /// The attempt completed: it waits for findings, or for its run to end.
    Park(AetherAgentId),
    /// The attempt ended any other way, so no later one resumes it.
    End(AetherAgentId),
}

/// Whether Aether gave no answer, so the same call can be repeated.
pub(super) fn unanswered(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Unavailable(_) | ArchonError::Protocol(_) | ArchonError::Io { .. }
    )
}

/// How the next stage run of a slot gets its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Takeover {
    /// The slot holds no kept agent: a first attempt, or one that ended.
    Fresh,
    /// The slot's kept agent goes on, told what to fix.
    Resume(KeptAgent),
    /// The slot's kept agent cannot go on: it is stopped for a fresh one.
    Replace(AetherAgentId),
}

/// What the stage run reserved for `plan` does about the agent kept in its
/// slot. `lost` is an infrastructure replacement of the slot's live attempt.
pub(super) fn takeover(
    store: &Store,
    run: &PipelineRunId,
    plan: &SpawnPlan,
    lost: bool,
) -> Result<Takeover, ArchonError> {
    assert!(plan.attempt >= 1, "attempts are 1-based");
    assert!(
        plan.resume.is_none() || (plan.keep && plan.attempt > 1),
        "only the kept agent of an earlier attempt is resumed"
    );
    let before = if lost { plan.attempt } else { plan.attempt - 1 };
    let runs = store.stage_runs(run)?;
    let last = runs
        .iter()
        .filter(|s| s.stage_id == plan.stage_id && s.fan_index == plan.fan_index)
        .filter(|s| s.attempt == before)
        .max_by_key(|s| s.started_at);
    let Some(last) = last else {
        return Ok(Takeover::Fresh);
    };
    // An attempt that ended any other way had its agent stopped as it settled.
    let waits = lost || last.state == StageState::Completed;
    let Some(agent) = last.aether_agent.clone().filter(|_| waits) else {
        return Ok(Takeover::Fresh);
    };
    if !is_kept(store, &last.id)? {
        return Ok(Takeover::Fresh);
    }
    // A steer that planned another model wants it from the start; a live retune
    // already moved the agent to its current choice.
    let same_model = *last.current_choice() == plan.choice;
    let (Some(message), Some(parked_at)) = (&plan.resume, last.finished_at) else {
        return Ok(Takeover::Replace(agent));
    };
    if lost || !same_model {
        return Ok(Takeover::Replace(agent));
    }
    // Notes steered in while it was parked reached the reviewers, not it.
    let item = store.run(run)?.item;
    let mut message = message.clone();
    for note in steer::notes_since(store, &item, parked_at)? {
        message.push_str(&format!("\n\n{}", steer::note_message(&note)));
    }
    Ok(Takeover::Resume(KeptAgent {
        agent,
        message,
        asked_record: None,
    }))
}

/// Record that the agent of resumed attempt `stage` was told `text` after
/// its findings: the attempt settles on what it says once it took that.
pub(super) fn told(store: &mut Store, stage: &StageRunId, text: &str) -> Result<(), ArchonError> {
    assert!(!text.trim().is_empty(), "an agent is told something");
    let mut stored = load_spawn(store, stage)?;
    let Some(kept) = &mut stored.resume else {
        return Ok(());
    };
    kept.message = text.to_owned();
    spawn::save_spawn(store, stage, &stored)
}

/// What the woken agent of an infrastructure replacement is told, with the
/// `error` its last turn failed on.
pub(super) fn woken_note(error: &str) -> String {
    assert!(!error.trim().is_empty(), "a failure says what failed");
    format!(
        "Your previous turn failed for an infrastructure reason: {error}\n\nArchon woke you to \
         continue. Your worktree and history stand: pick up where you left off. That failure \
         ended your task, so do not call `complete_task`; end your turn with the final message \
         your instructions ask for, its fenced `json` block included."
    )
}

/// Whether the slot's agent can take the attempt: a kept one still live, or a
/// failed one Aether wakes. One an earlier, unanswered wake reached is live.
async fn takes_over(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: &AetherAgentId,
) -> Result<bool, ArchonError> {
    assert_eq!(stage.state, StageState::Spawning, "a reserved attempt");
    let asked_before = {
        let mut store = ctx.store.lock().await;
        let mut spawn = load_spawn(&store, &stage.id)?;
        if !spawn.woken {
            drop(store);
            return ctx.backend.is_live(agent).await;
        }
        let asked = std::mem::replace(&mut spawn.wake_asked, true);
        spawn::save_spawn(&mut store, &stage.id, &spawn)?;
        asked
    };
    match ctx.backend.wake(agent).await {
        Ok(()) => Ok(true),
        Err(error) if unanswered(&error) => Err(error),
        Err(refusal) => {
            let live = asked_before && ctx.backend.is_live(agent).await?;
            if !live {
                tracing::info!(stage = %stage.id, %agent, %refusal, "failed agent not woken");
            }
            Ok(live)
        }
    }
}

/// Give the kept agent its deadline, new grant and findings, keyed by the stage run;
/// `false` when it cannot go on and a fresh agent takes the attempt (`request` is never spawned).
pub(super) async fn hand_over(
    ctx: &DaemonContext,
    stage: &StageRun,
    kept: &KeptAgent,
    request: &SpawnRequest,
) -> Result<bool, ArchonError> {
    assert_eq!(stage.state, StageState::Spawning, "a reserved attempt");
    assert!(!kept.message.trim().is_empty(), "resumed with findings");
    let agent = &kept.agent;
    if !takes_over(ctx, stage, agent).await? {
        tracing::info!(stage = %stage.id, %agent, "slot agent cannot go on; spawning a fresh one");
        return Ok(false);
    }
    let cost_budget_usd = {
        let store = ctx.store.lock().await;
        regranted_cap(&store, stage, agent, request.cost_budget_usd)?
    };
    let update = AgentUpdate {
        deadline: request.deadline,
        cost_budget_usd,
        ..AgentUpdate::default()
    };
    let key = stage.id.as_str();
    let handed = match ctx.backend.update_agent(agent, update).await {
        Ok(()) => ctx.backend.send_message(agent, &kept.message, key).await,
        Err(error) => Err(error),
    };
    let refusal = match handed {
        Ok(()) => return Ok(true),
        Err(error) if unanswered(&error) => return Err(error),
        Err(refusal) => refusal,
    };
    tracing::info!(stage = %stage.id, %agent, %refusal, "kept agent cannot be resumed; spawning a fresh one");
    replace(ctx, agent).await?;
    Ok(false)
}

/// `kept` told the resumed or woken attempt's own deadline `until`, which replaces the one
/// its contract named.
pub(super) fn with_deadline(mut kept: KeptAgent, until: Option<Timestamp>) -> KeptAgent {
    assert!(!kept.message.trim().is_empty(), "resumed with findings");
    let Some(until) = until else {
        return kept;
    };
    kept.message.push_str(&format!(
        "\n\nArchon now stops you and every agent you delegated to at {}. If your task \
         has you commit, commit each working step as you go. Finish with time to spare.",
        until.format("%Y-%m-%d %H:%M UTC")
    ));
    kept
}

/// Aether's new cap for `agent` resumed or woken on `stage`: the cap counts its subtree's
/// whole life, so what the ledger charged it so far plus `grant`. `None` when it has no cap.
fn regranted_cap(
    store: &Store,
    stage: &StageRun,
    agent: &AetherAgentId,
    grant: Option<f64>,
) -> Result<Option<f64>, ArchonError> {
    let Some(grant) = grant else {
        return Ok(None);
    };
    assert!(grant.is_finite(), "a grant is an amount, not {grant}");
    let runs = store.stage_runs(&stage.run)?;
    let earlier: Vec<&StageRun> = runs
        .iter()
        .filter(|s| s.id != stage.id && s.aether_agent.as_ref() == Some(agent))
        .collect();
    let Some(created) = earlier.iter().min_by_key(|s| s.attempt) else {
        return Ok(None);
    };
    // The cap its create sent, as `stage_create_agent` decides it.
    let spawned = load_spawn(store, &created.id)?.request;
    if spawned.cost_budget_usd.is_none() || spawned.aether_cost_budget == Some(false) {
        return Ok(None);
    }
    // Aether counts what it reported, never Archon's estimates.
    let beneath = store.delegated_beneath(agent, false)?;
    let spent =
        store.spend_for_agent(agent, false)? + beneath.iter().map(|d| d.cost_usd).sum::<f64>();
    assert!(spent.is_finite() && spent >= 0.0, "spend is an amount");
    Ok(Some((spent + grant).max(steer::MIN_GRANT_USD)))
}

/// Stop the kept agent a fresh one takes over from, so the fresh one has
/// the worktree to itself. Safe to repeat: one already ended is done.
pub(super) async fn replace(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    end(ctx, agent, REPLACED).await
}

/// Record the kept agent on the attempt it was resumed for. Aether opened
/// no task for it, so the attempt holds the agent alone.
pub(super) async fn resumed_ok(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: AetherAgentId,
) -> Result<FollowUp, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut current = store.stage_run(&stage.id)?;
    if current.state != StageState::Spawning {
        tracing::info!(stage = %stage.id, %agent, "resumed an agent for a settled stage run; stopping it");
        return Ok(FollowUp {
            kept: vec![Kept::End(agent)],
            ..FollowUp::default()
        });
    }
    assert!(current.aether_task.is_none(), "a resume opens no task");
    let previous = store
        .stage_run_by_agent(&agent)?
        .ok_or_else(|| ArchonError::Corrupt(format!("kept agent {agent} has no recorded stage")))?;
    assert_ne!(
        previous.id, current.id,
        "a resume continues an earlier attempt"
    );
    current.host_context = previous.host_context;
    current.aether_agent = Some(agent);
    current.state = StageState::Running;
    store.upsert_stage_run(&current)?;
    let run = store.run(&stage.run)?;
    if run.state.is_terminal() {
        return Ok(FollowUp {
            stop: vec![run.id],
            ..FollowUp::default()
        });
    }
    Ok(FollowUp {
        resumed: vec![current.id],
        roster: vec![run.id],
        ..FollowUp::default()
    })
}

/// A running attempt resumed on a kept agent, with that agent and what it
/// was told; `None` for a stage run that waits for a task.
fn resumed_on(
    store: &Store,
    id: &StageRunId,
) -> Result<Option<(StageRun, KeptAgent)>, ArchonError> {
    let stage = store.stage_run(id)?;
    if stage.state != StageState::Running || stage.aether_task.is_some() {
        return Ok(None);
    }
    let Some(agent) = &stage.aether_agent else {
        return Ok(None);
    };
    let kept = match load_spawn(store, id) {
        Ok(spawn) => spawn.resume,
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    assert!(
        kept.as_ref().is_none_or(|kept| &kept.agent == agent),
        "an attempt runs on the agent it was resumed on"
    );
    Ok(kept.map(|kept| (stage, kept)))
}

/// Settle a resumed attempt once its agent answered; nothing while it works.
pub(super) async fn check(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
    let Some((stage, kept)) = resumed_on(&*ctx.store.lock().await, id)? else {
        return Ok(FollowUp::default());
    };
    let agent = &kept.agent;
    let outcome = match ctx.backend.resumed_outcome(agent, &kept.message).await {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return Ok(FollowUp::default()),
        Err(error) if unanswered(&error) => {
            tracing::info!(stage = %id, %agent, %error, "resumed agent not read; the next sweep asks again");
            return Ok(FollowUp::default());
        }
        Err(error) => return Err(error),
    };
    let outcome = match records_ship(ctx, &stage).await {
        true => {
            let run = ctx.store.lock().await.run(&stage.run)?;
            ship::with_full_ids(&run, outcome).await?
        }
        false => outcome,
    };
    let Some(outcome) = recorded(ctx, &stage, &kept, outcome).await? else {
        return Ok(FollowUp::default());
    };
    // An infrastructure failure is replaced by a fresh agent, which stops
    // this one before it starts ([`Takeover::Replace`]).
    settle::settle(ctx, id, outcome).await
}

/// `outcome` as the attempt settles on it; `None` while its agent is asked for
/// the ship record its final message left out. The answer keeps the summary.
async fn recorded(
    ctx: &DaemonContext,
    stage: &StageRun,
    kept: &KeptAgent,
    outcome: StageOutcome,
) -> Result<Option<StageOutcome>, ArchonError> {
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(&kept.agent),
        "the attempt runs on its kept agent"
    );
    let StageOutcome::Completed {
        summary,
        structured,
    } = &outcome
    else {
        return Ok(Some(outcome));
    };
    if let Some(first) = &kept.asked_record {
        assert!(!first.trim().is_empty(), "a final message has a summary");
        let structured = structured.clone();
        let summary = first.clone();
        return Ok(Some(StageOutcome::Completed {
            summary,
            structured,
        }));
    }
    let Err(why) = ShipRecord::read(structured.as_ref()) else {
        return Ok(Some(outcome));
    };
    if !records_ship(ctx, stage).await {
        return Ok(Some(outcome));
    }
    let why = if structured.is_none() {
        "it has no fenced `json` block".to_owned()
    } else {
        why
    };
    let asked = ask_for_record(ctx, stage, &kept.agent, &why, summary).await?;
    Ok((!asked).then_some(outcome))
}

/// Whether `stage` hands in the ship record archond delivers, as its run's
/// driver has it. A run without a driver is settled as it stands.
async fn records_ship(ctx: &DaemonContext, stage: &StageRun) -> bool {
    let table = ctx.runs.lock().await;
    let driver = table.drivers.get(&stage.run);
    driver.is_some_and(|driver| driver.records_ship(&stage.stage_id))
}

/// Ask `agent` once, keyed by the stage run, for the ship record its final
/// message `summary` left out. `false` when Aether refuses the message.
async fn ask_for_record(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: &AetherAgentId,
    why: &str,
    summary: &str,
) -> Result<bool, ArchonError> {
    assert_eq!(stage.state, StageState::Running, "a resumed attempt");
    assert!(!why.is_empty(), "the agent is told what is missing");
    let text = format!(
        "Archon cannot ship your fix: the ship record is not in your last message ({why}). \
         Do not change your work. Reply with the ship record of the commit you leave, read \
         after your last commit, as one fenced `json` block:\n\n{SHIP_RECORD_JSON}\n\n\
         If there is no commit to ship, start the message with `FAILED:` and say why.\n"
    );
    let key = format!("{}:record", stage.id);
    match ctx.backend.send_message(agent, &text, &key).await {
        Ok(()) => {}
        Err(error) if unanswered(&error) => {
            tracing::info!(stage = %stage.id, %agent, %error, "record request unanswered; the next sweep asks again");
            return Ok(true);
        }
        Err(refusal) => {
            tracing::info!(stage = %stage.id, %agent, %refusal, "kept agent cannot be asked for its record");
            return Ok(false);
        }
    }
    let mut store = ctx.store.lock().await;
    let mut stored = load_spawn(&store, &stage.id)?;
    let kept = stored
        .resume
        .as_mut()
        .expect("a resumed attempt keeps its agent");
    if kept.asked_record.is_none() {
        kept.message = text;
        kept.asked_record = Some(summary.to_owned());
        spawn::save_spawn(&mut store, &stage.id, &stored)?;
    }
    Ok(true)
}

/// Stop a resumed attempt's agent and settle the attempt with `outcome`.
/// An agent Aether no longer holds live has nothing left to stop.
pub(super) async fn stop(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    reason: &str,
    outcome: StageOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(
        !matches!(outcome, StageOutcome::Completed { .. }),
        "a stopped attempt did not complete"
    );
    let refusal = match ctx.backend.cancel(agent, reason).await {
        Ok(()) | Err(ArchonError::NotFound(_)) => return settle::settle(ctx, stage, outcome).await,
        Err(ArchonError::Unavailable(why)) => {
            tracing::info!(%agent, %why, "cancel unanswered; the next sweep retries");
            return Ok(FollowUp::default());
        }
        Err(refusal) => refusal,
    };
    if ctx.backend.is_live(agent).await? {
        tracing::warn!(%agent, %refusal, "cancel refused for a live agent; the next sweep retries");
        return Ok(FollowUp::default());
    }
    settle::settle(ctx, stage, outcome).await
}

/// Whether the stage run's agent was created to outlive its task. A request
/// already pruned says no, which only costs its run a turn-end wait.
fn is_kept(store: &Store, stage: &StageRunId) -> Result<bool, ArchonError> {
    match load_spawn(store, stage) {
        Ok(spawn) => Ok(spawn.request.keep),
        Err(ArchonError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether the request of settled `stage` is still read while its run's node
/// lives: its agent is kept, which the node's retirement asks; it woke its
/// slot's failed agent, which the slot's next infrastructure retry asks; or
/// its agent failed for infrastructure, and a wake regrants the cap it created.
pub(super) fn request_in_use(store: &Store, stage: &StageRun) -> Result<bool, ArchonError> {
    assert!(
        stage.state.is_terminal(),
        "a live stage run always has its request"
    );
    let wakeable = stage.infrastructure_failure && stage.aether_agent.is_some();
    let read = match load_spawn(store, &stage.id) {
        Ok(spawn) => spawn.request.keep || spawn.woken || wakeable,
        Err(ArchonError::NotFound(_)) => false,
        Err(error) => return Err(error),
    };
    Ok(read && run_node::has_node(store, &stage.run)?)
}

/// How the `slot`'s attempts woke failed agents: whether one tried, and how
/// many took their attempt. One refused spawned a fresh agent, a cold retry.
pub(super) fn wakes(store: &Store, slot: &[&StageRun]) -> Result<(bool, usize), ArchonError> {
    let (mut tried, mut took) = (false, 0);
    for stage in slot {
        let spawn = match load_spawn(store, &stage.id) {
            Ok(spawn) => spawn,
            Err(ArchonError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        tried |= spawn.woken;
        took += usize::from(spawn.woken && spawn.resume.is_some());
    }
    assert!(took <= slot.len(), "an attempt wakes one agent at most");
    assert!(
        tried || took == 0,
        "an agent took its attempt only once woken"
    );
    Ok((tried, took))
}

/// The agent of `failed` to wake in place for its replacement, told `error`;
/// `None` without one, or for a stall, whose agent the watchdog stopped.
pub(super) fn wake_for(failed: &StageRun, error: &str) -> Option<KeptAgent> {
    assert!(
        failed.infrastructure_failure,
        "only infrastructure is woken from"
    );
    let agent = failed
        .aether_agent
        .clone()
        .filter(|_| !stall::is_stall(error))?;
    Some(KeptAgent {
        agent,
        message: woken_note(error),
        asked_record: None,
    })
}

/// The agent a replacement resumes when it wakes `kept`, its slot's failed
/// agent, given the takeover its reservation found (`resume`, `replaces`).
pub(super) fn wake_in(
    resume: Option<KeptAgent>,
    replaces: Option<AetherAgentId>,
    kept: KeptAgent,
) -> KeptAgent {
    assert!(
        resume.is_none(),
        "an infrastructure replacement resumes nothing"
    );
    // A kept agent lost is stopped for a fresh one ([`Takeover::Replace`]); woken, it goes on.
    assert!(
        replaces.is_none_or(|lost| lost == kept.agent),
        "the replacement stops no other agent than the one it wakes"
    );
    kept
}

/// The kept agents among the settled `stages`: live in Aether with no turn
/// to end, until their run's node is cancelled.
pub(super) fn parked(
    store: &Store,
    stages: &[StageRun],
) -> Result<BTreeSet<AetherAgentId>, ArchonError> {
    let mut parked = BTreeSet::new();
    for stage in stages.iter().filter(|s| s.state.is_terminal()) {
        let Some(agent) = &stage.aether_agent else {
            continue;
        };
        if is_kept(store, &stage.id)? {
            parked.insert(agent.clone());
        }
    }
    assert!(
        parked.len() <= stages.len(),
        "one agent per stage run at most"
    );
    Ok(parked)
}

/// What becomes of the agent of `settled`: `None` for one that ends with its
/// task, as every agent not kept or woken does. A woken one has no task to end
/// with, so it ends with its attempt.
pub(super) fn after_settle(store: &Store, settled: &StageRun) -> Result<Option<Kept>, ArchonError> {
    assert!(settled.state.is_terminal(), "only a settled attempt");
    let Some(agent) = settled.aether_agent.clone() else {
        return Ok(None);
    };
    let (kept, woken) = match load_spawn(store, &settled.id) {
        Ok(spawn) => (spawn.request.keep, spawn.woken && spawn.resume.is_some()),
        Err(ArchonError::NotFound(_)) => (false, false),
        Err(error) => return Err(error),
    };
    assert!(
        !woken || settled.aether_task.is_none(),
        "a woken agent took its attempt without a task"
    );
    Ok(match (kept, &settled.state) {
        (true, StageState::Completed) => Some(Kept::Park(agent)),
        (true, _) => Some(Kept::End(agent)),
        (false, _) => woken.then_some(Kept::End(agent)),
    })
}

/// Park or end a kept agent, as [`after_settle`] found.
pub(super) async fn settle_kept(ctx: &DaemonContext, kept: Kept) -> Result<FollowUp, ArchonError> {
    match kept {
        Kept::Park(agent) => park(ctx, &agent).await,
        Kept::End(agent) => end(ctx, &agent, ENDED).await,
    }?;
    Ok(FollowUp::default())
}

/// Move the agent's deadline past the reviews it now waits for. One that
/// cannot be parked ends at its stage deadline and is replaced: slower only.
async fn park(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    let until = archon_core::now() + chrono::Duration::days(PARKED_DAYS);
    let update = AgentUpdate {
        deadline: Some(until),
        ..AgentUpdate::default()
    };
    if let Err(error) = ctx.backend.update_agent(agent, update).await {
        tracing::warn!(%agent, %error, "kept agent not parked; its next attempt may start fresh");
    }
    Ok(())
}

/// Stop a kept agent no attempt will resume; one already ended is done.
async fn end(ctx: &DaemonContext, agent: &AetherAgentId, reason: &str) -> Result<(), ArchonError> {
    assert!(!reason.trim().is_empty(), "a stop says why");
    if !ctx.backend.is_live(agent).await? {
        return Ok(());
    }
    let refusal = match sweep::cancel_stray(ctx, agent, reason).await {
        Ok(_) => return Ok(()),
        Err(error) if unanswered(&error) => return Err(error),
        Err(refusal) => refusal,
    };
    // Aether refuses to cancel an agent that ended in the meantime.
    if ctx.backend.is_live(agent).await? {
        return Err(refusal);
    }
    Ok(())
}
