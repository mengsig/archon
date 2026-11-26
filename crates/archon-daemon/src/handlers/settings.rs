//! `Settings`, `SetSettings` and `ResetSettings`: the operator's overlays
//! at one scope, and the values in force there with their sources.

use archon_core::autonomy::ProposalMode;
use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, RepoRef};
use archon_core::settings::{OperatorSettings, SettingsView};

use crate::context::DaemonContext;
use crate::resolve::RepoEnv;

pub(super) async fn view(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
) -> Result<Reply, ArchonError> {
    let env = env(ctx, repo).await?;
    let id = env.repo.as_ref().map(|r| r.id.clone());
    let scoped = crate::settings::read(ctx, id.as_ref()).await?;
    let global = crate::settings::read(ctx, None).await?;
    let design_recipe = match &id {
        Some(repo) => crate::design::load_recipe(&*ctx.store.lock().await, repo)?,
        None => None,
    };
    let effective = env.effective(ctx);
    assert!(
        effective
            .autonomy
            .default
            .excess(effective.autonomy.ceiling)
            .is_empty()
    );
    Ok(Reply::Settings(Box::new(SettingsView {
        repo: env.repo_name().map(str::to_owned),
        scoped,
        global,
        effective,
        design_recipe,
    })))
}

pub(super) async fn set(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
    mut settings: OperatorSettings,
    legacy_proposals: bool,
) -> Result<Reply, ArchonError> {
    let id = env(ctx, repo).await?.repo.map(|r| r.id);
    if legacy_proposals {
        let current = crate::settings::read(ctx, id.as_ref()).await?;
        for (new, old) in [
            (&mut settings.autonomy_default, current.autonomy_default),
            (&mut settings.autonomy_max, current.autonomy_max),
        ] {
            if old.proposal == Some(ProposalMode::Live)
                && new.proposal == Some(ProposalMode::Lavish)
            {
                new.proposal = old.proposal;
            }
        }
    }
    crate::settings::write(ctx, id.as_ref(), settings).await?;
    view(ctx, repo).await
}

pub(super) async fn reset(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
) -> Result<Reply, ArchonError> {
    let id = env(ctx, repo).await?.repo.map(|r| r.id);
    crate::settings::reset(ctx, id.as_ref()).await?;
    view(ctx, repo).await
}

/// Forget a repository's saved design preview recipe and show its settings.
pub(super) async fn clear_design_recipe(
    ctx: &DaemonContext,
    repo: &RepoRef,
) -> Result<Reply, ArchonError> {
    crate::design::clear_recipe(ctx, repo).await?;
    let view = view(ctx, Some(repo)).await?;
    debug_assert!(
        matches!(&view, Reply::Settings(v) if v.design_recipe.is_none()),
        "the cleared recipe is not shown"
    );
    Ok(view)
}

/// The repository named, read with its file; a broken file is refused so a
/// view never hides a lowering it could not read.
async fn env(ctx: &DaemonContext, repo: Option<&RepoRef>) -> Result<RepoEnv, ArchonError> {
    let repo = match repo {
        Some(reference) => Some(ctx.store.lock().await.repo(reference)?),
        None => None,
    };
    RepoEnv::new(repo)
}
