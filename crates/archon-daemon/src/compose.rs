//! Tearing down the Docker Compose stacks stage agents start in item
//! worktrees. A stack outlives the agent that started it, and a restart
//! policy brings it back after every reboot, so archond stops what is left
//! when a run ends (`down`, keeping volumes for a retry), when a fresh retry
//! sets the worktree aside, and before the worktree is removed. Once it is,
//! the named volumes its stacks made go too: only of a project named after
//! the item, created after the item was, still the same and mounted by none.
//!
//! A project is Archon's when one of its containers' config files or
//! working directory lies inside the worktree, also once symlinks are
//! resolved, which lies inside Archon's worktree root. One with any
//! container from elsewhere is never touched.
//! Without Docker, or with its daemon unreachable, nothing happens; a failed
//! teardown is logged and told in the feed, and never fails the item.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::context::DaemonContext;

/// `kv`: per item, the projects stopped with their volumes kept, each with
/// the directory it ran from, so removing that directory removes them too.
const KV_PREFIX: &str = "compose:";
const PROJECT_LABEL: &str = "com.docker.compose.project";
/// Listing answers promptly; a teardown waits for its containers to stop.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWN_TIMEOUT: Duration = Duration::from_secs(180);

/// A project stopped with its volumes kept.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    dir: PathBuf,
    /// Each named volume it made, with the creation time Docker gave it: a
    /// volume of that name created anew since is someone else's.
    volumes: BTreeMap<String, String>,
}

/// The Compose projects with a container inside one of `trees`, and the
/// directory each runs from; those that also run from elsewhere are left
/// out and returned apart.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub inside: BTreeMap<String, PathBuf>,
    pub shared: Vec<String>,
}

/// Pick Archon's projects from `docker ps` lines of `project`, comma-joined
/// config files and working directory, tab-separated. A container counts as
/// inside when every place it names lies inside one of `trees`, as named and
/// once resolved; one naming no place may run from anywhere.
pub fn select(listing: &str, trees: &[PathBuf]) -> Selection {
    assert!(!trees.is_empty(), "a teardown names its worktree");
    assert!(
        trees.iter().all(|t| t.is_absolute()),
        "worktrees are absolute"
    );
    let mut inside: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut outside = BTreeSet::new();
    for container in listing.lines().filter_map(Container::parse) {
        let Some(dir) = container.dir() else {
            outside.insert(container.project.to_owned());
            continue;
        };
        if container.places().all(|place| lies_within(place, trees)) {
            inside.entry(container.project.to_owned()).or_insert(dir);
        } else {
            outside.insert(container.project.to_owned());
        }
    }
    let shared: Vec<String> = outside
        .into_iter()
        .filter(|p| inside.contains_key(p))
        .collect();
    inside.retain(|project, _| !shared.contains(project));
    assert!(
        shared.iter().all(|p| !inside.contains_key(p)),
        "a shared project is never inside"
    );
    Selection { inside, shared }
}

/// One line of the `docker ps` listing.
struct Container<'a> {
    project: &'a str,
    configs: Vec<PathBuf>,
    working_dir: Option<PathBuf>,
}

impl<'a> Container<'a> {
    /// `None` for a line no project labels.
    fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split('\t');
        let project = fields.next()?.trim();
        if project.is_empty() {
            return None;
        }
        let configs = fields.next().unwrap_or_default().split(',');
        let configs = configs.map(str::trim).filter(|c| !c.is_empty());
        let working_dir = fields.next().map(str::trim).filter(|w| !w.is_empty());
        Some(Self {
            project,
            configs: configs.map(PathBuf::from).collect(),
            working_dir: working_dir.map(PathBuf::from),
        })
    }

    fn places(&self) -> impl Iterator<Item = &PathBuf> {
        self.configs.iter().chain(&self.working_dir)
    }

    /// The directory it runs from: its working directory, else its first
    /// config file's; `None` when it names no place.
    fn dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.working_dir {
            return Some(dir.clone());
        }
        let config = self.configs.first()?;
        Some(config.parent().unwrap_or(config).to_path_buf())
    }
}

/// Every project the `docker ps` listing names, wherever it runs from.
fn listed_projects(listing: &str) -> BTreeSet<String> {
    let containers = listing.lines().filter_map(Container::parse);
    containers.map(|c| c.project.to_owned()).collect()
}

