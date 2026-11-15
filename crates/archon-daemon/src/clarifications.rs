//! Durable stage questions, explicitly linked answers and replayable fallback delivery.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::agent_message::{Clarification, ClarificationOptions, ClarificationResolution};
use archon_core::autonomy::DirectiveSource;
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QueueItemId};
use archon_core::run::StageRun;
use archon_queue::Store;

use crate::context::DaemonContext;

const PREFIX: &str = "clarification:";

pub(crate) fn key(id: &NotificationId) -> String {
    format!("{PREFIX}{id}")
}

pub(crate) fn prepare(
    store: &Store,
    stage: &StageRun,
    question: &str,
    options: Option<ClarificationOptions>,
    minutes: u32,
) -> Result<Clarification, ArchonError> {
    archon_core::agent_message::check_question(question)?;
    assert!((1..=43_200).contains(&minutes), "validated timeout");
    let options = options.unwrap_or(ClarificationOptions {
        fallback:
            "Preserve existing behavior and make the smallest change consistent with the brief."
                .into(),
        proceed_directive: None,
    });
    archon_core::agent_message::check_question(&options.fallback)?;
    let now = archon_core::now();
    let resolution =
        proceed_reason(store, &options, now)?.map(|reason| ClarificationResolution::Assumed {
            choice: options.fallback.clone(),
            reason,
            at: now,
        });
    let agent = stage
        .aether_agent
        .clone()
        .ok_or_else(|| ArchonError::Corrupt(format!("stage {} has no agent", stage.id)))?;
    assert!(!stage.state.is_terminal(), "only live stages ask");
    Ok(Clarification {
        id: NotificationId::new(),
        item: store.run(&stage.run)?.item,
        stage_run: stage.id.clone(),
        agent,
        question: question.into(),
        fallback: options.fallback,
        asked_at: now,
        deadline: now + chrono::Duration::minutes(i64::from(minutes)),
        resolution,
    })
}

/// Check actual operator authorization, including expiration and revocation.
pub fn proceed_allowed(
    store: &Store,
    directive: Option<&archon_core::id::DirectiveId>,
    now: Timestamp,
) -> Result<bool, ArchonError> {
    Ok(authorized_directive(store, directive, now)?.is_some())
}

fn authorized_directive(
    store: &Store,
    directive: Option<&archon_core::id::DirectiveId>,
    now: Timestamp,
) -> Result<Option<archon_core::autonomy::Directive>, ArchonError> {
    let found = store.directives(now)?.into_iter().find(|d| {
        directive.is_none_or(|id| d.id == *id)
            && d.source == DirectiveSource::Operator
            && d.clarifications == archon_core::autonomy::ClarificationPolicy::Proceed
    });
    assert!(found.as_ref().is_none_or(|d| d.is_active(now)));
    assert!(
        found
            .as_ref()
            .is_none_or(|d| d.source == DirectiveSource::Operator)
    );
    Ok(found)
}

fn proceed_reason(
    store: &Store,
    options: &ClarificationOptions,
    now: Timestamp,
) -> Result<Option<String>, ArchonError> {
    Ok(
        authorized_directive(store, options.proceed_directive.as_ref(), now)?
            .map(|d| format!("Proceed under operator directive {}: {}", d.id, d.text)),
    )
}

pub(crate) fn records(
    store: &Store,
    item: Option<&QueueItemId>,
    open_only: bool,
) -> Result<Vec<Clarification>, ArchonError> {
    let mut records = Vec::new();
    for (stored_key, raw) in store.kv_prefix(PREFIX)? {
        let record: Clarification = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {stored_key}: {e}")))?;
        if stored_key != key(&record.id)
            || record.deadline <= record.asked_at
            || record.question.trim().is_empty()
            || record.fallback.trim().is_empty()
        {
            return Err(ArchonError::Corrupt(format!(
                "invalid clarification {stored_key}"
            )));
        }
        if item.is_some_and(|id| *id != record.item) || (open_only && record.resolution.is_some()) {
            continue;
        }
        records.push(record);
    }
    records.sort_by(|a, b| (a.asked_at, &a.id).cmp(&(b.asked_at, &b.id)));
    assert!(records.iter().all(|r| !open_only || r.resolution.is_none()));
    Ok(records)
}

