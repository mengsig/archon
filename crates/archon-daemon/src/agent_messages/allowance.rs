//! What one agent may still send the orchestrator: its counted allowance,
//! the escalation reserve past it, and the replies granted to it.

use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{ESCALATION_RESERVE, MAX_REPLY_GRANTS, MessageCap, MessageKind};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::FeedItem;
use archon_core::run::StageRun;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::read;
use crate::triggers;

const CAP_PREFIX: &str = "message_cap:";

fn count_key(agent: &AetherAgentId) -> String {
    format!("message_count:{agent}")
}

fn reserve_key(agent: &AetherAgentId) -> String {
    format!("message_reserve:{agent}")
}

fn grants_key(agent: &AetherAgentId) -> String {
    format!("message_grants:{agent}")
}

fn cap_key(agent: &AetherAgentId) -> String {
    format!("{CAP_PREFIX}{agent}")
}

/// Replies that count against nothing; `inbox_seq` is the last inbox item
/// that granted one, so a replayed delivery grants nothing.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Grants {
    left: u32,
    inbox_seq: u64,
}

/// How one more message is paid for.
pub(super) enum Charge {
    /// The kv write that pays for it.
    Paid {
        key: String,
        value: String,
    },
    Refused {
        reserve_left: u32,
    },
}

/// Pay for one more message of `kind` from `agent`: a granted reply first,
/// then its allowance of `most`, then, for an escalation, the reserve.
pub(super) fn charge(
    store: &Store,
    agent: &AetherAgentId,
    kind: MessageKind,
    most: u32,
) -> Result<Charge, ArchonError> {
    assert!(most >= 1, "validated with the config");
    let grants: Grants = read(store, &grants_key(agent))?.unwrap_or_default();
    assert!(
        grants.left <= MAX_REPLY_GRANTS,
        "grants are capped as given"
    );
    if grants.left > 0 {
        let left = Grants {
            left: grants.left - 1,
            ..grants
        };
        let value = serde_json::to_string(&left)?;
        return Ok(Charge::Paid {
            key: grants_key(agent),
            value,
        });
    }
    let count: u32 = read(store, &count_key(agent))?.unwrap_or(0);
    if count < most {
        let (key, value) = (count_key(agent), (count + 1).to_string());
        return Ok(Charge::Paid { key, value });
    }
    let reserved: u32 = read(store, &reserve_key(agent))?.unwrap_or(0);
    assert!(
        reserved <= ESCALATION_RESERVE,
        "the reserve is never overdrawn"
    );
    if kind == MessageKind::Escalation && reserved < ESCALATION_RESERVE {
        let (key, value) = (reserve_key(agent), (reserved + 1).to_string());
        return Ok(Charge::Paid { key, value });
    }
    Ok(Charge::Refused {
        reserve_left: ESCALATION_RESERVE - reserved,
    })
}

/// A refused message, with the orchestrator's notice when it was the
/// agent's first refusal.
pub(crate) struct Refusal {
    pub(crate) error: ArchonError,
    pub(crate) notice: Option<Box<FeedItem>>,
}

impl From<ArchonError> for Refusal {
    fn from(error: ArchonError) -> Self {
        Self {
            error,
            notice: None,
        }
    }
}

/// Refuse a message from `agent` past its allowance, telling the
/// orchestrator once when `agent` runs `stage`.
pub(super) fn refuse(
    store: &mut Store,
    agent: &AetherAgentId,
    stage: Option<&StageRun>,
    most: u32,
    reserve_left: u32,
) -> Result<Refusal, ArchonError> {
    assert!(reserve_left <= ESCALATION_RESERVE);
    let notice = match stage {
        Some(stage) => notice_once(store, agent, stage, most)?.map(Box::new),
        None => None,
    };
    let reserve = match reserve_left {
        0 => format!("Its reserve of {ESCALATION_RESERVE} escalations is used up too."),
        left => format!("An `escalation` may still use {left} of its reserve."),
    };
    let text = format!(
        "message_orchestrator limit reached ({most} messages per agent, `[limits] max_agent_messages`): \
         the allowance of {agent}, the sender, is used up, so this message was refused. \
         Messages an agent sends for the agents it delegated to are charged to it, not to them. \
         {reserve} Each send_message or steer note the orchestrator sends {agent} grants it one reply."
    );
    Ok(Refusal {
        error: ArchonError::Invalid(text),
        notice,
    })
}

fn notice_once(
    store: &mut Store,
    agent: &AetherAgentId,
    stage: &StageRun,
    most: u32,
) -> Result<Option<FeedItem>, ArchonError> {
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(agent),
        "the stage's own agent"
    );
    let key = cap_key(agent);
    if stage.state.is_terminal() || store.kv_get(&key)?.is_some() {
        return Ok(None);
    }
    let cap = MessageCap {
        item: store.run(&stage.run)?.item,
        stage_run: stage.id.clone(),
        stage: stage.stage_id.clone(),
        agent: agent.clone(),
        cap: most,
        at: archon_core::now(),
    };
    triggers::record_cap(store, &stage.run, cap, &key).map(Some)
}

/// One more uncounted reply for `agent`, up to [`MAX_REPLY_GRANTS`]; an
/// inbox delivery at or before the last that granted one grants nothing.
pub(crate) fn grant_reply(
    store: &mut Store,
    agent: &AetherAgentId,
    inbox_seq: Option<u64>,
) -> Result<(), ArchonError> {
    let key = grants_key(agent);
    let mut grants: Grants = read(store, &key)?.unwrap_or_default();
    if let Some(seq) = inbox_seq {
        if seq <= grants.inbox_seq {
            return Ok(());
        }
        grants.inbox_seq = seq;
    }
    grants.left = (grants.left + 1).min(MAX_REPLY_GRANTS);
    assert!(grants.left >= 1, "a grant leaves at least one reply");
    store.kv_set(&key, &serde_json::to_string(&grants)?)
}

/// Whether `agent` runs a stage that has not settled.
pub(super) fn runs_live_stage(store: &Store, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    Ok(store
        .stage_run_by_agent(agent)?
        .is_some_and(|stage| !stage.state.is_terminal()))
}

/// The agents of unsettled stages, of `only` when given, that reached their
/// cap, first refused first.
pub(crate) fn caps(
    store: &Store,
    only: Option<&QueueItemId>,
) -> Result<Vec<MessageCap>, ArchonError> {
    let mut found = Vec::new();
    for (key, raw) in store.kv_prefix(CAP_PREFIX)? {
        let cap: MessageCap = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
        if key != cap_key(&cap.agent) {
            return Err(ArchonError::Corrupt(format!(
                "kv {key} holds the cap of {}",
                cap.agent
            )));
        }
        if only.is_some_and(|item| *item != cap.item) || !runs_live_stage(store, &cap.agent)? {
            continue;
        }
        found.push(cap);
    }
    found.sort_by_key(|cap| cap.at);
    Ok(found)
}
