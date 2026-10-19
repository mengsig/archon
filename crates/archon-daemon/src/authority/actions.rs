//! The finite set of commands delegated operator authority can reach.

use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, PauseTarget, Reply, RepoPatch, Request};
use archon_core::queue::Caps;
use archon_core::repo::{Repo, ShipMode};
use serde_json::{Value, json};

use crate::{context::DaemonContext, handlers, resolve::RepoEnv};

pub(super) async fn check_channel(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    action: &Request,
) -> Result<(), ArchonError> {
    if matches!(
        message.channel,
        archon_core::authority::OperatorChannel::AetherChat
    ) && ctx.config.aether.mode != archon_core::config::AetherMode::Dedicated
    {
        return Err(ArchonError::Forbidden(
            "operator authority from Aether chat is disabled in shared mode".into(),
        ));
    }
    if message.channel.is_github()
        && (matches!(action, Request::ConfigureGithubWatcher { .. })
            || matches!(action, Request::RepoAdd(add) if add.github_watch))
    {
        return Err(ArchonError::Forbidden(
            "GitHub cannot change watcher repositories, allowlists or operators; use operator chat"
                .into(),
        ));
    }
    super::scope::check_action(ctx, message, action).await?;
    let allowed = match action {
        // Only an item's autonomy, which `apply` raises above its ceiling.
        Request::UpdateItem { patch, .. } => {
            let mut rest = patch.clone();
            rest.autonomy.take().is_some() && rest == Default::default()
        }
        Request::SteerItem { .. } => false,
        action => action.changes_authority_state(),
    };
    if !allowed {
        return Err(ArchonError::Forbidden("this action is not available through operator authority; host lifecycle, destructive GC and nested requests stay excluded".into()));
    }
    Ok(())
}

pub(super) async fn snapshot(ctx: &DaemonContext, action: &Request) -> Result<Value, ArchonError> {
    match action {
        Request::SetSettings { repo, .. } | Request::ResetSettings { repo } => {
            super::settings::snapshot(ctx, repo).await
        }
        Request::UpdateItem { item, .. } => {
            let item = ctx.store.lock().await.item(item)?;
            let policy = RepoEnv::load(ctx, item.repo.as_ref()).await?.autonomy(ctx);
            Ok(
                json!({"item":item.id,"autonomy":item.autonomy,"state":item.state,"ceiling":policy.ceiling}),
            )
        }
        Request::RepoUpdate { repo, .. } | Request::RepoRemove { repo } => {
            match ctx.store.lock().await.repo(repo) {
                Ok(repo) => {
                    let policy = RepoEnv::new(Some(repo.clone()))?.autonomy(ctx);
                    let mut value = serde_json::to_value(repo)?;
                    value["effective_autonomy"] = serde_json::to_value(policy)?;
                    Ok(value)
                }
                Err(ArchonError::NotFound(_)) => Ok(Value::Null),
                Err(error) => Err(error),
            }
        }
        Request::SetCaps { .. } => super::settings::snapshot(ctx, &None).await,
        Request::Pause(_) | Request::Resume(_) => {
            Ok(serde_json::to_value(ctx.store.lock().await.pause_state()?)?)
        }
        Request::SetDirective { .. } | Request::ClearDirective { .. } => Ok(serde_json::to_value(
            ctx.store.lock().await.directives(archon_core::now())?,
        )?),
        Request::ConfigureGithubWatcher { .. } => {
            let status = ctx.aether.client()?.github_watcher().await?;
            Ok(json!({"revision":status.revision,"config":status.config}))
        }
        // These operations validate their domain invariants again at apply time.
        _ => Ok(json!({"request": action})),
    }
}

