//! Tearing down the Docker Compose stacks stage agents start in item
//! worktrees. A stack outlives the agent that started it, and a restart
//! policy brings it back after every reboot, so archond stops what is left
//! when a run ends, when a fresh retry sets the worktree aside, and before
//! the worktree is removed: `down`, never with `--volumes`. Docker cannot
//! tell whose a volume is, as any checkout may reuse a project's name, so
//! volumes are always kept.
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

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use tokio::process::Command;

use crate::context::DaemonContext;

const PROJECT_LABEL: &str = "com.docker.compose.project";
/// Listing answers promptly; a teardown waits for its containers to stop.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWN_TIMEOUT: Duration = Duration::from_secs(180);

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
/// their volumes. `when` completes the feed note of a failure. `Err` only
/// for a store failure.
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
    let failures = stop_projects(&docker, item, &selection.inside).await;
    report_failures(ctx, item, tree, when, &failures).await
}

/// `down` each of `projects`; the failures.
async fn stop_projects(
    docker: &Docker<'_>,
    item: &QueueItemId,
    projects: &BTreeMap<String, PathBuf>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (project, dir) in projects {
        assert!(dir.is_absolute(), "compose records absolute directories");
        tracing::info!(%project, %item, dir = %dir.display(), "tearing down a compose stack an item left");
        if let Err(error) = docker.down(project).await {
            failures.push(format!("{project}: {error}"));
        }
    }
    assert!(failures.len() <= projects.len(), "one note per project");
    failures
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
