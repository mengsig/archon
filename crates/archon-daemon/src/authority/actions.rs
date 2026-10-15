//! The finite set of commands delegated operator authority can reach.

use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, PauseTarget, Reply, Request};
use archon_core::queue::Caps;
use archon_core::repo::{Repo, ShipMode};
use serde_json::{Value, json};

use crate::{context::DaemonContext, handlers, resolve::RepoEnv};

pub(super) fn check_channel(
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
    if message.channel.is_github() && matches!(action, Request::ConfigureGithubWatcher { .. }) {
        return Err(ArchonError::Forbidden(
            "GitHub cannot change watcher repositories, allowlists or operators; use operator chat"
                .into(),
        ));
    }
    match action {
        Request::RepoUpdate { .. } | Request::RepoRemove { .. } | Request::RepoAdd(_)
        | Request::SetCaps { .. } | Request::Pause(_) | Request::Resume(_)
        | Request::ConfigureGithubWatcher { .. }
        | Request::SetDirective { .. } | Request::ClearDirective { .. }
        | Request::UpdateLesson { .. } | Request::LearningCorrect(_)
        | Request::LearningUndo { .. } | Request::LearningRestoreMemory { .. }
        | Request::LearningRetryMemory { .. } => Ok(()),
        _ => Err(ArchonError::Forbidden("this action is not available through operator authority; host lifecycle, destructive GC and nested requests stay excluded".into())),
    }
}

pub(super) async fn snapshot(ctx: &DaemonContext, action: &Request) -> Result<Value, ArchonError> {
    match action {
        Request::RepoUpdate { repo, .. } | Request::RepoRemove { repo } => {
            match ctx.store.lock().await.repo(repo) {
                Ok(repo) => Ok(serde_json::to_value(repo)?),
                Err(ArchonError::NotFound(_)) => Ok(Value::Null),
                Err(error) => Err(error),
            }
        }
        Request::SetCaps { .. } => Ok(serde_json::to_value(crate::resolve::caps(ctx).await)?),
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
        Request::SetCaps { caps } => {
            let old: Caps = serde_json::from_value(before.clone())?;
            Ok((old.budgets_enabled && !caps.budgets_enabled)
                || higher(old.budget_usd_total, caps.budget_usd_total)
                || higher(old.budget_usd_daily, caps.budget_usd_daily)
                || higher(old.budget_usd_per_item, caps.budget_usd_per_item)
                || higher(
                    old.max_parallel_agents.into(),
                    caps.max_parallel_agents.into(),
                )
                || higher(old.max_per_repo.into(), caps.max_per_repo.into())
                || higher(old.max_active_runs.into(), caps.max_active_runs.into())
                || old.max_per_pipeline.iter().any(|(key, old)| {
                    higher(
                        (*old).into(),
                        (*caps.max_per_pipeline.get(key).unwrap_or(&0)).into(),
                    )
                }))
        }
        Request::RepoUpdate { patch, .. } => {
            let repo: Repo = serde_json::from_value(before.clone())?;
            if patch.grant_merge == Some(true) && !repo.grant_merge
                || patch.ship == Some(ShipMode::Pr) && repo.ship == ShipMode::Local
                || patch.base_branch != Default::default()
                || patch.default_pipeline != Default::default()
            {
                return Ok(true);
            }
            if let Some(max) = &patch.autonomy_max {
                let old = RepoEnv::new(Some(repo.clone()))?.autonomy(ctx);
                let mut after = repo;
                after.autonomy_max = *max;
                let new = RepoEnv::new(Some(after))?.autonomy(ctx);
                return Ok(!new.ceiling.excess(old.ceiling).is_empty());
            }
            Ok(false)
        }
        Request::Resume(PauseTarget::Global { hard: true }) => Ok(true),
        // Conservative for actions whose direction cannot be ordered safely.
        _ => Ok(true),
    }
}

fn higher(old: f64, new: f64) -> bool {
    old != 0.0 && (new == 0.0 || new > old)
}

pub(super) async fn apply(ctx: &DaemonContext, action: Request) -> Result<Reply, ArchonError> {
    // Only callers that checked the finite allowlist and verified authority
    // reach this internal router. No socket identity is fabricated.
    Box::pin(handlers::route(ctx, ClientKind::ArchonCli, action)).await
}
