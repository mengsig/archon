use archon_aether::wire::is_internal;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model::{EffortContract, ModelChoice, ModelRef};
use archon_core::model_tier::ModelCatalog;
use archon_core::ports::{AgentUpdate, Descendant};
use archon_core::quota::QuotaView;
use archon_core::run::StageRun;
use archon_pipeline::contract::provider_reports_cost;

use super::{Replacement, Wait, bounded, save_wait};
use crate::context::DaemonContext;
use crate::executor::resume;
use crate::executor::{StoredSpawn, issuing_spawn};
use crate::resolve::RepoEnv;

pub(super) async fn choose(
    ctx: &DaemonContext,
    stage: &StageRun,
    reviewer: &Descendant,
    quota: Option<&QuotaView>,
    wait: &Wait,
) -> Result<Option<ModelChoice>, ArchonError> {
    if stage.aether_agent.as_ref() != Some(&reviewer.parent) {
        return Ok(None);
    }
    assert_ne!(reviewer.agent, reviewer.parent);
    assert_eq!(reviewer.role, "reviewer");
    let Some(quota) = quota else {
        return Ok(None);
    };
    let Some((spawn, repo)) = issued_plan(ctx, stage).await? else {
        return Ok(None);
    };
    let choices = named_choices(&spawn, reviewer);
    let has_quota = |model: &ModelRef| {
        model.provider != wait.source.provider
            && quota.get(&model.provider).is_some_and(|h| !h.exhausted())
    };
    if !choices.iter().any(|choice| has_quota(&choice.model)) {
        return Ok(None);
    }
    let catalog = bounded(2, crate::learned::catalog(ctx)).await?;
    if let Some(reason) = catalog.not_ready_reason() {
        return Err(ArchonError::Unavailable(reason.to_owned()));
    }
    let env = RepoEnv::load(ctx, repo.as_ref()).await?;
    let policy = env.policy_in(&ctx.config, &catalog.aliases());
    let lead_model = catalog.canonical_alias(&stage.current_choice().model);
    let governed = spawn.request.cost_budget_usd.is_some() || reviewer.cost_budget_usd.is_some();
    // If the update outcome is unknown, only that exact keyed attempt may run.
    let unanswered = wait
        .replacement
        .as_ref()
        .filter(|pending| pending.from == reviewer.model);
    let eligible = choices.into_iter().filter_map(|mut choice| {
        choice.model = connected(&catalog, &choice.model)?;
        (choice.model != *lead_model
            && (choice.model != reviewer.model || Some(choice.effort) == reviewer.effort)
            && has_quota(&choice.model)
            && !wait.refused.contains(&choice.model)
            && unanswered.is_none_or(|pending| choice == pending.to)
            && (!governed || provider_reports_cost(&choice.model.provider))
            && policy.check(&choice).is_ok())
        .then_some(choice)
    });
    Ok(eligible.min_by_key(|choice| {
        (
            choice.model != reviewer.model,
            ctx.config.review.prefer_other_provider && choice.model.provider == lead_model.provider,
        )
    }))
}

async fn issued_plan(
    ctx: &DaemonContext,
    stage: &StageRun,
) -> Result<Option<(StoredSpawn, Option<RepoId>)>, ArchonError> {
    let store = ctx.store.lock().await;
    let Some(spawn) = issuing_spawn(&store, &stage.id)? else {
        return Ok(None);
    };
    let run = store.run(&stage.run)?;
    Ok(Some((spawn, store.item(&run.item)?.repo)))
}

