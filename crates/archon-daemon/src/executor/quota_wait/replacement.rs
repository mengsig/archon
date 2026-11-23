use archon_core::error::ArchonError;
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
    quota: &QuotaView,
    pending: Option<&ModelChoice>,
) -> Result<Option<ModelChoice>, ArchonError> {
    if stage.aether_agent.as_ref() != Some(&reviewer.parent) {
        return Ok(None);
    }
    assert_ne!(reviewer.agent, reviewer.parent);
    assert_eq!(reviewer.role, "reviewer");
    let (spawn, repo) = {
        let store = ctx.store.lock().await;
        let Some(spawn) = issuing_spawn(&store, &stage.id)? else {
            return Ok(None);
        };
        let run = store.run(&stage.run)?;
        let item = store.item(&run.item)?;
        (spawn, item.repo)
    };
    let choices = named_choices(&spawn, reviewer);
    let has_quota = |model: &ModelRef| {
        model.provider != reviewer.model.provider
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
    let eligible = choices.into_iter().filter_map(|mut choice| {
        choice.model = connected(&catalog, &choice.model)?;
        (choice.model != *lead_model
            && has_quota(&choice.model)
            && pending.is_none_or(|pending| choice == *pending)
            && (!governed || provider_reports_cost(&choice.model.provider))
            && policy.check(&choice).is_ok())
        .then_some(choice)
    });
    Ok(eligible.min_by_key(|choice| {
        ctx.config.review.prefer_other_provider && choice.model.provider == lead_model.provider
    }))
}

pub(super) async fn apply(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &mut Wait,
    to: ModelChoice,
) -> Result<bool, ArchonError> {
    assert_ne!(to.model.provider, reviewer.model.provider);
    if let Some(pending) = &wait.replacement {
        assert_eq!(pending.to, to, "a retry uses the same replacement");
    } else {
        wait.replacement = Some(Replacement {
            from: reviewer.model.clone(),
            to,
        });
        save_wait(ctx, key, wait).await?;
    }
    if switch(ctx, reviewer, key, wait).await? {
        release(ctx, reviewer, key, wait).await?;
        return Ok(true);
    }
    wait.replacement = None;
    save_wait(ctx, key, wait).await?;
    Ok(false)
}

async fn switch(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &Wait,
) -> Result<bool, ArchonError> {
    let switch = wait
        .replacement
        .as_ref()
        .expect("the replacement was saved");
    assert_eq!(
        switch.from, reviewer.model,
        "the source reviewer still holds the old model"
    );
    assert_ne!(
        switch.to.model.provider, switch.from.provider,
        "the replacement has new quota"
    );
    let update = AgentUpdate {
        choice: Some(switch.to.clone()),
        ..AgentUpdate::default()
    };
    let update_key = format!("{key}:switch:{}", wait.since.to_rfc3339());
    match bounded(
        1,
        ctx.backend
            .update_agent_keyed(&reviewer.agent, update, &update_key),
    )
    .await
    {
        Ok(()) => Ok(true),
        Err(error) if error.is_store() || resume::unanswered(&error) => Err(error),
        Err(error) => {
            tracing::warn!(reviewer = %reviewer.agent, model = %switch.to.model, %error, "reviewer quota replacement refused");
            Ok(false)
        }
    }
}

pub(super) async fn release(
    ctx: &DaemonContext,
    reviewer: &Descendant,
    key: &str,
    wait: &Wait,
) -> Result<(), ArchonError> {
    let switch = wait
        .replacement
        .as_ref()
        .expect("only a replacement needs release");
    assert_ne!(reviewer.agent, reviewer.parent);
    assert!(reviewer.model == switch.from || reviewer.model == switch.to.model);
    if bounded(1, ctx.backend.quota_blocked(&reviewer.agent)).await? == Some(true) {
        // Aether keeps the old exhaustion block until an operator message wakes it.
        let text = format!(
            "Archon switched your review from {} to {} because of provider quota. {}",
            switch.from,
            switch.to.model,
            "Continue the same review under its existing instructions."
        );
        let message_key = format!("{key}:release:{}", wait.since.to_rfc3339());
        bounded(
            1,
            ctx.backend
                .send_message(&reviewer.agent, &text, &message_key),
        )
        .await?;
    }
    tracing::info!(
        reviewer = %reviewer.agent,
        from = %switch.from,
        to = %switch.to.model,
        reason = "provider quota exhausted",
        "replaced reviewer model"
    );
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
