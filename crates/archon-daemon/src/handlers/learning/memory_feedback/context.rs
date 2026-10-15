//! Freeze task bindings and discover only the memory ids actually recalled.

use super::{bound_client, terminal};
use crate::context::DaemonContext;
use archon_aether::AetherClient;
use archon_aether::memory_feedback::{MEMORY_CONTEXT_FEATURE, MEMORY_FEEDBACK_FEATURE};
use archon_core::error::ArchonError;
use archon_core::feedback::{
    CorrectionRequest, MemoryFeedback, MemoryFeedbackEntry, MemoryFeedbackTask, OperatorFeedback,
};
use archon_queue::{Store, store::keys};
use std::collections::BTreeSet;

pub(in crate::handlers::learning) fn plan(
    ctx: &DaemonContext,
    store: &Store,
    request: &CorrectionRequest,
) -> Result<MemoryFeedback, ArchonError> {
    let client = match ctx.aether.client() {
        Ok(client) => Some(client),
        Err(ArchonError::Unavailable(_)) => None,
        Err(error) => return Err(error),
    };
    let instance_id = store.kv_get(keys::AETHER_INSTANCE)?.unwrap_or_default();
    let tasks: BTreeSet<_> = store
        .stage_runs(&request.run)?
        .into_iter()
        .filter(|s| s.stage_id == request.stage)
        .filter_map(|s| s.aether_task)
        .collect();
    let skipped = if request.cause != archon_core::outcome::CorrectionCause::Quality
        && request.forget_memory.is_none()
    {
        Some("only quality corrections provide run outcome evidence".into())
    } else if instance_id.is_empty() {
        Some("no Aether instance is known".into())
    } else if tasks.is_empty() {
        Some("the corrected stage has no recorded Aether tasks".into())
    } else if client.as_ref().is_some_and(|c| !supported(c)) {
        Some("Aether does not advertise memory_feedback and memory_context".into())
    } else {
        None
    };
    let ledger = MemoryFeedback {
        instance_id,
        skipped,
        tasks: tasks.into_iter().map(pending_task).collect(),
    };
    ledger.check()?;
    assert!(ledger.tasks.windows(2).all(|t| t[0].task < t[1].task));
    assert!(ledger.skipped.is_some() || !ledger.instance_id.is_empty());
    Ok(ledger)
}

pub(super) fn supported(client: &AetherClient) -> bool {
    client.supports_memory_feature(MEMORY_FEEDBACK_FEATURE)
        && client.supports_memory_feature(MEMORY_CONTEXT_FEATURE)
}

fn pending_task(task: archon_core::aether_ids::AetherTaskId) -> MemoryFeedbackTask {
    MemoryFeedbackTask {
        task,
        discovered: false,
        partial: false,
        skipped: None,
        error: None,
        records: Vec::new(),
    }
}

pub(super) async fn discover(
    ctx: &DaemonContext,
    feedback: &OperatorFeedback,
    ledger: &mut MemoryFeedback,
    index: usize,
) {
    assert!(index < ledger.tasks.len());
    assert!(!ledger.tasks[index].discovered);
    let task = &mut ledger.tasks[index];
    if feedback.revoked_at.is_some() {
        task.discovered = true;
        task.skipped = Some("correction undone before context lookup".into());
        task.error = None;
        return;
    }
    let result = async {
        let client = bound_client(ctx, &ledger.instance_id)?;
        client.memories_in_context(&task.task).await
    }
    .await;
    match result {
        Ok(context) => {
            apply_context(task, feedback, context);
        }

        Err(error) if terminal(&error) => {
            task.skipped = Some(error.to_string());
            task.error = None;
        }
        Err(error) => task.error = Some(error.to_string()),
    }
}

fn apply_context(
    task: &mut MemoryFeedbackTask,
    feedback: &OperatorFeedback,
    context: archon_aether::memory_feedback::MemoriesInContext,
) {
    assert!(!task.discovered);
    assert!(task.records.is_empty());
    let memories: BTreeSet<_> = context
        .snapshots
        .into_iter()
        .flat_map(|s| s.memories)
        .collect();
    let named = feedback.curation.as_ref().map(|c| &c.memory);
    task.records = memories
        .into_iter()
        .filter(|m| named.is_none_or(|n| n == m))
        .map(|memory| MemoryFeedbackEntry {
            memory,
            feedback: None,
            attempted: false,
            submission_horizon: None,
            unconfirmed: false,
            failed: None,
            retracted: false,
            dropped: false,
            error: None,
        })
        .collect();
    task.discovered = true;
    task.partial = context.truncated;
    task.error = None;
    if task.records.is_empty() {
        task.skipped = Some(
            if named.is_some() {
                "named memory was not listed in this task's context"
            } else {
                "no memories were listed in this task's context"
            }
            .into(),
        );
    }
}
