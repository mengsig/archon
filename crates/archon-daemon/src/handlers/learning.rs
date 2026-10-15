//! `archon learning`: an operator's correction of one stage of one finished
//! run, and the curation of the one Aether memory it names. The correction
//! is recorded first; Aether is called without the store lock.

pub(super) mod memory_feedback;

use std::collections::BTreeMap;
use std::sync::Arc;

use archon_aether::AetherClient;
use archon_aether::memory::MemoryRecord;
use archon_core::aether_ids::AetherMemoryId;
use archon_core::error::ArchonError;
use archon_core::feedback::{
    CorrectionRequest, CurationState, FeedbackReceipt, FeedbackView, LearningWhy, MemoryBefore,
    MemoryCuration, MemoryView, OperatorFeedback,
};
use archon_core::id::{FeedbackId, QueueItemId, RepoId};
use archon_core::model_tier::Tier;
use archon_core::policy::ModelPolicy;
use archon_core::protocol::Reply;
use archon_queue::store::{CorrectionFacts, RouteQuery, keys};

use crate::context::DaemonContext;
use crate::resolve::RepoEnv;

pub(super) async fn correct(
    ctx: &DaemonContext,
    request: CorrectionRequest,
) -> Result<Reply, ArchonError> {
    request.check()?;
    let _curation = if request.forget_memory.is_some() {
        Some(ctx.memory_curation.lock().await)
    } else {
        None
    };
    let aether_instance = match &request.forget_memory {
        Some(memory) => Some(memory_instance(ctx, memory).await?),
        None => None,
    };
    let facts = correction_facts(ctx, &request, aether_instance).await?;
    let (feedback, replayed) = ctx
        .store
        .lock()
        .await
        .record_operator_correction(&request, &facts)?;
    assert_eq!(feedback.run, request.run, "recorded for the named run");
    let bound = feedback.curation.as_ref().map(|c| &c.instance_id);
    assert!(
        replayed || bound == facts.aether_instance.as_ref(),
        "a new forget is bound before Aether is called"
    );
    ctx.memory_feedback_ready.notify_one();
    let feedback = match feedback.curation.as_ref().map(|c| c.state) {
        Some(CurationState::Pending | CurationState::Failed) => forget(ctx, feedback).await?,
        _ => feedback,
    };
    receipt(ctx, feedback, replayed).await
}

pub(super) async fn why(ctx: &DaemonContext, item: &QueueItemId) -> Result<Reply, ArchonError> {
    let feedback = {
        let store = ctx.store.lock().await;
        store.item(item)?;
        store.item_feedback(item)?
    };
    let mut views = Vec::with_capacity(feedback.len());
    for event in feedback {
        views.push(view(ctx, event).await?);
    }
    assert!(views.iter().all(|v| &v.feedback.item == item));
    Ok(Reply::LearningWhy(Box::new(LearningWhy {
        item: item.clone(),
        feedback: views,
    })))
}

/// Revokes the correction's model penalty; its memory curation stays as it is.
pub(super) async fn undo(ctx: &DaemonContext, id: &FeedbackId) -> Result<Reply, ArchonError> {
    let (feedback, replayed) = {
        let store = ctx.store.lock().await;
        if !store.learning_config()?.enabled {
            return Err(ArchonError::Invalid(
                "learning is disabled, so no correction is undone; enable [learning] first".into(),
            ));
        }
        let before = store.operator_feedback(id)?;
        let after = store.revoke_feedback(id, archon_core::now())?;
        assert!(
            after.revoked_at.is_some(),
            "an undone correction is revoked"
        );
        assert_eq!(
            before.curation, after.curation,
            "undo leaves the memory alone"
        );
        (after, before.revoked_at.is_some())
    };
    ctx.memory_feedback_ready.notify_one();
    receipt(ctx, feedback, replayed).await
}

