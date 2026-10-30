//! What the watchdog does with a tree it finds waiting on something Archon
//! never asks a person for. An agent Aether blocked for inspection (a tool
//! call it started has no known outcome, as when an older aetherd restarts
//! mid-command) is resumed with a note, or its stage fails as infrastructure
//! when Aether refuses. A tree held with no cause Aether names for longer than
//! the stall fallback is told to the orchestrator once; nothing is stopped.

use std::time::Duration;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{AgentMessage, MessageKind, MessageSender};
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::run::{StageRun, StageState};

use super::{finish, resume, stall};
use crate::context::DaemonContext;

/// What a resumed agent is told before its next turn.
pub(super) const NOTE: &str = "Aether restarted while a command of yours was running, so its \
     outcome is unknown. Check the state it would have changed (files, processes, services, \
     commits) before you repeat it, then continue your task.";

/// The key of a stage whose long hold the orchestrator was told of.
pub(super) const ESCALATED_KEY_PREFIX: &str = "held_escalated:";

/// Resume the agents of `stage`'s tree that Aether blocked for inspection;
/// `Some(reason)` to fail the stage with when Aether refuses. One it does not
/// answer is tried again by the next sweep.
pub(super) async fn clear(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    now: Timestamp,
) -> Result<Option<String>, ArchonError> {
    assert_eq!(stage.state, StageState::Running, "only a running stage");
    assert_eq!(stage.aether_agent.as_ref(), Some(lead), "the stage's lead");
    // One note an hour at most, however often a lost reply has it re-sent.
    let key = format!("archon:inspection:{}:{}", stage.id, now.timestamp() / 3600);
    let resumed = match ctx.backend.clear_inspection(lead, NOTE, &key).await {
        Ok(resumed) => resumed,
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(%lead, %error, "inspection resume unanswered; tried again next sweep");
            return Ok(None);
        }
        Err(error) if error.is_store() => return Err(error),
        Err(refusal) => {
            tracing::warn!(stage = %stage.id, %refusal, "Aether refused to resume an inspection");
            return Ok(Some(stall::inspection_refused(&refusal.to_string())));
        }
    };
    if resumed.is_empty() {
        return Ok(None);
    }
    let names: Vec<String> = resumed.iter().map(ToString::to_string).collect();
    tracing::info!(stage = %stage.id, agents = ?names, "resumed agents Aether blocked for inspection");
    let item = ctx.store.lock().await.run(&stage.run)?.item;
    let text = format!(
        "stage {}: resumed {} that Aether blocked for inspection (a command's outcome was lost, \
         most likely to an Aether restart); each was told to check the state before repeating it",
        stage.stage_id,
        names.join(", ")
    );
    finish::notify(
        ctx,
        NotificationKind::Progress,
        &text,
        Some(&item),
        Some(&stage.run),
    )
    .await?;
    Ok(None)
}

/// Tell the orchestrator, once per stage run, that `stage`'s tree has been
/// held `silent` with no cause Aether names. The stage keeps running.
pub(super) async fn escalate_held(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    silent: Duration,
) -> Result<(), ArchonError> {
    assert_eq!(stage.aether_agent.as_ref(), Some(lead), "the stage's lead");
    assert!(
        silent > Duration::ZERO,
        "a long hold was silent for a while"
    );
    let key = format!("{ESCALATED_KEY_PREFIX}{}", stage.id);
    let mut store = ctx.store.lock().await;
    if store.kv_get(&key)?.is_some() {
        return Ok(());
    }
    let item = store.run(&stage.run)?.item;
    let text = format!(
        "Stage {} ({}) has been held by Aether for {} min with no approval, usage limit or \
         credentials wait and no gate or question open (agent {lead}). Archon keeps it running; \
         inspect it in Aether, or steer, retry or cancel the item.",
        stage.stage_id,
        stage.id,
        silent.as_secs() / 60
    );
    let message = AgentMessage {
        item,
        run: stage.run.clone(),
        stage: Some(stage.stage_id.clone()),
        stage_run: Some(stage.id.clone()),
        attempt: Some(stage.attempt),
        agent: Some(lead.clone()),
        kind: MessageKind::Escalation,
        text,
        original_sender: MessageSender::System,
        addressed_to_run_node: None,
    };
    let saved = crate::triggers::record_message(&mut store, message, &[(&key, "told")])?;
    drop(store);
    tracing::warn!(stage = %stage.id, %lead, "held with no named cause; orchestrator told");
    crate::feed::publish(ctx, vec![saved]);
    crate::triggers::wake(ctx);
    Ok(())
}
