//! The repository registry (ARCHITECTURE §13).

use std::path::{Path, PathBuf};

use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::github::{GithubRepo, WatcherChange};
use archon_core::id::RepoId;
use archon_core::protocol::{Reply, RepoAdd, RepoPatch, RepoRef};
use archon_core::repo::Repo;

use crate::context::DaemonContext;
use crate::executor::repo_identity;
use crate::resolve::RepoEnv;
use crate::{feed, github, worktree};

/// Register a git top level. Unset fields take the repository's
/// `.archon/config.toml` defaults; the base branch and default pipeline
/// must resolve. With `github_watch` its GitHub origin is also watched,
/// checked first so that a repository that cannot be watched is not registered.
pub(super) async fn add(ctx: &DaemonContext, add: RepoAdd) -> Result<Reply, ArchonError> {
    if !add.path.is_absolute() {
        return Err(ArchonError::Invalid(format!(
            "repository path {} must be absolute",
            add.path.display()
        )));
    }
    let path = add.path.clone();
    let (root, config) = blocking(move || inspect_repo(&path)).await?;
    let name = match &add.name {
        Some(name) => name.clone(),
        None => dir_name(&root)?,
    };
    let watch = match add.github_watch {
        true => Some(github_origin(ctx, &root).await?),
        false => None,
    };
    if !Repo::valid_name(&name) {
        return Err(ArchonError::Invalid(format!(
            "repository name {name:?} must be 1-64 letters, digits, '-', '_' or '.', not starting with '.'"
        )));
    }
    let repo = Repo {
        id: RepoId::new(),
        name,
        path: root,
        ship: add.ship.or(config.repo.ship).unwrap_or_default(),
        base_branch: add.base_branch.or(config.repo.base_branch),
        default_pipeline: add.default_pipeline.or(config.repo.default_pipeline),
        autonomy_max: registry_ceiling(add.auto_merge),
        grant_merge: false,
        added_at: archon_core::now(),
    };
    check_repo(&repo).await?;
    ctx.store.lock().await.insert_repo(&repo)?;
    repo_identity::record(ctx, &repo).await?;
    // The orchestrator reads it in a mirror, made in the background.
    ctx.mirrors.request_repo(&repo.id);
    tracing::info!(repo = %repo.name, path = %repo.path.display(), "repository registered");
    if let Some(repository) = watch {
        watch_origin(ctx, &repo, repository).await?;
    }
    Ok(Reply::Repo(repo))
}

/// Watch a registered repository's GitHub origin: the `/watch` chat command.
pub(super) async fn watch(ctx: &DaemonContext, reference: &RepoRef) -> Result<String, ArchonError> {
    let repo = ctx.store.lock().await.repo(reference)?;
    let repository = github_origin(ctx, &repo.path).await?;
    watch_origin(ctx, &repo, repository.clone()).await?;
    Ok(format!("watching {repository} on GitHub for {}", repo.name))
}

/// The GitHub repository `root`'s origin names, in the mode the watcher needs.
async fn github_origin(ctx: &DaemonContext, root: &Path) -> Result<GithubRepo, ArchonError> {
    github::require_dedicated(ctx)?;
    let dir = root.to_path_buf();
    let url = blocking(move || worktree::stack::origin_url(&dir))
        .await?
        .ok_or_else(|| {
            ArchonError::Invalid(format!(
                "{} has no origin remote to watch on GitHub",
                root.display()
            ))
        })?;
    GithubRepo::from_remote_url(&url).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "origin {url} is not a github.com repository, so it cannot be watched"
        ))
    })
}

/// Watch `repository` for the just-registered `repo`; a failure says the
/// registration stands and how to retry the watch alone.
async fn watch_origin(
    ctx: &DaemonContext,
    repo: &Repo,
    repository: GithubRepo,
) -> Result<(), ArchonError> {
    let change = WatcherChange::Add {
        repository: repository.clone(),
        allowed_users: None,
    };
    if let Err(error) = github::configure(ctx, change).await {
        let detail = format!(
            "{} is registered but not watched; retry with `archon github-watcher add {repository}`",
            repo.name
        );
        return Err(error.with_detail(&detail));
    }
    feed::system(
        ctx,
        format!("Watching {repository} on GitHub for {}", repo.name),
    )
    .await?;
    Ok(())
}

pub(super) async fn list(ctx: &DaemonContext) -> Result<Reply, ArchonError> {
    Ok(Reply::Repos(ctx.store.lock().await.repos()?))
}