/// Resumes pending memory evidence, retractions, and requested forgets.
pub(super) async fn retry_memory(
    ctx: &DaemonContext,
    id: &FeedbackId,
) -> Result<Reply, ArchonError> {
    let _curation = ctx.memory_curation.lock().await;
    let _delivery = ctx.learning_memory.lock().await;
    let feedback = ctx.store.lock().await.operator_feedback(id)?;
    assert_eq!(&feedback.id, id, "retry the requested correction");
    let revoked = feedback.revoked_at;
    let feedback = memory_feedback::deliver(ctx, feedback).await?;
    assert_eq!(
        feedback.revoked_at, revoked,
        "delivery never changes the model penalty"
    );
    let feedback = match feedback.curation.as_ref().map(|c| c.state) {
        Some(CurationState::Pending | CurationState::Failed) => forget(ctx, feedback).await?,
        Some(CurationState::Restored) => {
            return Err(ArchonError::Invalid(format!(
                "memory was restored through {id}; forget it again in Aether's /memory"
            )));
        }
        _ => feedback,
    };
    receipt(ctx, feedback, false).await
}

/// Restores the memory this correction forgot, unless something else
/// changed it since or restoring would not bring it back.
pub(super) async fn restore_memory(
    ctx: &DaemonContext,
    id: &FeedbackId,
) -> Result<Reply, ArchonError> {
    let feedback = ctx.store.lock().await.operator_feedback(id)?;
    let curation = restorable(&feedback)?.clone();
    let client = bound_client(ctx, &curation)?;
    let record = client
        .memory(&curation.memory)
        .await?
        .ok_or_else(|| missing(&client, &curation))?;
    still_ours(id, &curation, &record)?;
    let restored = client.restore_memory(&curation.memory).await?;
    assert!(restored.forgotten_at.is_none(), "checked by the adapter");
    let now = archon_core::now();
    let next = MemoryCuration {
        state: CurationState::Restored,
        restored_at: Some(now),
        updated_at: now,
        ..curation
    };
    let feedback = ctx.store.lock().await.update_curation(id, &next)?;
    receipt(ctx, feedback, false).await
}

fn restorable(feedback: &OperatorFeedback) -> Result<&MemoryCuration, ArchonError> {
    let id = &feedback.id;
    let Some(curation) = &feedback.curation else {
        return Err(ArchonError::Invalid(format!(
            "feedback {id} forgot no memory"
        )));
    };
    if curation.state != CurationState::Applied {
        return Err(ArchonError::Invalid(format!(
            "feedback {id} has not forgotten memory {}: its forget is {}",
            curation.memory,
            curation.state.as_str()
        )));
    }
    if curation.before != Some(MemoryBefore::Current) {
        return Err(ArchonError::Invalid(format!(
            "memory {} was already forgotten or superseded before {id}; restore it in \
             Aether's /memory if it should return",
            curation.memory
        )));
    }
    assert!(
        curation.forgotten_at.is_some(),
        "an applied forget is dated"
    );
    Ok(curation)
}

/// Refuses a restore that would undo someone else's curation or have no effect.
fn still_ours(
    id: &FeedbackId,
    curation: &MemoryCuration,
    record: &MemoryRecord,
) -> Result<(), ArchonError> {
    assert_eq!(record.id, curation.memory, "read by its id");
    let memory = &curation.memory;
    if let Some(newer) = &record.superseded_by {
        return Err(ArchonError::Invalid(format!(
            "memory {memory} is superseded by {newer}; restoring it would not bring it back"
        )));
    }
    match record.forgotten_at {
        None => Err(ArchonError::Invalid(format!(
            "memory {memory} is no longer forgotten: it was restored after {id} forgot it"
        ))),
        Some(at) if Some(at) != curation.forgotten_at => Err(ArchonError::Invalid(format!(
            "memory {memory} was forgotten again at {at}, after {id}; restore it in Aether's \
             /memory if it should return"
        ))),
        Some(_) => Ok(()),
    }
}

