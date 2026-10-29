//! Tearing down the Docker Compose stacks stage agents start in item
//! worktrees. A stack outlives the agent that started it, and a restart
//! policy brings it back after every reboot, so archond stops what is left
//! when a run ends (`down`, keeping volumes for a retry), when a fresh retry
//! sets the worktree aside, and when the worktree is removed (`down
//! --volumes`, with the volumes of stacks stopped earlier).
//!
//! A project is Archon's when one of its containers' config files or
//! working directory lies inside the worktree, which lies inside Archon's
//! worktree root. One with any container from elsewhere is never touched.
//! Without Docker, or with its daemon unreachable, nothing happens; a failed
//! teardown is logged and told in the feed, and never fails the item.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use tokio::process::Command;

use crate::context::DaemonContext;

/// `kv`: per item, the projects stopped with their volumes kept, and the
/// directory each ran from, so removing that directory removes them too.
const KV_PREFIX: &str = "compose:";
const PROJECT_LABEL: &str = "com.docker.compose.project";
/// Listing answers promptly; a teardown waits for its containers to stop.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWN_TIMEOUT: Duration = Duration::from_secs(180);

/// What a teardown does with the project's named volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volumes {
    /// A retry may run in the same worktree and reuse its data.
    Keep,
    /// The worktree is gone, and nothing can reuse them.
    Remove,
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
/// inside when every place it names lies inside one of `trees`.
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
            continue;
        };
        let within = |place: &PathBuf| trees.iter().any(|tree| is_within(place, tree));
        if container.places().all(within) {
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

/// `path` is `root` or below it, with no `..` that could climb out.
fn is_within(path: &Path, root: &Path) -> bool {
    let plain = path
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    plain && path.starts_with(root)
}

/// Tear down the Compose projects of `item` running from `tree`; with
/// [`Volumes::Remove`] also the volumes of those an earlier teardown kept.
/// `when` completes the feed note of a failure. `Err` only for a store
/// failure.
pub async fn tear_down(
    ctx: &DaemonContext,
    item: &QueueItemId,
    tree: &Path,
    volumes: Volumes,
    when: &str,
) -> Result<(), ArchonError> {
    if !in_worktree_root(&ctx.paths.worktrees, tree) {
        tracing::debug!(tree = %tree.display(), "not an Archon worktree; its stacks are left alone");
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
    let mut failures = Vec::new();
    for (project, dir) in &selection.inside {
        tracing::info!(%project, %item, dir = %dir.display(), "tearing down a compose stack an item left");
        match docker.down(project, volumes).await {
            Ok(()) if volumes == Volumes::Keep => remember(ctx, item, project, dir).await?,
            Ok(()) => {}
            Err(error) => failures.push(format!("{project}: {error}")),
        }
    }
    if volumes == Volumes::Remove {
        let listed = listed_projects(&listing);
        failures.extend(remove_kept_volumes(ctx, &docker, item, tree, &listed).await?);
    }
    report_failures(ctx, item, tree, when, &failures).await
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

/// `tree` as given and, while it exists, its real path: Compose records the
/// directory an agent ran it in, which may be either.
fn trees_of(tree: &Path) -> Vec<PathBuf> {
    let mut trees = vec![tree.to_path_buf()];
    if let Ok(real) = tree.canonicalize()
        && real != tree
    {
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
) -> Result<BTreeMap<String, PathBuf>, ArchonError> {
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
    kept: &BTreeMap<String, PathBuf>,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    if kept.is_empty() {
        return store.kv_delete(&key(item));
    }
    let raw = serde_json::to_string(kept).expect("paths and names serialize");
    store.kv_set(&key(item), &raw)
}

/// Note that `project`, run from `dir`, was stopped with its volumes kept.
async fn remember(
    ctx: &DaemonContext,
    item: &QueueItemId,
    project: &str,
    dir: &Path,
) -> Result<(), ArchonError> {
    assert!(!project.is_empty(), "compose projects are named");
    let mut kept = kept_projects(ctx, item).await?;
    kept.insert(project.to_owned(), dir.to_path_buf());
    store_kept(ctx, item, &kept).await
}

/// Remove the volumes of the projects of `item` stopped earlier from inside
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
        .filter(|(_, dir)| is_within(dir, tree))
        .map(|(project, _)| project.clone())
        .collect();
    let mut failures = Vec::new();
    for project in gone {
        if listed.contains(&project) {
            tracing::warn!(%project, %item, "a compose project of the same name runs again; its volumes are left alone");
            kept.remove(&project);
            continue;
        }
        match docker.remove_volumes(&project).await {
            Ok(()) => {
                kept.remove(&project);
            }
            Err(error) => failures.push(format!("{project}'s volumes: {error}")),
        }
    }
    store_kept(ctx, item, &kept).await?;
    Ok(failures)
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

    async fn down(&self, project: &str, volumes: Volumes) -> Result<(), ArchonError> {
        let mut args = vec![
            "compose",
            "--project-name",
            project,
            "down",
            "--remove-orphans",
        ];
        if volumes == Volumes::Remove {
            args.push("--volumes");
        }
        self.run(&args, DOWN_TIMEOUT).await.map(drop)
    }

    /// Remove the named volumes Compose labelled with `project`.
    async fn remove_volumes(&self, project: &str) -> Result<(), ArchonError> {
        let filter = format!("label={PROJECT_LABEL}={project}");
        let listed = self
            .run(
                &["volume", "ls", "--quiet", "--filter", &filter],
                LIST_TIMEOUT,
            )
            .await?;
        let names: Vec<&str> = listed.split_whitespace().collect();
        if names.is_empty() {
            return Ok(());
        }
        let mut args = vec!["volume", "rm"];
        args.extend(names);
        self.run(&args, DOWN_TIMEOUT).await.map(drop)
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
                       nested\t\t/data/worktrees/app/qi_1/services/db\n";
        let selection = select(listing, &trees());
        assert_eq!(selection.shared, vec!["app".to_owned()]);
        let dir = PathBuf::from("/data/worktrees/app/qi_1/services/db");
        assert_eq!(
            selection.inside,
            BTreeMap::from([("nested".to_owned(), dir)])
        );
    }
}
