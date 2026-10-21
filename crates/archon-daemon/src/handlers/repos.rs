//! The repository registry (ARCHITECTURE §13).

use std::path::{Path, PathBuf};

use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::github::{GithubRepo, WatcherChange};
use archon_core::id::RepoId;
use archon_core::protocol::{Reply, RepoAdd, RepoPatch, RepoRef};
use archon_core::repo::{Repo, ShipMode};

use crate::context::DaemonContext;
use crate::executor::repo_identity;
use crate::resolve::RepoEnv;
use crate::{feed, github, worktree};

/// Register a git top level. Unset fields take the repository's
/// `.archon/config.toml` defaults; the base branch and default pipeline
/// must resolve. With `github_watch` its GitHub origin is also watched,
/// checked first so that a repository that cannot be watched is not registered.
pub(super) async fn add(ctx: &DaemonContext, add: RepoAdd) -> Result<Reply, ArchonError> {
    add_with_watch(ctx, add, false).await
}

/// The authenticated operator socket can start the watcher root, outside
/// the authority guards. Delegated authority uses the root it already has.
pub(super) async fn add_from_operator(
    ctx: &DaemonContext,
    add: RepoAdd,
) -> Result<Reply, ArchonError> {
    add_with_watch(ctx, add, true).await
}

async fn add_with_watch(
    ctx: &DaemonContext,
    add: RepoAdd,
    ensure_root: bool,
) -> Result<Reply, ArchonError> {
    if !add.path.is_absolute() {
        return Err(ArchonError::Invalid(format!(
            "repository path {} must be absolute",
            add.path.display()
        )));
    }
    let path = add.path.clone();
    let (root, config) = blocking(move || inspect_repo(&path)).await?;
    refuse_registered(ctx, &root).await?;
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
    let base_branch = match add.base_branch.or(config.repo.base_branch) {
        Some(base) => base,
        None => {
            let root = root.clone();
            blocking(move || current_branch(&root)).await?
        }
    };
    let repo = Repo {
        id: RepoId::new(),
        name,
        path: root,
        ship: add.ship.or(config.repo.ship).unwrap_or_default(),
        base_branch: Some(base_branch),
        default_pipeline: add.default_pipeline.or(config.repo.default_pipeline),
        autonomy_max: registry_ceiling(add.auto_merge),
        grant_merge: false,
        merge_waive_checks: Vec::new(),
        attribution: Default::default(),
        gh_user: None,
        tiers: Default::default(),
        confirmed_fast_check: None,
        confirmed_full_check: None,
        confirmed_release_check: None,
        added_at: archon_core::now(),
    };
    check_repo(&repo).await?;
    require_origin_for_pr(&repo).await?;
    ctx.store.lock().await.insert_repo(&repo)?;
    repo_identity::record(ctx, &repo).await?;
    // The orchestrator reads it in a mirror, made in the background.
    ctx.mirrors.request_repo(&repo.id);
    tracing::info!(repo = %repo.name, path = %repo.path.display(), "repository registered");
    if let Some(repository) = watch {
        watch_origin(ctx, &repo, repository, ensure_root).await?;
    }
    Ok(Reply::Repo(repo))
}