/// Forgets the correction's memory in its own Aether instance and records
/// Aether's answer; a failure is kept on the receipt, not returned.
async fn forget(
    ctx: &DaemonContext,
    feedback: OperatorFeedback,
) -> Result<OperatorFeedback, ArchonError> {
    let curation = feedback.curation.clone().expect("a forget was asked for");
    assert!(matches!(
        curation.state,
        CurationState::Pending | CurationState::Failed
    ));
    let reason = format!("archon {}: {}", feedback.id, feedback.reason);
    let mut next = curation;
    let result = match bound_client(ctx, &next) {
        Ok(client) => forget_in(&client, &mut next, &reason).await,
        Err(error) => Err(error),
    };
    next.updated_at = archon_core::now();
    match result {
        Ok(()) => {
            next.state = CurationState::Applied;
            next.error = None;
        }
        Err(error) => {
            next.state = CurationState::Failed;
            next.error = Some(error.to_string());
            next.forgotten_at = None;
        }
    }
    ctx.store.lock().await.update_curation(&feedback.id, &next)
}

/// Reads the memory's state before the first forget, then forgets it. A
/// repeated forget converges: Aether answers a forgotten record unchanged.
async fn forget_in(
    client: &AetherClient,
    next: &mut MemoryCuration,
    reason: &str,
) -> Result<(), ArchonError> {
    if next.before.is_none() {
        let record = client
            .memory(&next.memory)
            .await?
            .ok_or_else(|| missing(client, next))?;
        next.before = Some(match (&record.superseded_by, record.forgotten_at) {
            (Some(_), _) => MemoryBefore::Superseded,
            (None, Some(_)) => MemoryBefore::Forgotten,
            (None, None) => MemoryBefore::Current,
        });
    }
    let record = client.forget_memory(&next.memory, reason).await?;
    assert_eq!(record.id, next.memory, "checked by the adapter");
    next.forgotten_at = record.forgotten_at;
    assert!(next.forgotten_at.is_some(), "checked by the adapter");
    Ok(())
}

fn missing(client: &AetherClient, curation: &MemoryCuration) -> ArchonError {
    ArchonError::NotFound(format!(
        "Aether instance {} has no memory {}",
        client.welcome().instance_id,
        curation.memory
    ))
}

/// The connected Aether, when it is the instance the forget is bound to and
/// speaks the memory commands.
fn bound_client(
    ctx: &DaemonContext,
    curation: &MemoryCuration,
) -> Result<Arc<AetherClient>, ArchonError> {
    assert!(!curation.instance_id.is_empty(), "checked when recorded");
    let client = ctx.aether.client()?;
    let connected = &client.welcome().instance_id;
    let bound = &curation.instance_id;
    if bound != connected {
        return Err(ArchonError::Invalid(format!(
            "memory {} is curated in Aether instance {bound}, but archond is connected to \
             {connected}; nothing was sent",
            curation.memory
        )));
    }
    client.check_memory_protocol()?;
    Ok(client)
}

/// The instance a requested forget binds to: the one archond last connected
/// to, which its pump records before using a link. Without one, or with a
/// connected instance that cannot curate memories, nothing is recorded.
async fn memory_instance(
    ctx: &DaemonContext,
    memory: &AetherMemoryId,
) -> Result<String, ArchonError> {
    let known = ctx.store.lock().await.kv_get(keys::AETHER_INSTANCE)?;
    let Some(instance) = known.filter(|id| !id.is_empty()) else {
        return Err(ArchonError::Unavailable(format!(
            "archond has never connected to Aether, so no Aether instance is known to hold \
             memory {memory}; nothing was recorded"
        )));
    };
    let client = match ctx.aether.client() {
        Ok(client) => client,
        Err(ArchonError::Unavailable(_)) => return Ok(instance),
        Err(error) => return Err(error),
    };
    let connected = &client.welcome().instance_id;
    if connected != &instance {
        return Err(ArchonError::Unavailable(format!(
            "archond is reconnecting from Aether instance {instance} to {connected}; nothing \
             was recorded, try again"
        )));
    }
    client.check_memory_protocol()?;
    Ok(instance)
}

