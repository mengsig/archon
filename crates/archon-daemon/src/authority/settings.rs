//! Scope-aware previews keep confirmations and undo on the same settings path.
use crate::{context::DaemonContext, handlers};
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Reply, RepoRef, Request};
use archon_core::settings::{OperatorSettings, SettingsView};
use serde_json::Value;

pub(super) async fn snapshot(
    ctx: &DaemonContext,
    repo: &Option<RepoRef>,
) -> Result<Value, ArchonError> {
    match Box::pin(handlers::route(
        ctx,
        ClientKind::ArchonCli,
        Request::Settings { repo: repo.clone() },
    ))
    .await?
    {
        Reply::Settings(view) => Ok(serde_json::to_value(view)?),
        _ => unreachable!("settings returns its view"),
    }
}

pub(super) async fn loosens(
    ctx: &DaemonContext,
    repo: &Option<RepoRef>,
    next: &OperatorSettings,
    before: &Value,
) -> Result<bool, ArchonError> {
    let old: SettingsView = serde_json::from_value(before.clone())?;
    let id = match repo {
        Some(repo) => Some(ctx.store.lock().await.repo(repo)?.id),
        None => None,
    };
    let new = crate::settings::preview(ctx, id.as_ref(), next).await?;
    // Removing an explicit lowering can expose inherited permissions even
    // when the repository file currently masks both effective values.
    let mut raw_env = crate::resolve::RepoEnv::load(ctx, id.as_ref()).await?;
    raw_env.config = Default::default();
    let raw_old = raw_env.effective_with(ctx, &old.scoped);
    let raw_new = raw_env.effective_with(ctx, next);

    // Compare both effective policy and explicit knobs: a lowering repository
    // file must not hide a raise that would surface when that file is removed.
    Ok(!raw_new
        .autonomy
        .default
        .excess(raw_old.autonomy.default)
        .is_empty()
        || !raw_new
            .autonomy
            .ceiling
            .excess(raw_old.autonomy.ceiling)
            .is_empty()
        || super::actions::caps_loosen(&raw_old.caps, &raw_new.caps)
        || !new
            .autonomy
            .default
            .excess(old.effective.autonomy.default)
            .is_empty()
        || !new
            .autonomy
            .ceiling
            .excess(old.effective.autonomy.ceiling)
            .is_empty()
        || !next
            .autonomy_default
            .excess(
                old.scoped
                    .autonomy_default
                    .over(old.effective.autonomy.default),
            )
            .is_empty()
        || !next
            .autonomy_max
            .excess(old.scoped.autonomy_max.over(old.effective.autonomy.ceiling))
            .is_empty()
        || super::actions::caps_loosen(&old.effective.caps, &new.caps)
        || old.scoped.default_pipeline != next.default_pipeline)
}
