//! A GitHub operator's commands reach only the repository of the thread they were posted in.

use archon_core::authority::{OperatorChannel, OperatorMessage};
use archon_core::error::ArchonError;
use archon_core::github::GithubRepo;
use archon_core::id::QueueItemId;
use archon_core::notify::Question;
use archon_core::protocol::{PauseTarget, Request};
use archon_core::repo::Repo;

use crate::context::DaemonContext;

fn refused(why: &str) -> ArchonError {
    ArchonError::Forbidden(format!(
        "GitHub operator commands cover only the repository of their own thread: {why}"
    ))
}

/// Refuse a GitHub `action` that is global or names another repository.
/// Other channels pass untouched.
pub(super) async fn check_action(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    action: &Request,
) -> Result<(), ArchonError> {
    let OperatorChannel::Github {
        repository, login, ..
    } = &message.channel
    else {
        return Ok(());
    };
    let own = thread_repo(ctx, repository, login).await?;
    let target = match action {
        Request::SetSettings { repo, .. } | Request::ResetSettings { repo } => repo
            .as_ref()
            .ok_or_else(|| refused("global settings need the operator's own channel"))?,
        Request::RepoUpdate { repo, .. } | Request::RepoRemove { repo } => repo,
        Request::Pause(PauseTarget::Repo { name })
        | Request::Resume(PauseTarget::Repo { name }) => name,
        Request::UpdateItem { item, .. } => return item_in(ctx, &own, item).await,
        _ => {
            return Err(refused(
                "this change is global or not tied to the thread's repository",
            ));
        }
    };
    let named = ctx.store.lock().await.repo(target)?;
    if named.id != own.id {
        return Err(refused("that repository is not the thread's"));
    }
    Ok(())
}

/// Refuse a GitHub operator naming an item of another repository.
pub(crate) async fn check_item(
    ctx: &DaemonContext,
    thread: &GithubRepo,
    login: &str,
    item: &QueueItemId,
) -> Result<(), ArchonError> {
    let own = thread_repo(ctx, thread, login).await?;
    item_in(ctx, &own, item).await
}

/// Refuse a GitHub operator answering a question about another repository's item,
/// or an item-less question that is not an authority confirmation.
pub(crate) async fn check_question(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    question: &Question,
) -> Result<(), ArchonError> {
    let OperatorChannel::Github {
        repository, login, ..
    } = &message.channel
    else {
        return Ok(());
    };
    match &question.item {
        Some(item) => check_item(ctx, repository, login, item).await,
        None if super::confirmations::is_confirmation(ctx, &question.id).await? => Ok(()),
        None => Err(refused("that question is not about this repository")),
    }
}

async fn item_in(ctx: &DaemonContext, own: &Repo, item: &QueueItemId) -> Result<(), ArchonError> {
    let item = ctx.store.lock().await.item(item)?;
    if item.repo.as_ref() != Some(&own.id) {
        return Err(refused("that item belongs to another repository"));
    }
    Ok(())
}

/// The registered repository whose `origin` is `thread`, when Aether's
/// watcher delivers `login` for it.
async fn thread_repo(
    ctx: &DaemonContext,
    thread: &GithubRepo,
    login: &str,
) -> Result<Repo, ArchonError> {
    let repos = ctx.store.lock().await.repos()?;
    let wanted = thread.clone();
    let own = tokio::task::spawn_blocking(move || {
        for repo in repos {
            let origin = crate::worktree::stack::origin_url(&repo.path)?;
            if origin.as_deref().and_then(GithubRepo::from_remote_url) == Some(wanted.clone()) {
                return Ok(Some(repo));
            }
        }
        Ok::<_, ArchonError>(None)
    })
    .await
    .map_err(|e| ArchonError::io("origin lookup", std::io::Error::other(e)))??
    .ok_or_else(|| refused("no registered repository has this thread's repository as origin"))?;
    check_allowed(ctx, thread, login).await?;
    Ok(own)
}

async fn check_allowed(
    ctx: &DaemonContext,
    thread: &GithubRepo,
    login: &str,
) -> Result<(), ArchonError> {
    let status = ctx.aether.client()?.github_watcher().await?;
    let watched = status
        .config
        .repositories
        .iter()
        .find(|r| r.repository.eq_ignore_ascii_case(thread.as_str()))
        .ok_or_else(|| refused("the thread's repository is not watched"))?;
    let listed = |allowed: &Vec<String>| allowed.iter().any(|u| u.eq_ignore_ascii_case(login));
    if watched.allowed_users.as_ref().is_some_and(|a| !listed(a)) {
        return Err(refused("the repository's allowlist excludes this login"));
    }
    Ok(())
}
