//! Initiative requests share item admission and policy validation.

use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, QueueItemId};
use archon_core::initiative::{EnqueueInitiative, Initiative, InitiativeMember, InitiativeState};
use archon_core::protocol::Reply;
use archon_core::queue::{QueueItem, QueueState, check_enqueue};

use super::items;
use crate::context::DaemonContext;
use crate::executor::initiative_ship;
use crate::knobs;
use crate::resolve::RepoEnv;

pub(super) async fn enqueue(
    ctx: &DaemonContext,
    request: EnqueueInitiative,
) -> Result<Reply, ArchonError> {
    request.check()?;
    if let Some(existing) = ctx.store.lock().await.replay_initiative(&request)? {
        return Ok(Reply::Initiative(Box::new(existing)));
    }
    let initiative = Initiative::new(
        &request.title,
        &request.contract,
        request.atomic,
        archon_core::now(),
    )?;
    let mut members = Vec::with_capacity(request.members.len());
    for member in &request.members {
        members.push(prepare_member(ctx, &initiative, member, &members).await?);
    }
    assert_eq!(members.len(), request.members.len());
    assert!(
        members
            .iter()
            .all(|m| m.initiative.as_ref() == Some(&initiative.id))
    );
    let created = ctx
        .store
        .lock()
        .await
        .insert_initiative(&initiative, &members, &request)?;
    ctx.dispatch.notify_one();
    Ok(Reply::Initiative(Box::new(created)))
}

async fn prepare_member(
    ctx: &DaemonContext,
    initiative: &Initiative,
    member: &InitiativeMember,
    earlier: &[QueueItem],
) -> Result<QueueItem, ArchonError> {
    let repo = ctx.store.lock().await.repo(&member.repo)?;
    let env = RepoEnv::new(Some(repo))?;
    let patch = AutonomyPatch {
        merge: initiative.atomic.then_some(MergeMode::Manual),
        ..Default::default()
    };
    let resolved = knobs::resolve(&ctx.config, &env, member.pipeline.clone(), &patch)?;
    items::check_runs_here(ctx, &env, &resolved.spec).await?;
    let plan = member_plan(ctx, &env, &resolved, member).await?;
    items::check_plan(ctx, &env, &resolved.spec, &plan)?;
    let title = member
        .title
        .clone()
        .unwrap_or_else(|| member_title(&member.brief));
    check_enqueue(&title, &member.brief, &[], None)?;
    assert!(member.after.iter().all(|&i| i < earlier.len()));
    let dependencies = member
        .after
        .iter()
        .map(|&i| earlier[i].id.clone())
        .collect();
    assert!(!title.trim().is_empty());
    Ok(QueueItem {
        id: QueueItemId::new(),
        repo: env.repo.as_ref().map(|r| r.id.clone()),
        pipeline: resolved.pipeline,
        title: title.trim().into(),
        brief: member.brief.clone(),
        priority: Default::default(),
        dependencies,
        tags: Vec::new(),
        model_plan: plan,
        model_reasons: None,
        budget_usd: None,
        autonomy: resolved.autonomy,
        origin: None,
        fresh_retries: 0,
        initiative: Some(initiative.id.clone()),
        state: QueueState::Queued,
        created_at: initiative.created_at,
        updated_at: initiative.created_at,
    })
}

pub(super) async fn get(ctx: &DaemonContext, id: &InitiativeId) -> Result<Reply, ArchonError> {
    Ok(Reply::Initiative(Box::new(
        ctx.store.lock().await.initiative(id)?,
    )))
}

pub(super) async fn list(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    Ok(Reply::Initiatives(ctx.store.lock().await.initiatives()?))
}

pub(super) async fn update(
    ctx: &DaemonContext,
    id: &InitiativeId,
    append: &str,
) -> Result<Reply, ArchonError> {
    let updated = ctx
        .store
        .lock()
        .await
        .append_initiative_contract(id, append)?;
    assert_eq!(&updated.initiative.id, id);
    assert!(updated.initiative.contract.ends_with(append.trim()));
    ctx.coordination.changed.notify_one();
    initiative_ship::deliver_contract(ctx, &updated).await?;
    Ok(Reply::Initiative(Box::new(updated)))
}

pub(super) async fn cancel(ctx: &DaemonContext, id: &InitiativeId) -> Result<Reply, ArchonError> {
    ctx.store
        .lock()
        .await
        .set_initiative_state(id, InitiativeState::Cancelled)?;
    initiative_ship::cancel_members(ctx, id).await?;
    initiative_ship::announce(ctx, id).await?;
    ctx.dispatch.notify_one();
    get(ctx, id).await
}

pub(super) async fn retry(ctx: &DaemonContext, id: &InitiativeId) -> Result<Reply, ArchonError> {
    initiative_ship::announce(ctx, id).await?;
    let _table = ctx.runs.lock().await;
    let retried = ctx.store.lock().await.retry_initiative(id)?;
    assert_eq!(retried.initiative.state, InitiativeState::Open);
    assert!(
        retried
            .members
            .iter()
            .any(|m| m.state == QueueState::Queued)
    );
    ctx.dispatch.notify_one();
    Ok(Reply::Initiative(Box::new(retried)))
}

fn member_title(brief: &str) -> String {
    brief
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(180)
        .collect()
}

async fn member_plan(
    ctx: &DaemonContext,
    env: &RepoEnv,
    resolved: &knobs::Resolved,
    member: &InitiativeMember,
) -> Result<archon_core::model::ModelPlan, ArchonError> {
    use archon_core::model::ModelChoice;
    let mut plan = items::filled_plan(
        ctx,
        env,
        &resolved.base,
        &resolved.spec,
        member.model_plan.clone().unwrap_or_default(),
    )
    .await?;
    let stages = &resolved.spec.file.stages;
    if stages.iter().all(|s| plan.choices_for(&s.id).is_some()) {
        return Ok(plan);
    }
    let policy = env.policy(&ctx.config);
    let configured = &ctx.config.orchestrator.model;
    let model = if policy.permits_model(configured) {
        configured.clone()
    } else {
        policy
            .allow
            .as_ref()
            .and_then(|allowed| allowed.iter().find(|m| policy.permits_model(m)))
            .cloned()
            .ok_or_else(|| {
                ArchonError::Policy(format!(
                    "initiative needs a model_plan allowed by {}",
                    policy.describe()
                ))
            })?
    };
    let effort = policy
        .max_effort
        .map_or(ctx.config.orchestrator.effort, |cap| {
            cap.min(ctx.config.orchestrator.effort)
        });
    let choice = ModelChoice { model, effort };
    policy.check(&choice)?;
    assert!(!stages.is_empty());
    for stage in stages {
        plan.0
            .entry(stage.id.clone())
            .or_insert_with(|| vec![choice.clone()]);
    }
    assert!(stages.iter().all(|s| plan.choices_for(&s.id).is_some()));
    Ok(plan)
}
