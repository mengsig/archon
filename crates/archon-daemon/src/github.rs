//! Archon's GitHub watcher (docs/github-watcher.md). Aether's own watcher,
//! in Archon's dedicated aetherd, delivers each `@archon` request from a
//! verified collaborator to the orchestrator: every `fixed` repository there
//! is Archon's, and archond keeps it pointed at the live orchestrator.
//! Only the operator's CLI edits it; nothing from GitHub can.

mod reply;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use archon_aether::github::{RepositoryConfig, WatcherConfig, WatcherStatus};
use archon_core::aether_ids::AetherAgentId;
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::github::{
    AllowedUsers, GithubRepo, GithubWatch, GithubWatcherView, LABEL_REGEX, MENTION_REGEX,
    WatchTarget, WatchedRepo, WatcherChange,
};
use tokio::sync::{Mutex, Notify};

use crate::context::DaemonContext;
use crate::supervisor;

pub use reply::{post_reply, reply_body};

/// How often archond checks the targets between orchestrator changes.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Aether's shipped patterns: a config still carrying them was never tuned.
const AETHER_MENTION_REGEX: &str = "(?i)(?:^|[^a-z0-9_@])@aether(?:$|[^a-z0-9_-])";
const AETHER_LABEL_REGEX: &str = "(?i)aether";

/// What a refused `operators` edit left undone, after the reason it failed.
const OPERATORS_NOT_SET: &str = "the operator logins were not set, so set them again afterwards";

#[derive(Debug, Default)]
pub struct GithubState {
    /// Serializes archond's read-modify-write of Aether's watcher config.
    serial: Mutex<()>,
    /// Poked on every connect to Aether.
    check: Notify,
}

/// Archon's watcher lives in Archon's own aetherd; the operator's shared
/// Aether and its watcher are never touched.
pub(crate) fn require_dedicated(ctx: &DaemonContext) -> Result<(), ArchonError> {
    match ctx.config.aether.mode {
        AetherMode::Dedicated => Ok(()),
        AetherMode::Shared => Err(ArchonError::Invalid(
            "the GitHub watcher needs Archon's own Aether; [aether] mode is \"shared\", \
             and archond never edits the operator's Aether"
                .into(),
        )),
    }
}

/// Apply the operator's `change` to `config`. Adding or enabling a
/// repository points it at `orchestrator`, which those changes require.
/// An allowlist only narrows dispatch; only `Operators` changes operator logins.
pub fn apply_change(
    config: &mut WatcherConfig,
    change: WatcherChange,
    orchestrator: Option<&AetherAgentId>,
) -> Result<(), ArchonError> {
    let needs_target = || {
        orchestrator.cloned().ok_or_else(|| {
            ArchonError::Unavailable("the orchestrator is not running to take requests".into())
        })
    };
    match change {
        WatcherChange::Add {
            repository,
            allowed_users,
        } => add(config, &repository, needs_target()?, allowed_users)?,
        WatcherChange::Allow {
            repository,
            allowed_users,
        } => {
            let index = position(config, &repository)?;
            config.repositories[index].allowed_users = allowed_users.map(Vec::from);
        }
        WatcherChange::Operators { operator_users } => {
            config.operator_users = operator_users.map(Vec::from);
        }
        WatcherChange::Remove { repository } => {
            let index = position(config, &repository)?;
            config.repositories.remove(index);
        }
        WatcherChange::Enable { repository: None } => config.enabled = true,
        WatcherChange::Disable { repository: None } => config.enabled = false,
        WatcherChange::Enable {
            repository: Some(repository),
        } => {
            let target = needs_target()?;
            let index = position(config, &repository)?;
            let repo = &mut config.repositories[index];
            repo.enabled = true;
            if repo.is_fixed() {
                repo.agent = Some(target);
            }
        }
        WatcherChange::Disable {
            repository: Some(repository),
        } => {
            let index = position(config, &repository)?;
            config.repositories[index].enabled = false;
        }
        WatcherChange::Configure {
            mention_regex,
            label_regex,
            poll_interval_secs,
        } => set_settings(config, mention_regex, label_regex, poll_interval_secs)?,
    }
    Ok(())
}

