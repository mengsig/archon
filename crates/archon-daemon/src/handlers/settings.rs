//! `Settings`, `SetSettings` and `ResetSettings`: the operator's overlays
//! at one scope, and the values in force there with their sources.

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
    })))
}

pub(super) async fn set(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
    settings: OperatorSettings,
) -> Result<Reply, ArchonError> {
    let id = env(ctx, repo).await?.repo.map(|r| r.id);
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

/// The repository named, read with its file; a broken file is refused so a
/// view never hides a lowering it could not read.
async fn env(ctx: &DaemonContext, repo: Option<&RepoRef>) -> Result<RepoEnv, ArchonError> {
    let repo = match repo {
        Some(reference) => Some(ctx.store.lock().await.repo(reference)?),
        None => None,
    };
    RepoEnv::new(repo)
}