/// `place` lies inside one of `trees` both as named and with its existing
/// part resolved, so a symlink in a worktree cannot vouch for elsewhere.
fn lies_within(place: &Path, trees: &[PathBuf]) -> bool {
    let named = trees.iter().any(|tree| is_within(place, tree));
    named && trees.iter().any(|tree| is_within(&resolved(place), tree))
}

/// `path` with its longest existing part replaced by its real path; the
/// rest, which no symlink can redirect yet, is kept as named.
fn resolved(path: &Path) -> PathBuf {
    assert!(path.is_absolute(), "compose records absolute paths");
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            assert!(real.is_absolute(), "a real path is absolute");
            return rest.iter().rev().fold(real, |path, name| path.join(name));
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return path.to_path_buf();
        };
        rest.push(name);
        existing = parent;
    }
}

/// `path` is `root` or below it, with no `..` that could climb out.
fn is_within(path: &Path, root: &Path) -> bool {
    let plain = path
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    plain && path.starts_with(root)
}

/// Tear down the Compose projects of `item` running from `tree`, keeping
/// their volumes and noting those each made. `when` completes the feed note
/// of a failure. `Err` only for a store failure.
pub async fn tear_down(
    ctx: &DaemonContext,
    item: &QueueItemId,
    tree: &Path,
    when: &str,
) -> Result<(), ArchonError> {
    if !in_worktree_root(&ctx.paths.worktrees, tree) {
        tracing::debug!(tree = %tree.display(), "not an Archon worktree; its stacks are left alone");
        return Ok(());
    }
    // A removed worktree no longer shows where its symlinks led.
    if !tree.is_dir() {
        tracing::debug!(tree = %tree.display(), "the worktree is gone; its stacks are left alone");
        return Ok(());
    }
    let docker = Docker::new(&ctx.config.worktrees.docker);
    let Some(listing) = docker.list_projects().await else {
        return Ok(());
    };
    let selection = select(&listing, &trees_of(tree));
    for project in &selection.shared {
        tracing::warn!(%project, tree = %tree.display(), "a compose project also runs from outside the worktree; left running");
    }
    let failures = stop_projects(ctx, &docker, item, &selection.inside).await?;
    report_failures(ctx, item, tree, when, &failures).await
}

/// Remove the volumes noted for the stacks of `item` stopped in `tree`,
/// which is gone; nothing is stopped, as nothing shows where its symlinks
/// led. `Err` only for a store failure.
pub async fn remove_volumes(
    ctx: &DaemonContext,
    item: &QueueItemId,
    tree: &Path,
    when: &str,
) -> Result<(), ArchonError> {
    if !in_worktree_root(&ctx.paths.worktrees, tree) {
        return Ok(());
    }
    if kept_projects(ctx, item).await?.is_empty() {
        return Ok(());
    }
    let docker = Docker::new(&ctx.config.worktrees.docker);
    let Some(listing) = docker.list_projects().await else {
        return Ok(());
    };
    let listed = listed_projects(&listing);
    let failures = remove_kept_volumes(ctx, &docker, item, tree, &listed).await?;
    report_failures(ctx, item, tree, when, &failures).await
}

/// `down` each of `projects` and note the volumes it made; the failures.
async fn stop_projects(
    ctx: &DaemonContext,
    docker: &Docker<'_>,
    item: &QueueItemId,
    projects: &BTreeMap<String, PathBuf>,
) -> Result<Vec<String>, ArchonError> {
    let mut failures = Vec::new();
    if projects.is_empty() {
        return Ok(failures);
    }
    let since = item_created(ctx, item).await?;
    for (project, dir) in projects {
        tracing::info!(%project, %item, dir = %dir.display(), "tearing down a compose stack an item left");
        if let Err(error) = docker.down(project).await {
            failures.push(format!("{project}: {error}"));
            continue;
        }
        match made_volumes(docker, item, project, since).await {
            Ok(volumes) => remember(ctx, item, project, dir, volumes).await?,
            Err(error) => failures.push(format!("{project}'s volumes: {error}")),
        }
    }
    assert!(failures.len() <= projects.len(), "one note per project");
    Ok(failures)
}

/// Whether `project` is named after `item`, as Compose names one run from
/// the worktree's root: no checkout elsewhere shares that name, or its volumes.
fn named_after(project: &str, item: &QueueItemId) -> bool {
    assert!(!item.as_str().is_empty(), "items are named");
    project.contains(&item.as_str().to_lowercase())
}