/// Watch `repository` through the orchestrator and switch the watcher on,
/// with Archon's patterns if Aether's untuned ones are still set. A given
/// allowlist replaces the repository's own; without one it is kept.
fn add(
    config: &mut WatcherConfig,
    repository: &GithubRepo,
    target: AetherAgentId,
    allowed_users: Option<AllowedUsers>,
) -> Result<(), ArchonError> {
    if let Ok(index) = position(config, repository) {
        let repo = &mut config.repositories[index];
        if !repo.is_fixed() {
            return Err(ArchonError::Invalid(format!(
                "{repository} is watched in {} dispatch, set through `archon aether \
                 github-watcher`; remove it there before handing it to Archon",
                repo.mode()
            )));
        }
        repo.enabled = true;
        repo.agent = Some(target);
        if let Some(users) = allowed_users {
            repo.allowed_users = Some(users.into());
        }
    } else {
        let name = repository.as_str().to_owned();
        config.repositories.push(RepositoryConfig {
            allowed_users: allowed_users.map(Vec::from),
            ..RepositoryConfig::fixed(name, target)
        });
    }
    if config.mention_regex == AETHER_MENTION_REGEX {
        MENTION_REGEX.clone_into(&mut config.mention_regex);
    }
    if config.label_regex == AETHER_LABEL_REGEX {
        LABEL_REGEX.clone_into(&mut config.label_regex);
    }
    config.enabled = true;
    Ok(())
}

fn set_settings(
    config: &mut WatcherConfig,
    mention_regex: Option<String>,
    label_regex: Option<String>,
    poll_interval_secs: Option<u64>,
) -> Result<(), ArchonError> {
    if mention_regex.is_none() && label_regex.is_none() && poll_interval_secs.is_none() {
        return Err(ArchonError::Invalid(
            "name a setting to change: mention regex, label regex or poll interval".into(),
        ));
    }
    // Aether validates the patterns and the interval's range.
    if let Some(pattern) = mention_regex {
        config.mention_regex = pattern;
    }
    if let Some(pattern) = label_regex {
        config.label_regex = pattern;
    }
    if let Some(secs) = poll_interval_secs {
        config.poll_interval_secs = secs;
    }
    Ok(())
}

/// Index of `repository` in `config`; Aether stores names lowercased.
fn position(config: &WatcherConfig, repository: &GithubRepo) -> Result<usize, ArchonError> {
    config
        .repositories
        .iter()
        .position(|r| r.repository.eq_ignore_ascii_case(repository.as_str()))
        .ok_or_else(|| ArchonError::NotFound(format!("watched GitHub repository {repository}")))
}

/// Point every `fixed` repository at `orchestrator`; whether any changed.
/// Other dispatch modes are the operator's own and stay as they are.
pub fn repoint(config: &mut WatcherConfig, orchestrator: &AetherAgentId) -> bool {
    let mut changed = false;
    for repo in config.repositories.iter_mut().filter(|r| r.is_fixed()) {
        if repo.agent.as_ref() != Some(orchestrator) {
            repo.agent = Some(orchestrator.clone());
            changed = true;
        }
    }
    debug_assert!(
        config
            .repositories
            .iter()
            .filter(|r| r.is_fixed())
            .all(|r| r.agent.as_ref() == Some(orchestrator)),
        "every fixed repository targets the orchestrator"
    );
    changed
}

/// The `operators` a repository's allowlist leaves out; none without one.
fn excluded_operators(operators: &[String], allowed: Option<&[String]>) -> Vec<String> {
    let Some(allowed) = allowed else {
        return Vec::new();
    };
    let listed = |login: &String| allowed.iter().any(|user| user.eq_ignore_ascii_case(login));
    let excluded: Vec<String> = operators
        .iter()
        .filter(|login| !listed(login))
        .cloned()
        .collect();
    debug_assert!(excluded.len() <= operators.len());
    excluded
}