/// Only a marked answer to this agent's question resolves it; ordinary notes do not.
pub(crate) fn answer(
    store: &mut Store,
    target: &AetherAgentId,
    text: &str,
) -> Result<(), ArchonError> {
    if !text.contains("[clarification:") {
        return Ok(());
    }
    for mut record in records(store, None, true)? {
        if record.agent != *target || !text.contains(&format!("[clarification:{}]", record.id)) {
            continue;
        }
        assert!(!text.trim().is_empty(), "a marked answer contains text");
        assert_eq!(&record.agent, target);
        assert!(record.resolution.is_none());
        record.resolution = Some(ClarificationResolution::Answered {
            answer: text.into(),
            at: archon_core::now(),
        });
        store.kv_set(&key(&record.id), &serde_json::to_string(&record)?)?;
    }
    Ok(())
}

/// Record choices before delivery; retry the same idempotent notification after failure.
pub async fn expire(ctx: &DaemonContext, now: Timestamp) -> Result<(), ArchonError> {
    let records = {
        let mut store = ctx.store.lock().await;
        let mut found = records(&store, None, false)?;
        let directive = authorized_directive(&store, None, now)?;
        for record in &mut found {
            if record.resolution.is_none() && (record.deadline <= now || directive.is_some()) {
                record.resolution = Some(ClarificationResolution::Assumed {
                    choice: record.fallback.clone(),
                    reason: directive.as_ref().map_or_else(
                        || "Clarification timeout elapsed".into(),
                        |d| format!("Proceed under operator directive {}: {}", d.id, d.text),
                    ),
                    at: now,
                });
                store.kv_set(&key(&record.id), &serde_json::to_string(record)?)?;
            }
        }
        found
    };
    for record in records {
        deliver_assumption(ctx, &record).await?;
    }
    Ok(())
}

async fn deliver_assumption(
    ctx: &DaemonContext,
    record: &Clarification,
) -> Result<(), ArchonError> {
    let Some(ClarificationResolution::Assumed { .. }) = &record.resolution else {
        return Ok(());
    };
    let delivered = format!("clarification_delivered:{}", record.id);
    {
        let store = ctx.store.lock().await;
        if store.kv_get(&delivered)?.is_some()
            || store.stage_run(&record.stage_run)?.state.is_terminal()
        {
            return Ok(());
        }
        assert_eq!(
            store.stage_run(&record.stage_run)?.aether_agent.as_ref(),
            Some(&record.agent)
        );
    }
    let text = format!(
        "{} Proceed now with this recorded assumption; include the choice in your final review.",
        record.describe()
    );
    assert!(record.resolution.is_some());
    ctx.backend
        .send_message(&record.agent, &text, &delivered)
        .await?;
    ctx.store.lock().await.kv_set(&delivered, "true")
}

/// The parent can render this beside its approved-intent deviations at final review.
pub(crate) fn review_section(store: &Store, item: &QueueItemId) -> Result<String, ArchonError> {
    let records = records(store, Some(item), false)?;
    let lines: Vec<_> = records.iter().map(Clarification::describe).collect();
    assert_eq!(lines.len(), records.len());
    assert!(records.iter().all(|r| r.item == *item));
    Ok(lines.join("\n"))
}

pub(crate) fn stage_section(store: &Store, item: &QueueItemId) -> Result<String, ArchonError> {
    let now = archon_core::now();
    let directives: Vec<_> = store
        .directives(now)?
        .into_iter()
        .filter(|d| d.source == DirectiveSource::Operator)
        .collect();
    assert!(
        directives
            .iter()
            .all(|d| d.source == DirectiveSource::Operator)
    );
    assert!(directives.iter().all(|d| d.is_active(now)));
    Ok(format!(
        "\n## Clarification context\n\nActive operator directives (proceed requires clarifications=proceed; cite the applicable id in clarification.proceed_directive): {}\n{}\n",
        serde_json::to_string(&directives)?,
        review_section(store, item)?
    ))
}