/// Watch a registered repository's GitHub origin: the `/watch` chat command.
pub(super) async fn watch(ctx: &DaemonContext, reference: &RepoRef) -> Result<String, ArchonError> {
    let repo = ctx.store.lock().await.repo(reference)?;
    let repository = github_origin(ctx, &repo.path).await?;
    watch_origin(ctx, &repo, repository.clone(), true).await?;
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
    ensure_root: bool,
) -> Result<(), ArchonError> {
    let change = WatcherChange::Add {
        repository: repository.clone(),
        allowed_users: None,
    };
    let result = if ensure_root {
        github::configure(ctx, change).await
    } else {
        github::configure_existing(ctx, change).await
    };
    if let Err(error) = result {
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
    if let Some(bases) = patch.merge_waive_checks {
        repo.merge_waive_checks = checked_bases(bases)?;
    }
    if let Some(attribution) = patch.attribution {
        repo.attribution = attribution;
    }
    repo.gh_user = patch.gh_user.apply(repo.gh_user);
    let confirmed = patch.tiers.confirmed_fast_check.clone();
    repo.confirmed_fast_check = confirmed.apply(repo.confirmed_fast_check);
    let confirmed = patch.tiers.confirmed_full_check.clone();
    repo.confirmed_full_check = confirmed.apply(repo.confirmed_full_check);
    let confirmed = patch.tiers.confirmed_release_check.clone();
    repo.confirmed_release_check = confirmed.apply(repo.confirmed_release_check);
    repo.tiers = patch.tiers.apply(repo.tiers);
    check_repo(&repo).await?;
    if repo.ship == ShipMode::Pr && before.ship != ShipMode::Pr {
        require_origin_for_pr(&repo).await?;
    }
    assert_eq!(repo.id, before.id, "a patch never changes identity");
    assert_eq!(repo.path, before.path, "a patch never moves a repository");
    ctx.store.lock().await.update_repo(&repo)?;
    if repo.grant_merge != before.grant_merge {
        crate::feed::system(ctx, grant_note(&repo)).await?;
    }
    if repo.merge_waive_checks != before.merge_waive_checks {
        crate::feed::system(ctx, waive_note(&repo)).await?;
    }
    if repo.gh_user != before.gh_user {
        crate::feed::system(ctx, gh_user_note(&repo)).await?;
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

/// Branch names without blanks, whitespace or repeats.
fn checked_bases(bases: Vec<String>) -> Result<Vec<String>, ArchonError> {
    let mut kept: Vec<String> = Vec::new();
    for base in bases {
        if base.is_empty()
            || base.starts_with('-')
            || base.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ArchonError::Invalid(format!(
                "{base:?} is not a branch name for merge_waive_checks"
            )));
        }
        if !kept.contains(&base) {
            kept.push(base);
        }
    }
    Ok(kept)
}

/// The feed's record of the operator changing `merge_waive_checks` (§18.6).
fn waive_note(repo: &Repo) -> String {
    if repo.merge_waive_checks.is_empty() {
        return format!(
            "merge_waive_checks is empty for {}: a granted merge waits for green checks",
            repo.name
        );
    }
    format!(
        "merge_waive_checks for {} is {}: under grant_merge, archond merges a reviewed pull \
         request into those branches without waiting for its checks",
        repo.name,
        repo.merge_waive_checks.join(", ")
    )
}

/// The feed's record of the operator changing `gh_user`.
fn gh_user_note(repo: &Repo) -> String {
    match &repo.gh_user {
        Some(user) => format!(
            "{} now runs `gh` and pushes as the GitHub login {user}, with the token `gh auth \
             token --user {user}` gives for each call",
            repo.name
        ),
        None => format!(
            "{} runs `gh` and pushes as archond's own GitHub login again",
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

/// The base branch resolves to a commit, the tiers are well formed and the
/// default pipeline loads. A missing integration branch is created on use.
async fn check_repo(repo: &Repo) -> Result<(), ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    repo.tiers.validate_fields()?;
    if let Some(confirmed) = &repo.confirmed_fast_check {
        archon_core::repo::check_command("confirmed fast check", confirmed)?;
    }
    if let Some(confirmed) = &repo.confirmed_full_check {
        archon_core::repo::check_command("confirmed full check", confirmed)?;
    }
    if let Some(confirmed) = &repo.confirmed_release_check {
        archon_core::repo::check_command("confirmed release hook", confirmed)?;
    }
    RepoEnv::new(Some(repo.clone()))?.tiers()?;
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

/// The branch checked out at `root`: the base a repository registered
/// without one delivers to. A repository with no commits or a detached
/// HEAD has none, and says so.
fn current_branch(root: &Path) -> Result<String, ArchonError> {
    if worktree::commit_id(root, "HEAD")?.is_none() {
        return Err(ArchonError::Invalid(format!(
            "repository has no commits: {} needs a first commit before it can be registered",
            root.display()
        )));
    }
    let branch = worktree::git(root, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "{} has a detached HEAD; check out a branch or pass --base",
            root.display()
        )));
    }
    assert!(!branch.contains('\n'), "one current branch");
    Ok(branch)
}

/// A path that is already registered is said to be, before any rule that
/// only a new registration must meet.
async fn refuse_registered(ctx: &DaemonContext, root: &Path) -> Result<(), ArchonError> {
    assert!(root.is_absolute(), "inspected roots are canonical");
    let repos = ctx.store.lock().await.repos()?;
    match repos.iter().find(|repo| repo.path == root) {
        Some(repo) => Err(ArchonError::Invalid(format!(
            "{} is already registered as {}; change it with `archon repo set {}`",
            root.display(),
            repo.name,
            repo.name
        ))),
        None => Ok(()),
    }
}

/// A `ship = pr` repository pushes to `origin`, so it must have one.
async fn require_origin_for_pr(repo: &Repo) -> Result<(), ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    if repo.ship != ShipMode::Pr {
        return Ok(());
    }
    let root = repo.path.clone();
    let origin = blocking(move || worktree::stack::origin_url(&root)).await?;
    if origin.is_none() {
        return Err(ArchonError::Invalid(format!(
            "{} has no origin remote, which ship = pr needs; add one or use ship = local",
            repo.path.display()
        )));
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