/// `auto_merge` at registration is the ceiling `merge=auto`, as before
/// autonomy knobs existed; `archon repo set --autonomy-max` sets the rest.
fn registry_ceiling(auto_merge: bool) -> AutonomyPatch {
    let merge = auto_merge.then_some(MergeMode::Auto);
    let ceiling = AutonomyPatch {
        merge,
        ..AutonomyPatch::default()
    };
    assert_eq!(
        ceiling.is_empty(),
        !auto_merge,
        "only auto_merge sets a ceiling"
    );
    ceiling
}

/// Refused while the repository has unfinished items (store rule).
pub(super) async fn remove(ctx: &DaemonContext, reference: &RepoRef) -> Result<Reply, ArchonError> {
    let mut store = ctx.store.lock().await;
    let repo = store.repo(reference)?;
    store.remove_repo(&repo.id)?;
    repo_identity::forget(&mut store, &repo.id)?;
    // Every pass forgets the mirrors of unregistered repositories.
    ctx.mirrors.request_repo(&repo.id);
    tracing::info!(repo = %repo.name, "repository removed");
    Ok(Reply::Ok)
}

/// The one path to a repository's autonomy ceiling (CLI only). A lowered
/// ceiling applies to enqueues and to runs started later.
pub(super) async fn update(
    ctx: &DaemonContext,
    reference: &RepoRef,
    patch: RepoPatch,
) -> Result<Reply, ArchonError> {
    let mut repo = ctx.store.lock().await.repo(reference)?;
    let before = repo.clone();
    if let Some(ship) = patch.ship {
        repo.ship = ship;
    }
    repo.base_branch = patch.base_branch.apply(repo.base_branch);
    repo.default_pipeline = patch.default_pipeline.apply(repo.default_pipeline);
    if let Some(max) = patch.autonomy_max {
        repo.autonomy_max = max;
    }
    if let Some(grant) = patch.grant_merge {
        repo.grant_merge = grant;
    }
    check_repo(&repo).await?;
    assert_eq!(repo.id, before.id, "a patch never changes identity");
    assert_eq!(repo.path, before.path, "a patch never moves a repository");
    ctx.store.lock().await.update_repo(&repo)?;
    if repo.grant_merge != before.grant_merge {
        crate::feed::system(ctx, grant_note(&repo)).await?;
    }
    // The operator's deliberate update is when a changed git identity is taken.
    repo_identity::record(ctx, &repo).await?;
    // Its base branch may have changed.
    ctx.mirrors.request_repo(&repo.id);
    Ok(Reply::Repo(repo))
}

/// The feed's record of the operator changing `grant_merge` (§18.6).
fn grant_note(repo: &Repo) -> String {
    match repo.grant_merge {
        true => format!(
            "grant_merge is on for {}: when you ask the orchestrator to merge an item's pull \
             request, archond merges it without a confirmation question, if it is reviewed and \
             green",
            repo.name
        ),
        false => format!(
            "grant_merge is off for {}: archond asks you to confirm each merge the orchestrator \
             requests",
            repo.name
        ),
    }
}

/// The canonical top level of `path` and its repository config. A path
/// inside a repository is refused so each repository has one registration.
fn inspect_repo(path: &Path) -> Result<(PathBuf, RepoConfig), ArchonError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| ArchonError::Invalid(format!("repository path {}: {e}", path.display())))?;
    let top = worktree::toplevel(&canonical).map_err(|e| {
        ArchonError::Invalid(format!(
            "{} is not a git repository: {e}",
            canonical.display()
        ))
    })?;
    if top != canonical {
        return Err(ArchonError::Invalid(format!(
            "{} is inside the git repository {}; register its top level",
            canonical.display(),
            top.display()
        )));
    }
    let config = RepoConfig::load(&top)?;
    Ok((top, config))
}

/// The base branch resolves to a commit and the default pipeline loads.
async fn check_repo(repo: &Repo) -> Result<(), ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    if let Some(base) = repo.base_branch.clone() {
        let root = repo.path.clone();
        blocking(move || {
            let spec = format!("{base}^{{commit}}");
            worktree::git(&root, &["rev-parse", "--verify", "--quiet", &spec]).map_err(|_| {
                ArchonError::Invalid(format!(
                    "base branch {base:?} does not exist in {}",
                    root.display()
                ))
            })
        })
        .await?;
    }
    if let Some(pipeline) = &repo.default_pipeline {
        let env = RepoEnv::new(Some(repo.clone()))?;
        env.catalog()?.get(pipeline)?;
    }
    Ok(())
}

fn dir_name(root: &Path) -> Result<String, ArchonError> {
    root.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            ArchonError::Invalid(format!(
                "{} has no usable directory name; pass a name",
                root.display()
            ))
        })
}

/// Run blocking git or file work off the async workers.
pub(super) async fn blocking<T, F>(work: F) -> Result<T, ArchonError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ArchonError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ArchonError::io("blocking task", std::io::Error::other(e)))?
}