/// What `status` says, seen against the orchestrator Archon runs.
pub fn view(status: &WatcherStatus, orchestrator: Option<&AetherAgentId>) -> GithubWatcherView {
    let operators = status.config.operator_users.as_deref().unwrap_or_default();
    let repositories = status
        .config
        .repositories
        .iter()
        .map(|repo| {
            let target = match (repo.is_fixed(), &repo.agent) {
                (false, _) => WatchTarget::Unmanaged {
                    mode: repo.mode().to_owned(),
                },
                (true, Some(agent)) if Some(agent) == orchestrator => WatchTarget::Orchestrator,
                (true, agent) => WatchTarget::Stale {
                    agent: agent.clone(),
                },
            };
            let health = status
                .repositories
                .iter()
                .find(|h| h.repository.eq_ignore_ascii_case(&repo.repository));
            WatchedRepo {
                repository: repo.repository.clone(),
                enabled: repo.enabled,
                target,
                allowed_users: repo.allowed_users.clone(),
                excluded_operators: excluded_operators(operators, repo.allowed_users.as_deref()),
                last_poll_at: health.and_then(|h| h.last_poll_at),
                last_error: health.and_then(|h| h.last_error.clone()),
                dispatched: health.map_or(0, |h| h.dispatched),
                ignored: health.map_or(0, |h| h.ignored),
            }
        })
        .collect();
    GithubWatcherView {
        revision: status.revision,
        enabled: status.config.enabled,
        mention_regex: status.config.mention_regex.clone(),
        label_regex: status.config.label_regex.clone(),
        poll_interval_secs: status.config.poll_interval_secs,
        orchestrator: orchestrator.cloned(),
        operator_users: status.config.operator_users.clone(),
        repositories,
    }
}

/// `archon github-watcher status`.
pub async fn watcher(ctx: &DaemonContext) -> Result<GithubWatcherView, ArchonError> {
    require_dedicated(ctx)?;
    let status = ctx.aether.client()?.github_watcher().await?;
    let orchestrator = supervisor::current_orchestrator(ctx).await?;
    Ok(view(&status, orchestrator.as_ref()))
}

/// The watcher for `archon status`: `None` in shared mode, while no
/// repository is watched, and while Aether's link (shown already) is down.
pub async fn status(ctx: &DaemonContext) -> Option<GithubWatch> {
    if ctx.config.aether.mode == AetherMode::Shared {
        return None;
    }
    match watcher(ctx).await {
        Ok(view) if view.repositories.is_empty() => None,
        Ok(view) => Some(GithubWatch::Watching(Box::new(view))),
        Err(_) if !ctx.aether.is_connected() => None,
        Err(error) => Some(GithubWatch::Unreachable {
            error: error.to_string(),
        }),
    }
}

/// The routes (repository, target) `config` delivers on: enabled
/// repositories with a fixed target, and none while the watcher is off.
fn routes(config: &WatcherConfig) -> BTreeSet<(String, AetherAgentId)> {
    if !config.enabled {
        return BTreeSet::new();
    }
    config
        .repositories
        .iter()
        .filter(|r| r.enabled && r.is_fixed())
        .filter_map(|r| Some((r.repository.to_ascii_lowercase(), r.agent.clone()?)))
        .collect()
}

/// Every agent `config` hands requests to.
fn delivering(config: &WatcherConfig) -> BTreeSet<AetherAgentId> {
    routes(config).into_iter().map(|(_, agent)| agent).collect()
}

/// Agents that `after` hands requests to and `before` did not: a route
/// (repository, target) that delivers now and did not before.
fn newly_delivering(before: &WatcherConfig, after: &WatcherConfig) -> BTreeSet<AetherAgentId> {
    let old = routes(before);
    routes(after)
        .into_iter()
        .filter(|route| !old.contains(route))
        .map(|(_, agent)| agent)
        .collect()
}

/// Refuse a write from `before` to `after` that would hand requests to an
/// agent started without the GitHub policy.
async fn check_targets(
    ctx: &DaemonContext,
    before: &WatcherConfig,
    after: &WatcherConfig,
) -> Result<(), ArchonError> {
    for agent in newly_delivering(before, after) {
        supervisor::require_github_policy(ctx, &agent).await?;
    }
    Ok(())
}