/// When `item` was created; `None` once it is pruned, when no volume can
/// be told to be its own.
async fn item_created(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<Option<Timestamp>, ArchonError> {
    match ctx.store.lock().await.item(item) {
        Ok(found) => Ok(Some(found.created_at)),
        Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The volumes of `project`, if named after `item`, created at or after
/// `since`, with their creation times: an older one is someone else's.
async fn made_volumes(
    docker: &Docker<'_>,
    item: &QueueItemId,
    project: &str,
    since: Option<Timestamp>,
) -> Result<BTreeMap<String, String>, ArchonError> {
    let mut made = BTreeMap::new();
    if !named_after(project, item) {
        tracing::info!(%project, %item, "a project not named after its item keeps its volumes");
        return Ok(made);
    }
    let Some(since) = since else {
        return Ok(made);
    };
    for name in docker.project_volumes(project).await? {
        let created = docker.volume_created(&name).await?;
        match chrono::DateTime::parse_from_rfc3339(&created) {
            Ok(at) if at >= since => {
                made.insert(name, created);
            }
            Ok(_) => {
                tracing::info!(volume = %name, %project, "a volume older than the item is left alone")
            }
            Err(error) => {
                tracing::warn!(volume = %name, %created, %error, "a volume of unknown age is left alone")
            }
        }
    }
    assert!(
        made.keys().all(|name| !name.is_empty()),
        "volumes are named"
    );
    Ok(made)
}

/// The `docker ps` listing [`select`] reads, of every Compose container;
/// `None` when `docker` is missing or its daemon unreachable.
pub async fn list(docker: &Path) -> Option<String> {
    Docker::new(docker).list_projects().await
}

/// The command that stops `project` as a teardown at a run's end does.
pub fn down_command(project: &str) -> String {
    assert!(!project.is_empty(), "compose projects are named");
    format!("docker compose --project-name {project} down --remove-orphans")
}

/// `tree` lies strictly inside `root`, as given or as its real path.
fn in_worktree_root(root: &Path, tree: &Path) -> bool {
    let inside = |root: &Path| tree != root && is_within(tree, root);
    inside(root) || root.canonicalize().is_ok_and(|real| inside(&real))
}

/// `tree` as given and as resolved: Compose records the directory an agent
/// ran it in, which may be either.
fn trees_of(tree: &Path) -> Vec<PathBuf> {
    let mut trees = vec![tree.to_path_buf()];
    let real = resolved(tree);
    if real != tree {
        trees.push(real);
    }
    trees
}

async fn report_failures(
    ctx: &DaemonContext,
    item: &QueueItemId,
    tree: &Path,
    when: &str,
    failures: &[String],
) -> Result<(), ArchonError> {
    if failures.is_empty() {
        return Ok(());
    }
    let detail = failures.join("; ");
    tracing::warn!(%item, tree = %tree.display(), %detail, "a compose teardown failed");
    let text = format!(
        "Could not tear down the Docker Compose stack {item} left in {} {when}: {detail}. \
         `archon doctor` lists what still runs.",
        tree.display()
    );
    crate::feed::system(ctx, text).await.map(drop)
}

fn key(item: &QueueItemId) -> String {
    format!("{KV_PREFIX}{item}")
}

async fn kept_projects(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<BTreeMap<String, Kept>, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(&key(item))?;
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("{} holds no compose projects: {e}", key(item))))
}

async fn store_kept(
    ctx: &DaemonContext,
    item: &QueueItemId,
    kept: &BTreeMap<String, Kept>,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    if kept.is_empty() {
        return store.kv_delete(&key(item));
    }
    let raw = serde_json::to_string(kept).expect("paths and names serialize");
    store.kv_set(&key(item), &raw)
}

/// Note that `project`, run from `dir`, was stopped with `volumes` kept.
async fn remember(
    ctx: &DaemonContext,
    item: &QueueItemId,
    project: &str,
    dir: &Path,
    volumes: BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    assert!(!project.is_empty(), "compose projects are named");
    assert!(dir.is_absolute(), "compose records absolute directories");
    let mut kept = kept_projects(ctx, item).await?;
    let entry = kept.entry(project.to_owned()).or_default();
    entry.dir = dir.to_path_buf();
    entry.volumes.extend(volumes);
    store_kept(ctx, item, &kept).await
}

/// Note that the kept stacks of `item` that ran inside `from` now lie in
/// `to`, where a fresh retry moved its worktree; their volumes then go
/// with `to`, not with a new worktree made at `from`.
pub async fn moved(
    ctx: &DaemonContext,
    item: &QueueItemId,
    from: &Path,
    to: &Path,
) -> Result<(), ArchonError> {
    assert!(
        from.is_absolute() && to.is_absolute(),
        "worktrees are absolute"
    );
    assert_ne!(from, to, "a move changes the directory");
    // `from` is gone; Compose may have recorded it through its real parent.
    let real_from = resolved(from);
    let mut kept = kept_projects(ctx, item).await?;
    for project in kept.values_mut() {
        let dir = &project.dir;
        let below = dir
            .strip_prefix(from)
            .or_else(|_| dir.strip_prefix(&real_from));
        if let Ok(below) = below {
            project.dir = to.join(below);
        }
    }
    store_kept(ctx, item, &kept).await
}

/// Remove the volumes noted for the projects of `item` stopped from inside
/// `tree`; the failures. A project `listed` with a container again, even
/// one from elsewhere that reuses its name, may own them now: it is forgotten.
async fn remove_kept_volumes(
    ctx: &DaemonContext,
    docker: &Docker<'_>,
    item: &QueueItemId,
    tree: &Path,
    listed: &BTreeSet<String>,
) -> Result<Vec<String>, ArchonError> {
    let mut kept = kept_projects(ctx, item).await?;
    let gone: Vec<String> = kept
        .iter()
        .filter(|(_, project)| is_within(&project.dir, tree))
        .map(|(name, _)| name.clone())
        .collect();
    let mut failures = Vec::new();
    for project in gone {
        if listed.contains(&project) {
            tracing::warn!(%project, %item, "a compose project of the same name runs again; its volumes are left alone");
            kept.remove(&project);
            continue;
        }
        let volumes = &kept[&project].volumes;
        match remove_made_volumes(docker, &project, volumes).await {
            Ok(()) => {
                kept.remove(&project);
            }
            Err(error) => failures.push(format!("{project}'s volumes: {error}")),
        }
    }
    store_kept(ctx, item, &kept).await?;
    Ok(failures)
}

/// Remove each of `volumes` of `project` that is still the volume noted,
/// created when it was, and that no container uses.
async fn remove_made_volumes(
    docker: &Docker<'_>,
    project: &str,
    volumes: &BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    if volumes.is_empty() {
        return Ok(());
    }
    let present = docker.project_volumes(project).await?;
    for (name, created) in volumes {
        if !present.contains(name) {
            continue;
        }
        if docker.volume_created(name).await? != *created {
            tracing::warn!(volume = %name, %project, "a volume was made anew under the same name; left alone");
            continue;
        }
        if docker.volume_in_use(name).await? {
            tracing::warn!(volume = %name, %project, "a container uses the volume; left alone");
            continue;
        }
        docker.remove_volume(name).await?;
    }
    Ok(())
}

/// The `docker` CLI, each call bounded.
struct Docker<'a> {
    binary: &'a Path,
}