pub(super) async fn needs_confirmation(
    ctx: &DaemonContext,
    action: &Request,
    before: &Value,
) -> Result<bool, ArchonError> {
    match action {
        Request::Pause(_) | Request::SetDirective { .. } => Ok(false),
        Request::SetSettings { repo, settings } => {
            super::settings::loosens(ctx, repo, settings, before).await
        }
        Request::ResetSettings { repo } => {
            super::settings::loosens(ctx, repo, &Default::default(), before).await
        }
        Request::UpdateItem { item, patch } => {
            let item = ctx.store.lock().await.item(item)?;
            let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
            let (base, _) = crate::knobs::item_specs(&env, &item)?;
            let policy = env.autonomy(ctx);
            let autonomy = patch.autonomy.as_ref().expect("allowlisted autonomy patch");
            let after = crate::knobs::revise(&base, item.autonomy, autonomy, &policy)?;
            Ok(crate::knobs::check_raised(&policy, &env, &base, item.autonomy, after).is_err())
        }
        Request::SetCaps { caps } => {
            let old: archon_core::settings::SettingsView = serde_json::from_value(before.clone())?;
            Ok(caps_loosen(&old.effective.caps, caps))
        }
        Request::RepoUpdate { patch, .. } => {
            let repo: Repo = serde_json::from_value(before.clone())?;
            // Destructured so that a new field cannot skip this decision.
            let RepoPatch {
                ship,
                base_branch,
                default_pipeline,
                autonomy_max,
                grant_merge,
                merge_waive_checks,
                attribution,
                gh_user,
                tiers,
            } = patch;
            // The operator alone decides what a repository's pull requests attribute,
            // and which GitHub login publishes them.
            let waived_more = merge_waive_checks
                .as_ref()
                .is_some_and(|bases| bases.iter().any(|b| !repo.merge_waive_checks.contains(b)));
            if *grant_merge == Some(true) && !repo.grant_merge
                || waived_more
                || attribution.is_some_and(|a| a != repo.attribution)
                || gh_user.clone().apply(repo.gh_user.clone()) != repo.gh_user
                || *ship == Some(ShipMode::Pr) && repo.ship == ShipMode::Local
                || *base_branch != Default::default()
                || *default_pipeline != Default::default()
                || !tiers.is_keep()
            {
                return Ok(true);
            }
            if let Some(max) = autonomy_max {
                let old = RepoEnv::new(Some(repo.clone()))?.autonomy(ctx);
                let global = crate::settings::mirror(ctx).global.autonomy_max.over(
                    ctx.config
                        .autonomy
                        .max
                        .over(archon_core::autonomy::Autonomy::default_ceiling()),
                );
                let old_raw = repo.autonomy_max.over(global);
                let new_raw = max.over(global);
                let mut after = repo;
                after.autonomy_max = *max;
                let new = RepoEnv::new(Some(after))?.autonomy(ctx);
                return Ok(!new.ceiling.excess(old.ceiling).is_empty()
                    || !new_raw.excess(old_raw).is_empty());
            }
            Ok(false)
        }
        Request::Resume(PauseTarget::Global { hard: true }) => Ok(true),
        // Conservative for actions whose direction cannot be ordered safely.
        _ => Ok(true),
    }
}

pub(super) fn caps_loosen(old: &Caps, new: &Caps) -> bool {
    (old.budgets_enabled && !new.budgets_enabled)
        || higher(old.budget_usd_total, new.budget_usd_total)
        || higher(old.budget_usd_daily, new.budget_usd_daily)
        || higher(old.budget_usd_per_item, new.budget_usd_per_item)
        || higher(
            old.max_parallel_agents.into(),
            new.max_parallel_agents.into(),
        )
        || higher(old.max_per_repo.into(), new.max_per_repo.into())
        || higher(old.max_active_runs.into(), new.max_active_runs.into())
        || old.max_per_pipeline.iter().any(|(key, old)| {
            higher(
                (*old).into(),
                (*new.max_per_pipeline.get(key).unwrap_or(&0)).into(),
            )
        })
}

fn higher(old: f64, new: f64) -> bool {
    old != 0.0 && (new == 0.0 || new > old)
}

pub(super) async fn apply(ctx: &DaemonContext, action: Request) -> Result<Reply, ArchonError> {
    if let Request::UpdateItem { item, patch } = action {
        return handlers::update_item_autonomy_above_ceiling(
            ctx,
            &item,
            patch.autonomy.expect("allowlisted autonomy patch"),
        )
        .await;
    }
    // Only callers that checked the finite allowlist and verified authority
    // reach this internal router. No socket identity is fabricated.
    Box::pin(handlers::route(ctx, ClientKind::ArchonCli, action)).await
}
