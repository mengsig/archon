use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::NotificationId;
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context::DaemonContext;
use crate::{feed, supervisor, triggers};

pub(super) const EVENT_NAME: &str = "turn_ended_after_tool_error";
const CHAIN_KEY: &str = "orchestrator_tool_error_chain";

#[derive(Deserialize)]
struct FailedCall {
    agent_id: AetherAgentId,
    turn: u32,
    tool: String,
    error: String,
}

#[derive(Deserialize, Serialize)]
struct Chain {
    agent: AetherAgentId,
    last_turn: u32,
    escalated: bool,
}

pub(super) async fn handle(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    payload: &Value,
) -> Result<(), ArchonError> {
    let call: FailedCall = serde_json::from_value(payload.clone())
        .map_err(|error| ArchonError::Protocol(format!("invalid {EVENT_NAME} payload: {error}")))?;
    if call.agent_id != *agent || call.turn == 0 || call.tool.trim().is_empty() {
        return Err(ArchonError::Protocol(format!(
            "{EVENT_NAME} must name its agent, a positive turn and a tool"
        )));
    }
    let _steady = supervisor::steady(ctx).await;
    if supervisor::current_orchestrator(ctx).await?.as_ref() != Some(agent) {
        return Ok(());
    }
    let previous = load_chain(ctx).await?.filter(|chain| chain.agent == *agent);
    if previous
        .as_ref()
        .is_some_and(|chain| call.turn <= chain.last_turn)
    {
        return Ok(());
    }
    // Aether emits the event only for a failed turn. A gap starts a fresh chain.
    let consecutive = previous.filter(|chain| chain.last_turn.checked_add(1) == Some(call.turn));
    let next = Chain {
        agent: agent.clone(),
        last_turn: call.turn,
        escalated: consecutive.is_some(),
    };
    match consecutive {
        None => wake(ctx, agent, &call).await?,
        Some(chain) if !chain.escalated => return notify(ctx, &call, &next).await,
        Some(_) => {}
    }
    let saved = serde_json::to_string(&next)?;
    ctx.store.lock().await.kv_set(CHAIN_KEY, &saved)
}

async fn load_chain(ctx: &DaemonContext) -> Result<Option<Chain>, ArchonError> {
    let Some(raw) = ctx.store.lock().await.kv_get(CHAIN_KEY)? else {
        return Ok(None);
    };
    let chain: Chain = serde_json::from_str(&raw)
        .map_err(|error| ArchonError::Corrupt(format!("kv {CHAIN_KEY}: {error}")))?;
    if chain.last_turn == 0 || (chain.last_turn == 1 && chain.escalated) {
        return Err(ArchonError::Corrupt(format!(
            "kv {CHAIN_KEY}: invalid turn {} for the stored escalation state",
            chain.last_turn
        )));
    }
    Ok(Some(chain))
}

async fn wake(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    call: &FailedCall,
) -> Result<(), ArchonError> {
    assert_eq!(
        &call.agent_id, agent,
        "the wake is for the agent that called the tool"
    );
    assert!(call.turn > 0, "the key identifies a real turn");
    let key = format!("archon:tool-error:{agent}:{}", call.turn);
    let payload = json!({
        "archon": triggers::PAYLOAD_VERSION,
        "events": [{
            "type": EVENT_NAME,
            "turn": call.turn,
            "tool": call.tool,
            "error": call.error,
            "text": "Your turn ended after a tool error. Retry the failed call if safe, or tell the operator what failed."
        }],
        "summary": "The orchestrator ended a turn on a tool error",
    });
    ctx.aether.client()?.trigger(agent, payload, &key).await
}

async fn notify(ctx: &DaemonContext, call: &FailedCall, chain: &Chain) -> Result<(), ArchonError> {
    assert!(
        chain.escalated && call.turn > 1,
        "a blocker requires two turns"
    );
    assert_eq!((&chain.agent, chain.last_turn), (&call.agent_id, call.turn));
    let saved = serde_json::to_string(chain)?;
    let at = archon_core::now();
    let entry = FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind: NotificationKind::Blocker,
        text: format!(
            "The orchestrator ended two consecutive turns on tool errors. `{}` returned {:?} on turn {}.",
            call.tool, call.error, call.turn
        ),
        item: None,
        run: None,
        at,
    });
    // Save the receipt with the blocker so replay cannot announce it again.
    let item = ctx
        .store
        .lock()
        .await
        .append_feed_with_kv(entry, at, &[(CHAIN_KEY, &saved)])?;
    feed::publish(ctx, vec![item]);
    Ok(())
}
