//! Persist intent before every Aether write, and recover lost receipts on undo.

use std::collections::BTreeSet;
use std::sync::Arc;

use archon_aether::AetherClient;
use archon_aether::memory_feedback::{FeedbackResolution, MemoryEvidence, RecordMemoryFeedback};
use archon_core::error::ArchonError;
use archon_core::feedback::{
    MemoryFeedback, MemoryFeedbackEntry, OperatorFeedback, memory_feedback_key,
};

use crate::context::DaemonContext;

mod context;
pub(super) use context::plan;
use context::{discover, supported};

pub(super) async fn deliver(
    ctx: &DaemonContext,
    feedback: OperatorFeedback,
) -> Result<OperatorFeedback, ArchonError> {
    let Some(mut ledger) = feedback.memory_feedback.clone() else {
        return Ok(feedback);
    };
    if ledger.skipped.is_some() {
        return Ok(feedback);
    }
    assert!(ledger.check().is_ok());
    assert!(!ledger.instance_id.is_empty());
    for index in 0..ledger.tasks.len() {
        if ledger.tasks[index].skipped.is_some() {
            continue;
        }
        if !ledger.tasks[index].discovered {
            let latest = ctx.store.lock().await.operator_feedback(&feedback.id)?;
            discover(ctx, &latest, &mut ledger, index).await;
            save(ctx, &feedback, &ledger).await?;
        }
        for entry in 0..ledger.tasks[index].records.len() {
            let latest = ctx.store.lock().await.operator_feedback(&feedback.id)?;
            deliver_entry(ctx, &latest, &mut ledger, index, entry).await?;
        }
    }
    save(ctx, &feedback, &ledger).await
}

async fn deliver_entry(
    ctx: &DaemonContext,
    feedback: &OperatorFeedback,
    ledger: &mut MemoryFeedback,
    task_index: usize,
    entry_index: usize,
) -> Result<(), ArchonError> {
    assert!(ledger.tasks[task_index].discovered);
    assert!(entry_index < ledger.tasks[task_index].records.len());
    let revoked = feedback.revoked_at.is_some();
    let entry = &mut ledger.tasks[task_index].records[entry_index];
    if entry.failed.is_some()
        || entry.dropped
        || entry.retracted
        || (!revoked && entry.feedback.is_some())
    {
        return Ok(());
    }
    if revoked && !entry.attempted {
        entry.dropped = true;
        entry.error = None;
        save(ctx, feedback, ledger).await?;
        return Ok(());
    }
    let client = match bound_client(ctx, &ledger.instance_id) {
        Ok(client) => client,
        Err(error) => {
            record_error(entry, error);
            save(ctx, feedback, ledger).await?;
            return Ok(());
        }
    };
    let recovering = entry.attempted && entry.feedback.is_none();
    if !entry.attempted {
        entry.submission_horizon = client.welcome().submission_horizon.clone();
    }
    entry.attempted = true;
    save(ctx, feedback, ledger).await?;
    let task_state = &mut ledger.tasks[task_index];
    let task = &task_state.task;
    let entry = &mut task_state.records[entry_index];
    entry.error = None;
    if let Err(error) = send(&client, feedback, task, entry, recovering).await {
        record_error(entry, error);
    }
    save(ctx, feedback, ledger).await?;
    Ok(())
}

async fn send(
    client: &AetherClient,
    feedback: &OperatorFeedback,
    task: &archon_core::aether_ids::AetherTaskId,
    entry: &mut MemoryFeedbackEntry,
    recovering: bool,
) -> Result<(), ArchonError> {
    assert!(entry.attempted && !entry.dropped);
    assert!(!entry.retracted);
    if entry.feedback.is_none() {
        let request = submission(feedback, task, entry);
        if recovering {
            match client
                .resolve_memory_feedback(request.clone(), entry.submission_horizon.clone())
                .await?
            {
                FeedbackResolution::Executed(record) => entry.feedback = Some(record.id),
                FeedbackResolution::AbsentSafe if !entry.unconfirmed => {}
                FeedbackResolution::AbsentSafe => {
                    entry.error = Some(
                        "delivery unconfirmed by an earlier resolution; nothing resent".into(),
                    );
                    return Ok(());
                }
                FeedbackResolution::Unknown(reason) => {
                    entry.unconfirmed = true;
                    entry.error = Some(format!("delivery unconfirmed: {reason}; nothing resent"));
                    return Ok(());
                }
            }
        }
        if entry.feedback.is_none() {
            entry.feedback = Some(client.record_memory_feedback(request).await?.id);
        }
    }
    if feedback.revoked_at.is_some() {
        client
            .retract_memory_feedback(
                entry.feedback.as_ref().expect("receipt recovered"),
                &format!("archon {}: operator correction undone", feedback.id),
            )
            .await?;
        entry.retracted = true;
    }
    Ok(())
}

fn bound_client(ctx: &DaemonContext, instance: &str) -> Result<Arc<AetherClient>, ArchonError> {
    let client = ctx.aether.client()?;
    if client.welcome().instance_id != instance {
        return Err(ArchonError::Invalid(format!(
            "memory feedback belongs to Aether instance {instance}; connected to {}",
            client.welcome().instance_id
        )));
    }
    if !supported(&client) {
        return Err(ArchonError::Invalid(
            "Aether does not advertise memory_feedback and memory_context".into(),
        ));
    }
    assert!(!instance.is_empty(), "delivery is bound to an instance");
    assert_eq!(client.welcome().instance_id, instance);
    Ok(client)
}

async fn save(
    ctx: &DaemonContext,
    feedback: &OperatorFeedback,
    ledger: &MemoryFeedback,
) -> Result<OperatorFeedback, ArchonError> {
    ctx.store
        .lock()
        .await
        .update_memory_feedback(&feedback.id, ledger)
}

fn terminal(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Invalid(_) | ArchonError::NotFound(_) | ArchonError::InvalidTransition(_)
    )
}

fn record_error(entry: &mut MemoryFeedbackEntry, error: ArchonError) {
    assert!(!entry.dropped && !entry.retracted);
    assert!(entry.failed.is_none());
    if terminal(&error) {
        entry.failed = Some(error.to_string());
        entry.error = None;
    } else {
        entry.error = Some(error.to_string());
    }
}

fn submission(
    feedback: &OperatorFeedback,
    task: &archon_core::aether_ids::AetherTaskId,
    entry: &MemoryFeedbackEntry,
) -> RecordMemoryFeedback {
    assert!(entry.attempted);
    assert!(entry.feedback.is_none());
    RecordMemoryFeedback {
        memory: entry.memory.clone(),
        verdict: "hurt".into(),
        evidence: if feedback.curation.is_some() {
            MemoryEvidence::Operator
        } else {
            MemoryEvidence::Outcome
        },
        task: task.clone(),
        reason: format!("archon {}: {}", feedback.id, feedback.reason),
        idempotency_key: memory_feedback_key(&feedback.id, &entry.memory, task),
    }
}

/// Delivery runs separately from requests; store failures stop the supervised worker.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        resume(&ctx).await?;
        ctx.memory_feedback_ready.notified().await;
    }
}

pub async fn resume(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let _delivery = ctx.learning_memory.lock().await;
    let ids = ctx.store.lock().await.pending_memory_feedback()?;
    assert!(ids.iter().collect::<BTreeSet<_>>().len() == ids.len());
    for id in ids {
        let feedback = ctx.store.lock().await.operator_feedback(&id)?;
        assert!(feedback.memory_feedback.is_some());
        deliver(ctx, feedback).await?;
    }
    Ok(())
}