pub(super) async fn apply(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &mut Wait,
    to: ModelChoice,
) -> Result<(), ArchonError> {
    assert_ne!(to.model, reviewer.model);
    assert_ne!(to.model.provider, wait.source.provider);
    reserve_attempt(ctx, reviewer, key, wait, to).await?;
    let switch = wait
        .replacement
        .as_ref()
        .expect("the replacement was saved");
    assert_eq!(
        switch.from, reviewer.model,
        "the source reviewer still holds the old model"
    );
    let update = AgentUpdate {
        choice: Some(switch.to.clone()),
        ..AgentUpdate::default()
    };
    let update_key = format!("{key}:switch:{}:{}", wait.since.to_rfc3339(), wait.attempt);
    match bounded(
        1,
        ctx.backend
            .update_agent_keyed(&reviewer.agent, update, &update_key),
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                reviewer = %reviewer.agent,
                from = %switch.from,
                to = %switch.to.model,
                reason = "provider quota exhausted",
                "replaced reviewer model; hold will be rechecked"
            );
            Ok(())
        }
        Err(error) if error.is_store() || resume::unanswered(&error) || is_internal(&error) => {
            Err(error)
        }
        Err(error) => {
            tracing::warn!(reviewer = %reviewer.agent, model = %switch.to.model, %error, "reviewer quota replacement refused");
            wait.refused.insert(switch.to.model.clone());
            wait.replacement = None;
            save_wait(ctx, key, wait).await
        }
    }
}

async fn reserve_attempt(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &mut Wait,
    to: ModelChoice,
) -> Result<(), ArchonError> {
    if let Some(previous) = &wait.replacement {
        if previous.from == reviewer.model {
            assert_eq!(previous.to, to, "a retry uses the same replacement");
            return Ok(());
        }
        assert_eq!(
            previous.to.model, reviewer.model,
            "the previous update applied"
        );
    }
    wait.attempt = wait.attempt.checked_add(1).ok_or_else(|| {
        ArchonError::Corrupt(format!(
            "too many replacement attempts for {}",
            reviewer.agent
        ))
    })?;
    wait.replacement = Some(Replacement {
        from: reviewer.model.clone(),
        to,
    });
    save_wait(ctx, key, wait).await
}

pub(super) async fn release(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &Wait,
) -> Result<(), ArchonError> {
    assert_ne!(reviewer.agent, reviewer.parent);
    assert_ne!(
        reviewer.model.provider, wait.source.provider,
        "a replacement moved providers"
    );
    tracing::info!(
        reviewer = %reviewer.agent,
        from = %wait.source,
        to = %reviewer.model,
        reason = "provider quota exhausted",
        "releasing quota hold on replacement reviewer"
    );
    // Aether keeps the old exhaustion block until an operator message wakes it.
    let text = format!(
        "Archon switched your review from {} to {} because of provider quota. {}",
        wait.source, reviewer.model, "Continue the same review under its existing instructions."
    );
    let message_key = format!("{key}:release:{}:{}", wait.since.to_rfc3339(), wait.attempt);
    bounded(
        1,
        ctx.backend
            .send_message(&reviewer.agent, &text, &message_key),
    )
    .await?;
    ctx.store.lock().await.kv_delete(key)
}

fn named_choices(spawn: &StoredSpawn, reviewer: &Descendant) -> Vec<ModelChoice> {
    let Some(effort) = reviewer.effort else {
        return Vec::new();
    };
    if let Some(mut choices) = spawn.named_reviewers() {
        let floor = spawn.issued_effort_contract() == Some(EffortContract::Floor);
        for choice in &mut choices {
            choice.effort = if floor {
                effort.max(choice.effort)
            } else {
                effort
            };
        }
        debug_assert!(choices.iter().all(|choice| choice.effort >= effort));
        return choices;
    }
    spawn
        .request
        .delegation_policy
        .iter()
        .flat_map(|policy| &policy.exempt_models)
        .map(|model| ModelChoice::pinned(model.clone(), effort))
        .collect()
}

fn connected(catalog: &ModelCatalog, model: &ModelRef) -> Option<ModelRef> {
    match catalog.check(model) {
        Ok(model) => Some(model),
        Err(error) => {
            tracing::debug!(%model, %error, "quota replacement model unavailable");
            None
        }
    }
}
