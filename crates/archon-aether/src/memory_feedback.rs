//! Optional context and memory feedback commands, gated by Welcome features.

use archon_core::Timestamp;
use archon_core::aether_ids::{AetherMemoryFeedbackId, AetherMemoryId, AetherTaskId};
use archon_core::error::ArchonError;
use serde::{Deserialize, Serialize};

use crate::AetherClient;
use crate::wire::Command;

pub const MEMORY_FEEDBACK_FEATURE: &str = "memory_feedback";
pub const MEMORY_CONTEXT_FEATURE: &str = "memory_context";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEvidence {
    Operator,
    Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordMemoryFeedback {
    pub memory: AetherMemoryId,
    pub verdict: String,
    pub evidence: MemoryEvidence,
    pub task: AetherTaskId,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MemoryFeedbackRecord {
    pub id: AetherMemoryFeedbackId,
    pub memory: AetherMemoryId,
    pub verdict: String,
    pub evidence: MemoryEvidence,
    pub task: AetherTaskId,
    pub reason: String,
    pub created_at: Timestamp,
    pub retracted_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MemoriesInContext {
    pub snapshots: Vec<MemorySnapshot>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MemorySnapshot {
    pub task: Option<AetherTaskId>,
    pub memories: Vec<AetherMemoryId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackCommand {
    pub cmd: &'static str,
    #[serde(flatten)]
    pub request: RecordMemoryFeedback,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum SubmissionResolution {
    Executed { reply: crate::wire::RawReply },
    AbsentSafe,
    Unknown { reason: String },
}

#[derive(Debug)]
pub enum FeedbackResolution {
    Executed(MemoryFeedbackRecord),
    AbsentSafe,
    Unknown(String),
}

impl AetherClient {
    pub fn supports_memory_feature(&self, feature: &str) -> bool {
        self.welcome().features.iter().any(|f| f == feature)
    }

    fn require_memory_feature(&self, feature: &str) -> Result<(), ArchonError> {
        if !self.supports_memory_feature(feature) {
            return Err(ArchonError::Aether(format!(
                "aetherd does not advertise {feature}"
            )));
        }
        assert!(
            !self.welcome().instance_id.is_empty(),
            "a negotiated instance"
        );
        assert!(self.welcome().features.iter().any(|f| f == feature));
        Ok(())
    }

    pub async fn memories_in_context(
        &self,
        task: &AetherTaskId,
    ) -> Result<MemoriesInContext, ArchonError> {
        self.require_memory_feature(MEMORY_CONTEXT_FEATURE)?;
        let data = self
            .request(
                Command::MemoriesInContext { task: task.clone() },
                "memories_in_context",
            )
            .await?;
        let context: MemoriesInContext = decode(data)?;
        if context
            .snapshots
            .iter()
            .any(|s| s.task.as_ref().is_some_and(|t| t != task))
        {
            return Err(ArchonError::Protocol(
                "memory context answered for another task".into(),
            ));
        }
        assert!(
            context
                .snapshots
                .iter()
                .all(|s| s.task.as_ref().is_none_or(|t| t == task))
        );
        assert!(self.supports_memory_feature(MEMORY_CONTEXT_FEATURE));
        Ok(context)
    }

    pub async fn record_memory_feedback(
        &self,
        request: RecordMemoryFeedback,
    ) -> Result<MemoryFeedbackRecord, ArchonError> {
        self.require_memory_feature(MEMORY_FEEDBACK_FEATURE)?;
        check_reason(&request.reason)?;
        if request.verdict != "hurt"
            || request.idempotency_key.is_empty()
            || request.idempotency_key.len() > 200
            || !request
                .idempotency_key
                .bytes()
                .all(|b| b.is_ascii_graphic())
        {
            return Err(ArchonError::Invalid(
                "invalid memory feedback verdict or idempotency key".into(),
            ));
        }
        let data = self
            .request(
                Command::RecordMemoryFeedback(request.clone()),
                "memory_feedback",
            )
            .await?;
        Self::validate_feedback(decode(data)?, &request)
    }

    fn validate_feedback(
        record: MemoryFeedbackRecord,
        request: &RecordMemoryFeedback,
    ) -> Result<MemoryFeedbackRecord, ArchonError> {
        if record.memory != request.memory
            || record.task != request.task
            || record.verdict != request.verdict
            || record.evidence != request.evidence
            || record.reason != request.reason
        {
            return Err(ArchonError::Protocol(
                "memory feedback reply differs from the request".into(),
            ));
        }
        assert_eq!(record.memory, request.memory, "receipt names its memory");
        assert_eq!(record.task, request.task, "receipt names its task");
        Ok(record)
    }

    pub async fn resolve_memory_feedback(
        &self,
        request: RecordMemoryFeedback,
        horizon: Option<String>,
    ) -> Result<FeedbackResolution, ArchonError> {
        assert!(!request.idempotency_key.is_empty());
        assert!(!request.reason.trim().is_empty());
        if self.welcome().submission_horizon.is_none() {
            return Ok(FeedbackResolution::Unknown(
                "Aether cannot resolve lost submissions; upgrade Aether".into(),
            ));
        }
        let data = self
            .request(
                Command::ResolveSubmission {
                    command: FeedbackCommand {
                        cmd: "record_memory_feedback",
                        request: request.clone(),
                    },
                    horizon,
                },
                "submission_resolved",
            )
            .await?;
        match decode(data)? {
            SubmissionResolution::Executed { reply } => {
                if reply.reply != "memory_feedback" {
                    return Err(ArchonError::Protocol(
                        "resolved receipt is not memory feedback".into(),
                    ));
                }
                Self::validate_feedback(decode(reply.data)?, &request)
                    .map(FeedbackResolution::Executed)
            }
            SubmissionResolution::AbsentSafe => Ok(FeedbackResolution::AbsentSafe),
            SubmissionResolution::Unknown { reason } => Ok(FeedbackResolution::Unknown(reason)),
        }
    }

    pub async fn retract_memory_feedback(
        &self,
        feedback: &AetherMemoryFeedbackId,
        reason: &str,
    ) -> Result<MemoryFeedbackRecord, ArchonError> {
        self.require_memory_feature(MEMORY_FEEDBACK_FEATURE)?;
        check_reason(reason)?;
        let data = self
            .request(
                Command::RetractMemoryFeedback {
                    feedback: feedback.clone(),
                    reason: reason.to_owned(),
                },
                "memory_feedback",
            )
            .await?;
        let record: MemoryFeedbackRecord = decode(data)?;
        if &record.id != feedback || record.retracted_at.is_none() {
            return Err(ArchonError::Protocol(
                "memory feedback retraction was not confirmed".into(),
            ));
        }
        assert_eq!(&record.id, feedback, "retract the requested receipt");
        assert!(record.retracted_at.is_some(), "retraction is dated");
        Ok(record)
    }
}

fn check_reason(reason: &str) -> Result<(), ArchonError> {
    if reason.trim().is_empty() || reason.chars().count() > 4096 {
        return Err(ArchonError::Invalid(
            "memory feedback reason needs 1–4096 characters".into(),
        ));
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(data: serde_json::Value) -> Result<T, ArchonError> {
    serde_json::from_value(data)
        .map_err(|e| ArchonError::Protocol(format!("memory feedback reply: {e}")))
}