impl<'a> Docker<'a> {
    fn new(binary: &'a Path) -> Self {
        Self { binary }
    }

    /// Every Compose container's project, config files and working
    /// directory; `None` when Docker is missing or its daemon unreachable.
    async fn list_projects(&self) -> Option<String> {
        let format = format!(
            "{{{{.Label \"{PROJECT_LABEL}\"}}}}\t{{{{.Label \"{PROJECT_LABEL}.config_files\"}}}}\t{{{{.Label \"{PROJECT_LABEL}.working_dir\"}}}}"
        );
        let filter = format!("label={PROJECT_LABEL}");
        let args = ["ps", "--all", "--filter", &filter, "--format", &format];
        match self.run(&args, LIST_TIMEOUT).await {
            Ok(listing) => Some(listing),
            Err(error) => {
                tracing::debug!(%error, "docker is not available; no compose stack is looked for");
                None
            }
        }
    }

    async fn down(&self, project: &str) -> Result<(), ArchonError> {
        let args = [
            "compose",
            "--project-name",
            project,
            "down",
            "--remove-orphans",
        ];
        self.run(&args, DOWN_TIMEOUT).await.map(drop)
    }

    /// The named volumes Compose labelled with `project`.
    async fn project_volumes(&self, project: &str) -> Result<BTreeSet<String>, ArchonError> {
        let filter = format!("label={PROJECT_LABEL}={project}");
        let args = ["volume", "ls", "--quiet", "--filter", &filter];
        let listed = self.run(&args, LIST_TIMEOUT).await?;
        Ok(listed.split_whitespace().map(str::to_owned).collect())
    }