/// The facts the store cannot know: the delivered commit, and the Aether
/// instance a requested forget is bound to.
async fn correction_facts(
    ctx: &DaemonContext,
    request: &CorrectionRequest,
    aether_instance: Option<String>,
) -> Result<CorrectionFacts, ArchonError> {
    let (delivery_commit, memory_feedback) = {
        let store = ctx.store.lock().await;
        (
            crate::executor::shipped_commit(&store, &request.run)?,
            Some(memory_feedback::plan(ctx, &store, request)?),
        )
    };
    Ok(CorrectionFacts {
        delivery_commit,
        memory_feedback,
        aether_instance,
        observed_at: archon_core::now(),
    })
}

async fn receipt(
    ctx: &DaemonContext,
    feedback: OperatorFeedback,
    replayed: bool,
) -> Result<Reply, ArchonError> {
    let view = view(ctx, feedback).await?;
    Ok(Reply::Feedback(Box::new(FeedbackReceipt {
        view,
        replayed,
    })))
}

/// What a correction does now: the standing of its choice and of the
/// cohort's default, the item's frozen explanation, and the memory's state.
async fn view(
    ctx: &DaemonContext,
    feedback: OperatorFeedback,
) -> Result<FeedbackView, ArchonError> {
    let (memory, memory_error) = match &feedback.curation {
        Some(curation) => memory_state(ctx, curation).await,
        None => (None, None),
    };
    let attribution = &feedback.attribution;
    let (policy, max_tier) = cohort_policy(ctx, attribution.repo.as_ref()).await?;
    let as_of = archon_core::now();
    let query = RouteQuery {
        repo: attribution.repo.as_ref(),
        pipeline: &attribution.pipeline,
        stage: &feedback.stage,
        size: None,
        context: attribution.context.as_ref(),
    };
    let corrected = feedback.penalized().map(|(_, choice)| choice);
    let (corrected, selected, frozen_explanation) = {
        let store = ctx.store.lock().await;
        let (corrected, selected) =
            crate::learned::standings(&store, &query, (&policy, max_tier), corrected, as_of)?;
        let enabled = store.learning_config()?.enabled;
        let frozen = frozen_explanation(&store, &feedback)?;
        (corrected, selected.filter(|_| enabled), frozen)
    };
    Ok(FeedbackView {
        memory_feedback: feedback
            .memory_feedback
            .as_ref()
            .map(|l| l.counts())
            .unwrap_or_default(),
        penalizing: feedback.penalizing(as_of),
        feedback,
        corrected,
        selected,
        frozen_explanation,
        memory,
        memory_error,
    })
}

/// The model policy of the correction's repository; the global one once
/// that repository is no longer registered.
async fn cohort_policy(
    ctx: &DaemonContext,
    repo: Option<&RepoId>,
) -> Result<(ModelPolicy, Tier), ArchonError> {
    match RepoEnv::load(ctx, repo).await {
        Ok(env) => Ok((env.policy(&ctx.config), env.max_default_tier(&ctx.config))),
        Err(ArchonError::NotFound(_)) => {
            let global = &ctx.config.models;
            Ok((
                global.policy(),
                global.max_default_tier(&Default::default()),
            ))
        }
        Err(error) => Err(error),
    }
}

fn frozen_explanation(
    store: &archon_queue::Store,
    feedback: &OperatorFeedback,
) -> Result<Option<String>, ArchonError> {
    let key = format!("learning/routing/{}", feedback.item);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let mut reasons: BTreeMap<String, String> =
        serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
    Ok(reasons.remove(&feedback.stage))
}

/// The memory's current record, or why it could not be read: the reply
/// still shows the recorded correction.
async fn memory_state(
    ctx: &DaemonContext,
    curation: &MemoryCuration,
) -> (Option<MemoryView>, Option<String>) {
    let read = async {
        let client = bound_client(ctx, curation)?;
        client
            .memory(&curation.memory)
            .await?
            .ok_or_else(|| missing(&client, curation))
    };
    match read.await {
        Ok(record) => (Some(record.view()), None),
        Err(error) => (None, Some(error.to_string())),
    }
}