/// Refuse to set operator logins while `config` hands requests to an agent
/// started without the GitHub policy: only it says what the mark allows.
async fn check_operator_targets(
    ctx: &DaemonContext,
    config: &WatcherConfig,
) -> Result<(), ArchonError> {
    debug_assert!(
        config.operator_users.is_some(),
        "clearing the list is never held back"
    );
    for agent in delivering(config) {
        supervisor::require_github_policy(ctx, &agent)
            .await
            .map_err(|error| error.with_detail(OPERATORS_NOT_SET))?;
    }
    Ok(())
}

/// One operator edit, as a compare-and-swap on the revision it read: an
/// edit made meanwhile through `archon aether` fails this one visibly.
/// Archon's repositories are repointed in the same write, and no write
/// hands requests to an orchestrator without the GitHub policy, nor sets
/// operator logins while one takes requests. A list aetherd cannot store is
/// refused before anything else happens.
pub async fn configure(
    ctx: &DaemonContext,
    change: WatcherChange,
) -> Result<GithubWatcherView, ArchonError> {
    require_dedicated(ctx)?;
    check_storable(ctx, &change)?;
    if enables(&change) {
        prepare_enable(ctx).await?;
    }
    configure_existing(ctx, change).await
}

pub(crate) fn enables(change: &WatcherChange) -> bool {
    matches!(
        change,
        WatcherChange::Add { .. } | WatcherChange::Enable { .. }
    )
}

/// Lifecycle changes precede both the orchestrator read guard and the
/// authority settings mutex; neither may be held while ensuring a root.
pub(crate) async fn prepare_enable(ctx: &DaemonContext) -> Result<(), ArchonError> {
    require_dedicated(ctx)?;
    supervisor::ensure_orchestrator(ctx).await.map(drop)
}

/// Configure while authority is held, without creating or replacing its root.
pub(crate) async fn configure_existing(
    ctx: &DaemonContext,
    change: WatcherChange,
) -> Result<GithubWatcherView, ArchonError> {
    require_dedicated(ctx)?;
    check_storable(ctx, &change)?;
    let enabling = matches!(
        change,
        WatcherChange::Add { .. } | WatcherChange::Enable { .. }
    );
    let setting_operators = sets_operators(&change);
    let orchestrator = supervisor::current_orchestrator(ctx).await?;
    if enabling && orchestrator.is_none() {
        return Err(ArchonError::Unavailable(
            "start the orchestrator before enabling its GitHub watcher".into(),
        ));
    }
    let _serial = ctx.github.serial.lock().await;
    let client = ctx.aether.client()?;
    let current = client.github_watcher().await?;
    let mut config = current.config.clone();
    apply_change(&mut config, change, orchestrator.as_ref())?;
    if let Some(orchestrator) = &orchestrator {
        let mut repointed = config.clone();
        if repoint(&mut repointed, orchestrator) {
            match check_targets(ctx, &current.config, &repointed).await {
                Ok(()) => config = repointed,
                Err(error) if enabling => return Err(error),
                // Incidental to this edit, which still applies; the repoint
                // check retries it and status shows the old target meanwhile.
                Err(error) => {
                    tracing::warn!(%error, "left the GitHub watcher's targets as they were")
                }
            }
        }
    }
    check_targets(ctx, &current.config, &config).await?;
    if setting_operators {
        check_operator_targets(ctx, &config).await?;
    }
    let operators = config.operator_users.clone();
    let updated = client
        .configure_github_watcher(config, current.revision)
        .await?;
    check_operators_stored(operators.as_deref(), &updated.config)?;
    tracing::info!(revision = updated.revision, "configured the GitHub watcher");
    Ok(view(&updated, orchestrator.as_ref()))
}