    /// When Docker created volume `name`, as it prints it.
    async fn volume_created(&self, name: &str) -> Result<String, ArchonError> {
        let args = ["volume", "inspect", "--format", "{{.CreatedAt}}", name];
        let created = self.run(&args, LIST_TIMEOUT).await?;
        let created = created.trim();
        if created.is_empty() {
            return Err(ArchonError::Unavailable(format!(
                "docker gave no creation time for volume {name}"
            )));
        }
        Ok(created.to_owned())
    }

    /// Whether any container, running or stopped, mounts volume `name`.
    async fn volume_in_use(&self, name: &str) -> Result<bool, ArchonError> {
        let filter = format!("volume={name}");
        let args = ["ps", "--quiet", "--all", "--filter", &filter];
        let users = self.run(&args, LIST_TIMEOUT).await?;
        Ok(!users.trim().is_empty())
    }

    async fn remove_volume(&self, name: &str) -> Result<(), ArchonError> {
        assert!(!name.is_empty(), "volumes are named");
        self.run(&["volume", "rm", name], DOWN_TIMEOUT)
            .await
            .map(drop)
    }

    /// stdout of a successful call; a failure carries stderr.
    async fn run(&self, args: &[&str], limit: Duration) -> Result<String, ArchonError> {
        assert!(!args.is_empty(), "docker always takes a command");
        let context = || format!("{} {}", self.binary.display(), args.join(" "));
        let child = Command::new(self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(limit, child)
            .await
            .map_err(|_| {
                ArchonError::Unavailable(format!("{} timed out after {limit:?}", context()))
            })?
            .map_err(|e| ArchonError::io(context(), e))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(ArchonError::Unavailable(format!(
            "{} failed ({}): {}",
            context(),
            output.status,
            stderr.trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trees() -> Vec<PathBuf> {
        vec![PathBuf::from("/data/worktrees/app/qi_1")]
    }

    #[test]
    fn a_project_whose_config_lies_in_the_worktree_is_selected() {
        let listing = "review-1\t/data/worktrees/app/qi_1/compose.yml,/data/worktrees/app/qi_1/compose.dev.yml\t/data/worktrees/app/qi_1\n\
                       review-1\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n";
        let selection = select(listing, &trees());
        let dir = PathBuf::from("/data/worktrees/app/qi_1");
        assert_eq!(
            selection.inside,
            BTreeMap::from([("review-1".to_owned(), dir)])
        );
        assert!(selection.shared.is_empty());
    }

    #[test]
    fn projects_from_elsewhere_or_a_sibling_worktree_are_left_alone() {
        let listing = "mine\t/home/op/app/compose.yml\t/home/op/app\n\
                       sibling\t/data/worktrees/app/qi_10/compose.yml\t/data/worktrees/app/qi_10\n\
                       climbing\t/data/worktrees/app/qi_1/../../../etc/compose.yml\t\n\
                       \t\t\n";
        let selection = select(listing, &trees());
        assert!(selection.inside.is_empty(), "{selection:?}");
        assert!(selection.shared.is_empty(), "{selection:?}");
    }

    #[test]
    fn a_project_that_also_runs_from_outside_is_never_touched() {
        let listing = "app\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n\
                       app\t/home/op/app/compose.yml\t/home/op/app\n\
                       unplaced\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n\
                       unplaced\t\t\n\
                       nested\t\t/data/worktrees/app/qi_1/services/db\n";
        let selection = select(listing, &trees());
        assert_eq!(
            selection.shared,
            vec!["app".to_owned(), "unplaced".to_owned()]
        );
        let dir = PathBuf::from("/data/worktrees/app/qi_1/services/db");
        assert_eq!(
            selection.inside,
            BTreeMap::from([("nested".to_owned(), dir)])
        );
    }

    #[test]
    fn a_symlink_in_the_worktree_does_not_vouch_for_a_place_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("worktrees/app/qi_1");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&checkout, tree.join("ext")).unwrap();
        let line = |project: &str, dir: &Path| {
            format!(
                "{project}\t{}\t{}\n",
                dir.join("compose.yml").display(),
                dir.display()
            )
        };
        let listing = line("theirs", &tree.join("ext")) + &line("ours", &tree.join("db"));
        let selection = select(&listing, &trees_of(&tree));
        let ours = BTreeMap::from([("ours".to_owned(), tree.join("db"))]);
        assert_eq!(selection.inside, ours);
        assert!(selection.shared.is_empty(), "{selection:?}");
    }
}
