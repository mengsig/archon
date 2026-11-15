//! Messages routed through Archon, with provenance independent of their text.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::AetherAgentId;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QueueItemId, StageRunId};

pub const MAX_MESSAGE_CHARS: usize = 4096;
/// `[limits] max_agent_messages` by default, and the most it may be: every
/// counted message is an orchestrator turn; clarification questions are exempt.
pub const DEFAULT_AGENT_MESSAGES: u32 = 20;
pub const AGENT_MESSAGES_CEILING: u32 = 200;
/// Escalations a stage agent may still send once its allowance is used up,
/// so `info` chatter never silences the last escalation.
pub const ESCALATION_RESERVE: u32 = 3;
/// Most uncounted replies an agent holds: each orchestrator message or steer
/// note to it grants one, up to this many.
pub const MAX_REPLY_GRANTS: u32 = 3;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Relay,
    Question,
    Escalation,
    #[default]
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "actor", rename_all = "snake_case")]
pub enum MessageSender {
    Operator,
    Agent { id: AetherAgentId },
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// A stage agent whose `[limits] max_agent_messages` allowance is used up,
/// as told once to the orchestrator and shown by `get_item` and `queue_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MessageCap {
    pub item: QueueItemId,
    pub stage_run: StageRunId,
    /// The stage's name in its pipeline.
    pub stage: String,
    pub agent: AetherAgentId,
    /// The allowance it used up.
    pub cap: u32,
    /// When its first message was refused.
    pub at: Timestamp,
}

impl MessageCap {
    /// `stage build of qi_…: agt_… reached its message cap (20, …)…`.
    pub fn describe(&self) -> String {
        format!(
            "stage {} of {}: {} reached its message cap ({}, `[limits] max_agent_messages`) at {}; \
             its info and relay messages are refused, it keeps {ESCALATION_RESERVE} escalations \
             in reserve, clarification questions remain uncapped, and each send_message or steer note to it grants one reply",
            self.stage,
            self.item,
            self.agent,
            self.cap,
            self.at.to_rfc3339()
        )
    }
}

pub fn check_text(text: &str) -> Result<(), ArchonError> {
    if text.trim().is_empty() || text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ArchonError::Invalid(format!(
            "message text must be non-blank and at most {MAX_MESSAGE_CHARS} characters"
        )));
    }
    Ok(())
}

pub fn check_question(text: &str) -> Result<(), ArchonError> {
    if text.trim().is_empty() {
        return Err(ArchonError::Invalid(
            "question text must be non-blank".into(),
        ));
    }
    Ok(())
}

/// A question carries the stage's best judgment in case no answer arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClarificationOptions {
    pub fallback: String,
    #[serde(default)]
    pub proceed_directive: Option<crate::id::DirectiveId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClarificationResolution {
    Answered {
        answer: String,
        at: Timestamp,
    },
    Assumed {
        choice: String,
        reason: String,
        at: Timestamp,
    },
}

/// Durable question and its answer or recorded assumption, retained after the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Clarification {
    pub id: crate::id::NotificationId,
    pub item: QueueItemId,
    pub stage_run: StageRunId,
    pub agent: AetherAgentId,
    pub question: String,
    pub fallback: String,
    pub asked_at: Timestamp,
    pub deadline: Timestamp,
    pub resolution: Option<ClarificationResolution>,
}

impl Clarification {
    pub fn describe(&self) -> String {
        let resolution = match &self.resolution {
            None => "open".to_owned(),
            Some(ClarificationResolution::Answered { answer, at }) => {
                format!("answered at {at}: {answer}")
            }
            Some(ClarificationResolution::Assumed { choice, reason, at }) => {
                format!("assumed at {at}: {choice} ({reason})")
            }
        };
        format!(
            "Clarification [clarification:{}] for {} ({}): {}. Fallback: {}. Deadline: {}. Resolution: {}",
            self.id, self.item, self.agent, self.question, self.fallback, self.deadline, resolution
        )
    }
}
