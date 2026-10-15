//! Durable delivery ledger for evidence about the corrected stage's memories.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::aether_ids::{AetherMemoryFeedbackId, AetherMemoryId, AetherTaskId};
use crate::error::ArchonError;
use crate::id::FeedbackId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFeedback {
    pub instance_id: String,
    pub skipped: Option<String>,
    pub tasks: Vec<MemoryFeedbackTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFeedbackTask {
    pub task: AetherTaskId,
    pub discovered: bool,
    pub partial: bool,
    pub skipped: Option<String>,
    pub error: Option<String>,
    pub records: Vec<MemoryFeedbackEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFeedbackEntry {
    pub memory: AetherMemoryId,
    pub feedback: Option<AetherMemoryFeedbackId>,
    /// Persisted before sending: a lost reply must be recovered before undo.
    pub attempted: bool,
    #[serde(default)]
    pub submission_horizon: Option<String>,
    #[serde(default)]
    pub unconfirmed: bool,
    #[serde(default)]
    pub failed: Option<String>,
    pub retracted: bool,
    pub dropped: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFeedbackCounts {
    pub sent: usize,
    /// Includes tasks still awaiting context discovery.
    pub pending: usize,
    pub skipped: usize,
    pub retracted: usize,
    pub partial_tasks: usize,
}

impl MemoryFeedback {
    pub fn check(&self) -> Result<(), ArchonError> {
        if self.instance_id.is_empty() && self.skipped.is_none() {
            return Err(ArchonError::Invalid(
                "memory feedback needs an Aether instance".into(),
            ));
        }
        let mut tasks = BTreeSet::new();
        for task in &self.tasks {
            if !tasks.insert(&task.task) {
                return Err(ArchonError::Invalid(
                    "duplicate memory feedback task".into(),
                ));
            }
            let mut memories = BTreeSet::new();
            for entry in &task.records {
                if !task.discovered
                    || !memories.insert(&entry.memory)
                    || (entry.feedback.is_some() && !entry.attempted)
                    || (entry.retracted && entry.feedback.is_none())
                    || (entry.dropped && entry.attempted)
                    || (entry.unconfirmed && !entry.attempted)
                    || entry
                        .submission_horizon
                        .as_ref()
                        .is_some_and(|h| h.is_empty() || !entry.attempted)
                    || entry.failed.as_ref().is_some_and(|reason| {
                        reason.is_empty()
                            || entry.dropped
                            || entry.retracted
                            || entry.error.is_some()
                    })
                {
                    return Err(ArchonError::Invalid(
                        "inconsistent memory feedback entry".into(),
                    ));
                }
            }
        }
        assert_eq!(tasks.len(), self.tasks.len(), "one discovery per task");
        assert!(self.skipped.is_some() || !self.instance_id.is_empty());
        Ok(())
    }

    pub fn counts(&self) -> MemoryFeedbackCounts {
        assert!(self.check().is_ok(), "count a valid delivery ledger");
        let mut counts = MemoryFeedbackCounts {
            skipped: usize::from(self.skipped.is_some()),
            ..MemoryFeedbackCounts::default()
        };
        for task in &self.tasks {
            counts.partial_tasks += usize::from(task.partial);
            counts.skipped += usize::from(task.skipped.is_some());
            counts.pending +=
                usize::from(!task.discovered && task.skipped.is_none() && self.skipped.is_none());
            for entry in &task.records {
                counts.retracted += usize::from(entry.retracted);
                counts.skipped += usize::from(entry.dropped || entry.failed.is_some());
                counts.sent += usize::from(entry.feedback.is_some() && !entry.retracted);
                counts.pending += usize::from(
                    self.skipped.is_none()
                        && entry.failed.is_none()
                        && !entry.dropped
                        && !entry.retracted
                        && (entry.feedback.is_none() || entry.error.is_some()),
                );
            }
        }
        assert!(counts.retracted <= self.tasks.iter().map(|t| t.records.len()).sum());
        counts
    }
}

pub fn memory_feedback_key(
    id: &FeedbackId,
    memory: &AetherMemoryId,
    task: &AetherTaskId,
) -> String {
    let key = format!("archon:{id}:{memory}:{task}");
    assert!(
        key.len() <= 200,
        "Aether idempotency keys allow at most 200 bytes"
    );
    assert!(
        key.bytes().all(|b| b.is_ascii_graphic()),
        "printable ASCII without spaces"
    );
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_the_correction_memory_and_task() {
        let id = FeedbackId::new();
        let memory = AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let task = AetherTaskId::parse("tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let key = memory_feedback_key(&id, &memory, &task);
        assert_eq!(key, format!("archon:{id}:{memory}:{task}"));
        assert!(key.len() <= 200 && key.is_ascii());
    }

    fn pending_ledger() -> MemoryFeedback {
        MemoryFeedback {
            instance_id: "instance".into(),
            skipped: None,
            tasks: vec![MemoryFeedbackTask {
                task: AetherTaskId::parse("tsk_1").unwrap(),
                discovered: false,
                partial: false,
                skipped: None,
                error: None,
                records: Vec::new(),
            }],
        }
    }

    #[test]
    fn counts_cover_discovery_delivery_retraction_and_dropped_work() {
        let mut ledger = pending_ledger();
        assert_eq!(ledger.counts().pending, 1);
        assert_eq!(ledger.counts().sent, 0);
        let entry = MemoryFeedbackEntry {
            memory: AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            feedback: None,
            attempted: false,
            submission_horizon: None,
            unconfirmed: false,
            failed: None,
            retracted: false,
            dropped: false,
            error: None,
        };
        ledger.tasks[0].discovered = true;
        ledger.tasks[0].partial = true;
        ledger.tasks[0].records.push(entry);
        assert_eq!(ledger.counts().pending, 1);
        ledger.tasks[0].records[0].attempted = true;
        ledger.tasks[0].records[0].feedback = Some(AetherMemoryFeedbackId::parse("mfb_1").unwrap());
        assert_eq!(ledger.counts().sent, 1);
        assert_eq!(ledger.counts().pending, 0);
        ledger.tasks[0].records[0].error = Some("retraction failed".into());
        assert_eq!(ledger.counts().pending, 1);
        ledger.tasks[0].records[0].error = None;
        ledger.tasks[0].records[0].retracted = true;
        assert_eq!(ledger.counts().retracted, 1);
        assert_eq!(ledger.counts().sent, 0);
        assert_eq!(ledger.counts().partial_tasks, 1);
        ledger.tasks[0].records[0].retracted = false;
        ledger.tasks[0].records[0].feedback = None;
        ledger.tasks[0].records[0].attempted = false;
        ledger.tasks[0].records[0].dropped = true;
        assert_eq!(ledger.counts().skipped, 1);
        assert_eq!(ledger.counts().pending, 0);
    }

    #[test]
    fn duplicate_tasks_and_impossible_receipts_are_rejected() {
        let mut ledger = pending_ledger();
        ledger.tasks.push(ledger.tasks[0].clone());
        assert!(ledger.check().is_err());
        ledger.tasks.pop();
        ledger.tasks[0].records.push(MemoryFeedbackEntry {
            memory: AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            feedback: Some(AetherMemoryFeedbackId::parse("mfb_1").unwrap()),
            attempted: false,
            submission_horizon: None,
            unconfirmed: false,
            failed: None,
            retracted: false,
            dropped: false,
            error: None,
        });
        assert!(ledger.check().is_err(), "discovery must precede delivery");
        ledger.tasks[0].discovered = true;
        assert!(ledger.check().is_err(), "a receipt requires an attempt");
        ledger.tasks[0].records[0].attempted = true;
        assert!(ledger.check().is_ok());
        ledger.tasks[0].records[0].failed = Some("not found".into());
        assert_eq!(ledger.counts().pending, 0);
        assert_eq!(ledger.counts().skipped, 1);
        ledger.tasks[0].records[0].error = Some("retry".into());
        assert!(
            ledger.check().is_err(),
            "terminal entries have no retry error"
        );
        ledger.tasks[0].records[0].error = None;
        ledger.tasks[0].records[0].failed = None;
        ledger.tasks[0].records[0].dropped = true;
        assert!(ledger.check().is_err(), "attempted work cannot be dropped");
    }
}
