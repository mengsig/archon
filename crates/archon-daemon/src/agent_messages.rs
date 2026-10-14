//! Stage messages and passive run-node inboxes share durable news delivery.

use archon_aether::events::InboxMessage;
use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{AgentMessage, MessageKind, MessageSender, check_text};
use archon_core::error::ArchonError;
use archon_core::notify::FeedItem;
use archon_core::protocol::Reply;
use archon_core::run::StageRun;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;
use crate::pump::FleetRole;
use crate::{executor, feed, triggers};

#[derive(Serialize, Deserialize)]
struct OperatorMessage {
    seq: u64,
    text: String,
}

fn operator_key(agent: &AetherAgentId) -> String {
    format!("message_operator:{agent}")
}

fn counter_key(sender: &MessageSender, target: &AetherAgentId) -> String {
    match sender {
        MessageSender::Agent { id } => format!("message_count:{id}"),
        MessageSender::Operator | MessageSender::System => {
            format!("message_count:{target}:external")
        }
    }
}

fn read<T: serde::de::DeserializeOwned>(
    store: &Store,
    key: &str,
) -> Result<Option<T>, ArchonError> {
    store
        .kv_get(key)?
        .map(|raw| {
            serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))
        })
        .transpose()
}

/// The count `key` reaches with one more message, within `most`.
fn next_count(store: &Store, key: &str, most: u32) -> Result<u32, ArchonError> {
    assert!(most >= 1, "validated with the config");
    let count: u32 = read(store, key)?.unwrap_or(0);
    if count >= most {
        return Err(ArchonError::Invalid(format!(
            "message_orchestrator limit reached ({most} messages per agent or run-node external inbox, `[limits] max_agent_messages`); this message was refused"
        )));
    }
    Ok(count + 1)
}

fn stage_fields(message: &mut AgentMessage, stage: &StageRun) {
    message.stage = Some(stage.stage_id.clone());
    message.stage_run = Some(stage.id.clone());
    message.attempt = Some(stage.attempt);
}

pub(crate) async fn send(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    kind: MessageKind,
    text: String,
) -> Result<Reply, ArchonError> {
    check_text(&text)?;
    let agent = caller.ok_or_else(|| {
        ArchonError::Forbidden(
            "message_orchestrator requires Aether caller metadata (send_caller_meta = true)".into(),
        )
    })?;
    let receipt = {
        let mut store = ctx.store.lock().await;
        let Some(stage) = store.stage_run_by_agent(agent)? else {
            drop(store);
            return Err(not_a_stage_agent(ctx, agent).await?);
        };
        if stage.state.is_terminal() {
            return Err(ArchonError::Forbidden(
                "this stage has already settled".into(),
            ));
        }
        let run = store.run(&stage.run)?;
        let sender = MessageSender::Agent { id: agent.clone() };
        let key = counter_key(&sender, agent);
        let most = ctx.config.limits.max_agent_messages;
        let count = next_count(&store, &key, most)?.to_string();
        let operator: Option<OperatorMessage> = read(&store, &operator_key(agent))?;
        let original_sender =
            if kind == MessageKind::Relay && operator.is_some_and(|m| m.text == text) {
                MessageSender::Operator
            } else {
                sender
            };
        let mut message = AgentMessage {
            item: run.item,
            run: run.id,
            stage: None,
            stage_run: None,
            attempt: None,
            agent: Some(agent.clone()),
            kind,
            text,
            original_sender,
            addressed_to_run_node: None,
        };
        stage_fields(&mut message, &stage);
        triggers::record_message(&mut store, message, &[(&key, &count)])?
    };
    publish(ctx, &receipt);
    Ok(Reply::MessageReceipt {
        receipt: receipt.id,
    })
}

