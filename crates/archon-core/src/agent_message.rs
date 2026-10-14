//! Messages routed through Archon, with provenance independent of their text.

use serde::{Deserialize, Serialize};

use crate::aether_ids::AetherAgentId;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QueueItemId, StageRunId};

pub const MAX_MESSAGE_CHARS: usize = 4096;
/// `[limits] max_agent_messages` by default, and the most it may be: every
/// message is an orchestrator turn.
pub const DEFAULT_AGENT_MESSAGES: u32 = 20;
pub const AGENT_MESSAGES_CEILING: u32 = 200;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Relay,
    Escalation,
    #[default]
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "actor", rename_all = "snake_case")]
pub enum MessageSender {
    Operator,
    Agent { id: AetherAgentId },
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub item: QueueItemId,
    pub run: PipelineRunId,
    pub stage: Option<String>,
    pub stage_run: Option<StageRunId>,
    pub attempt: Option<u32>,
    pub agent: Option<AetherAgentId>,
    pub kind: MessageKind,
    pub text: String,
    pub original_sender: MessageSender,
    pub addressed_to_run_node: Option<AetherAgentId>,
}

pub fn check_text(text: &str) -> Result<(), ArchonError> {
    if text.trim().is_empty() || text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ArchonError::Invalid(format!(
            "message text must be non-blank and at most {MAX_MESSAGE_CHARS} characters"
        )));
    }
    Ok(())
}
