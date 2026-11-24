//! Initiative requests share item admission and policy validation.

use archon_core::autonomy::MergeMode;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, QueueItemId};
use archon_core::initiative::{
    ContractChange, EnqueueInitiative, Initiative, InitiativeMember, InitiativeState,
};
use archon_core::model::{ModelPlanDraft, ModelReasons};
use archon_core::plan_origin::{Authority, PlannedPlan};
use archon_core::protocol::Reply;
use archon_core::queue::{QueueItem, QueueState, check_enqueue};

use super::items;
use crate::context::DaemonContext;
use crate::executor::initiative_ship;
use crate::knobs::{self, ItemRoute};
use crate::resolve::RepoEnv;

pub(super) async fn enqueue(
    ctx: &DaemonContext,
    request: EnqueueInitiative,
    by: Authority,
) -> Result<Reply, ArchonError> {
    request.check(&ctx.config.limits.initiatives())?;
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
        members.push(prepare_member(ctx, (&initiative, by), member, &members).await?);
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
    (initiative, by): (&Initiative, Authority),
    member: &InitiativeMember,
    earlier: &[QueueItem],
) -> Result<QueueItem, ArchonError> {
    let intent = items::resolve_intent(ctx, member.intent.as_ref(), member.onto.as_ref()).await?;
    let repo = ctx.store.lock().await.repo(&member.repo)?;
    let env = RepoEnv::new(Some(repo))?;
    let mut patch = member.autonomy;
    if initiative.atomic {
        if patch.merge == Some(MergeMode::Auto) {
            return Err(ArchonError::Invalid(
                "an atomic initiative's members are merged by hand, together".into(),
            ));
        }
        patch.merge = Some(MergeMode::Manual);
    }
    let mut resolved = knobs::resolve(ctx, &env, member.pipeline.clone(), &patch)?;
    items::check_runs_here(ctx, &env, &resolved.spec).await?;
    let onto = items::freeze_onto(
        ctx,
        &env,
        &resolved.spec,
        member.onto.as_ref(),
        !member.after.is_empty(),
    )
    .await?;
    let intent = items::resolve_pull_request_intent(ctx, intent.as_ref(), onto.as_ref()).await?;
    if intent.is_some()
        || matches!(
            resolved.autonomy.proposal,
            archon_core::autonomy::ProposalMode::Lavish | archon_core::autonomy::ProposalMode::Live
        )
    {
        resolved.spec = archon_pipeline::acceptance::augment(&resolved.spec)?;
    }
    let id = QueueItemId::new();
    let (plan, routed_provider) = route_member(ctx, (&env, by), &resolved, member, &id).await?;
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
        intent: intent.map(Box::new),
        onto: onto.map(Box::new),
        id,
        repo: env.repo.as_ref().map(|r| r.id.clone()),
        pipeline: resolved.pipeline,
        title: title.trim().into(),
        brief: member.brief.clone(),
        priority: Default::default(),
        dependencies,
        tags: Vec::new(),
        model_plan: plan,
        model_reasons: member.model_reasons.clone(),
        budget_usd: None,
        autonomy: resolved.autonomy,
        origin: None,
        stage_timeouts: Default::default(),
        supersedes: None,
        fresh_retries: 0,
        routed_provider,
        provider_switch: None,
        initiative: Some(initiative.id.clone()),
        risk: None,
        state: QueueState::Queued,
        created_at: initiative.created_at,
        updated_at: initiative.created_at,
    })
}

/// `draft` resolved, filled and checked as an enqueue's plan is: an effort above
/// its stage's default needs a reason in the member's `model_reasons`.
async fn route_member(
    ctx: &DaemonContext,
    (env, by): (&RepoEnv, Authority),
    resolved: &knobs::Resolved,
    member: &InitiativeMember,
    id: &QueueItemId,
) -> Result<(PlannedPlan, Option<String>), ArchonError> {
    let unrouted = ItemRoute { id, held: None };
    let routed_provider = items::routed_provider(ctx, env, unrouted).await?;
    let route = ItemRoute {
        held: routed_provider.as_deref(),
        ..unrouted
    };
    let draft = member.model_plan.clone().unwrap_or_default();
    let asked = (draft, member.model_reasons.as_ref());
    let plan = member_plan(ctx, (env, route), resolved, asked, by).await?;
    Ok((plan, routed_provider))
}

async fn member_plan(
    ctx: &DaemonContext,
    (env, route): (&RepoEnv, ItemRoute<'_>),
    resolved: &knobs::Resolved,
    (draft, reasons): (ModelPlanDraft, Option<&ModelReasons>),
    by: Authority,
) -> Result<PlannedPlan, ArchonError> {
    let spec = &resolved.spec;
    let given = items::resolve_draft(ctx, (env, route), spec, (draft, reasons), (by, None)).await?;
    let plan = items::filled_plan(ctx, route, env, &resolved.base, spec, given, None).await?;
    items::check_plan(ctx, env, spec, &plan)?;
    items::check_reasons(spec, reasons)?;
    Ok(plan)
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
    change: &ContractChange,
) -> Result<Reply, ArchonError> {
    let limits = ctx.config.limits.initiatives();
    let updated = ctx
        .store
        .lock()
        .await
        .change_initiative_contract(id, change, &limits)?;
    assert_eq!(&updated.initiative.id, id);
    assert!(updated.initiative.contract.ends_with(change.text().trim()));
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