/// Why `agent` may not message the orchestrator, said so that only an agent
/// whose stage is still being recorded tries again.
async fn not_a_stage_agent(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<ArchonError, ArchonError> {
    let why = match crate::pump::fleet_role(ctx, agent).await? {
        Some(FleetRole::Stage(_)) => format!(
            "the stage of {agent} is still being recorded. Try once more in a few seconds, and keep working meanwhile"
        ),
        Some(FleetRole::StageChild(item)) => format!(
            "{agent} was delegated part of item {item} by one. Do not retry: report to the agent that delegated to you, which can send the message"
        ),
        Some(FleetRole::OrchestratorChild) => format!(
            "{agent} was delegated to by the orchestrator. Do not retry: put what it should know in your result"
        ),
        None => format!(
            "Archon has no stage on record for {agent}. If your stage started moments ago, try once more in a few seconds; otherwise do not retry"
        ),
    };
    Ok(ArchonError::Forbidden(format!(
        "message_orchestrator is for stage agents, and {why}. Nothing was sent."
    )))
}

fn publish(ctx: &DaemonContext, receipt: &FeedItem) {
    feed::publish(ctx, vec![receipt.clone()]);
    triggers::wake(ctx);
}

pub(crate) async fn inbox(
    ctx: &DaemonContext,
    target: &AetherAgentId,
    seq: u64,
    incoming: &InboxMessage,
) -> Result<(), ArchonError> {
    let cursor = format!("message_inbox:{target}");
    let known = {
        let store = ctx.store.lock().await;
        let seen: u64 = read(&store, &cursor)?.unwrap_or(0);
        if seq <= seen {
            return Ok(());
        }
        executor::node_run(&store, target)?.is_some()
            || store.stage_run_by_agent(target)?.is_some()
            || store
                .kv_get(archon_queue::store::keys::ORCHESTRATOR_AGENT)?
                .as_deref()
                == Some(target.as_str())
    };
    if !known {
        match executor::recover_message_node(ctx, target).await {
            Ok(()) => {}
            Err(error) if error.is_store() || matches!(error, ArchonError::Unavailable(_)) => {
                return Err(error);
            }
            Err(refusal) => {
                refuse_inbox(ctx, target, seq, incoming, &refusal).await?;
                ctx.store.lock().await.kv_set(&cursor, &seq.to_string())?;
                return Ok(());
            }
        }
    }
    let result = {
        let mut store = ctx.store.lock().await;
        let seen: u64 = read(&store, &cursor)?.unwrap_or(0);
        if seq <= seen {
            return Ok(());
        }
        let Some(run) = executor::node_run(&store, target)? else {
            remember_operator(&mut store, target, seq, incoming)?;
            return Ok(());
        };
        let key = counter_key(&incoming.sender, target);
        let most = ctx.config.limits.max_agent_messages;
        match check_text(&incoming.text).and_then(|()| next_count(&store, &key, most)) {
            Ok(count) => {
                let count = count.to_string();
                let seq = seq.to_string();
                node_message(&store, target, run, incoming).and_then(|message| {
                    triggers::record_message(
                        &mut store,
                        message,
                        &[(&key, &count), (&cursor, &seq)],
                    )
                })
            }
            Err(refusal) => Err(refusal),
        }
    };
    match result {
        Ok(receipt) => publish(ctx, &receipt),
        Err(error) if error.is_store() => return Err(error),
        Err(refusal) => {
            refuse_inbox(ctx, target, seq, incoming, &refusal).await?;
            ctx.store.lock().await.kv_set(&cursor, &seq.to_string())?;
        }
    }
    Ok(())
}

fn remember_operator(
    store: &mut Store,
    target: &AetherAgentId,
    seq: u64,
    incoming: &InboxMessage,
) -> Result<(), ArchonError> {
    if incoming.sender != MessageSender::Operator || store.stage_run_by_agent(target)?.is_none() {
        return Ok(());
    }
    let key = operator_key(target);
    let old: Option<OperatorMessage> = read(store, &key)?;
    if old.is_some_and(|m| seq <= m.seq) {
        return Ok(());
    }
    let text = if check_text(&incoming.text).is_ok() {
        incoming.text.clone()
    } else {
        String::new()
    };
    store.kv_set(
        &key,
        &serde_json::to_string(&OperatorMessage { seq, text })?,
    )
}

fn node_message(
    store: &Store,
    target: &AetherAgentId,
    run: archon_core::id::PipelineRunId,
    incoming: &InboxMessage,
) -> Result<AgentMessage, ArchonError> {
    let mut message = AgentMessage {
        item: store.run(&run)?.item,
        run,
        stage: None,
        stage_run: None,
        attempt: None,
        agent: None,
        kind: if incoming.sender == MessageSender::Operator {
            MessageKind::Relay
        } else {
            MessageKind::Info
        },
        text: incoming.text.clone(),
        original_sender: incoming.sender.clone(),
        addressed_to_run_node: Some(target.clone()),
    };
    if let MessageSender::Agent { id } = &incoming.sender {
        message.agent = Some(id.clone());
        if let Some(stage) = store.stage_run_by_agent(id)?
            && stage.run == message.run
        {
            stage_fields(&mut message, &stage);
        }
    }
    Ok(message)
}

async fn refuse_inbox(
    ctx: &DaemonContext,
    target: &AetherAgentId,
    seq: u64,
    incoming: &InboxMessage,
    refusal: &ArchonError,
) -> Result<(), ArchonError> {
    let text = format!(
        "Message to passive Archon run node {target} was refused: {refusal}. Use message_orchestrator for bounded stage messages and continue your stage."
    );
    if let MessageSender::Agent { id } = &incoming.sender {
        match ctx
            .backend
            .send_message(id, &text, &format!("message_refusal:{target}:{seq}"))
            .await
        {
            Ok(()) => {}
            Err(error) if error.is_store() || matches!(error, ArchonError::Unavailable(_)) => {
                return Err(error);
            }
            Err(error) => {
                feed::system(
                    ctx,
                    format!("{text} Could not deliver the refusal to {id}: {error}"),
                )
                .await?;
            }
        }
    } else {
        feed::system(ctx, text).await?;
    }
    Ok(())
}