/// Refuse a list this aetherd's protocol cannot store, before any write.
fn check_storable(ctx: &DaemonContext, change: &WatcherChange) -> Result<(), ArchonError> {
    if sets_allowlist(change) {
        ctx.aether.client()?.check_allowlists()?;
    }
    if sets_operators(change) {
        ctx.aether.client()?.check_operator_users()?;
    }
    Ok(())
}

/// Whether `change` sets operator logins; clearing them is not setting.
fn sets_operators(change: &WatcherChange) -> bool {
    matches!(
        change,
        WatcherChange::Operators {
            operator_users: Some(_)
        }
    )
}

/// Aether must answer a write with exactly the operator logins `sent`: a
/// list lost or widened on the way is an error, never a silent change.
fn check_operators_stored(
    sent: Option<&[String]>,
    stored: &WatcherConfig,
) -> Result<(), ArchonError> {
    let logins = |list: Option<&[String]>| -> Option<BTreeSet<String>> {
        list.map(|users| users.iter().map(|user| user.to_ascii_lowercase()).collect())
    };
    if logins(sent) == logins(stored.operator_users.as_deref()) {
        return Ok(());
    }
    Err(ArchonError::Protocol(format!(
        "Aether answered the watcher write with operator logins {:?}, not the {sent:?} Archon \
         sent; check `archon github-watcher status`",
        stored.operator_users
    )))
}

fn sets_allowlist(change: &WatcherChange) -> bool {
    matches!(
        change,
        WatcherChange::Add {
            allowed_users: Some(_),
            ..
        } | WatcherChange::Allow {
            allowed_users: Some(_),
            ..
        }
    )
}

/// Point Archon's repositories at the current orchestrator if any are
/// not; whether a write was needed. Nothing is repointed to an
/// orchestrator started without the GitHub policy while requests flow.
pub async fn repoint_now(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    require_dedicated(ctx)?;
    let Some(orchestrator) = supervisor::current_orchestrator(ctx).await? else {
        return Ok(false);
    };
    let _serial = ctx.github.serial.lock().await;
    let client = ctx.aether.client()?;
    let current = client.github_watcher().await?;
    let mut config = current.config.clone();
    if !repoint(&mut config, &orchestrator) {
        return Ok(false);
    }
    check_targets(ctx, &current.config, &config).await?;
    let operators = config.operator_users.clone();
    let updated = client
        .configure_github_watcher(config, current.revision)
        .await?;
    check_operators_stored(operators.as_deref(), &updated.config)?;
    if view(&updated, Some(&orchestrator)).stale().next().is_some() {
        return Err(ArchonError::Protocol(
            "Aether did not keep the orchestrator as the GitHub target".into(),
        ));
    }
    tracing::info!(%orchestrator, "pointed the GitHub watcher at the orchestrator");
    Ok(true)
}

/// Ask [`watch_github`] to check now (after a connect to Aether).
pub fn check_github_soon(ctx: &DaemonContext) {
    ctx.github.check.notify_one();
}

/// Repoints on every orchestrator change, every [`check_github_soon`] and
/// every [`CHECK_INTERVAL`]. Only a store failure ends it; any other failure
/// is logged and the next check tries again. Idle in shared mode.
pub async fn watch_github(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    if ctx.config.aether.mode == AetherMode::Shared {
        return Ok(());
    }
    let mut orchestrator = ctx.supervisor.watch();
    loop {
        tokio::select! {
            // The sender lives in `ctx`, so `changed` cannot fail here.
            _ = orchestrator.changed() => {}
            () = ctx.github.check.notified() => {}
            () = tokio::time::sleep(CHECK_INTERVAL) => {}
        }
        match repoint_now(&ctx).await {
            Ok(changed) => tracing::debug!(changed, "checked the GitHub watcher's targets"),
            Err(error) if error.is_store() => return Err(error),
            Err(error) if !ctx.aether.is_connected() => {
                tracing::debug!(%error, "GitHub watcher check skipped: Aether is not connected");
            }
            Err(error) => {
                tracing::warn!(%error, "pointing the GitHub watcher at the orchestrator failed")
            }
        }
    }
}

#[cfg(test)]
mod tests;
