//! Tearing down the Docker Compose stacks stage agents start in item
//! worktrees. A stack outlives the agent that started it, and a restart
//! policy brings it back after every reboot, so archond stops what is left
//! when a run ends, when a fresh retry sets the worktree aside, and before
//! the worktree is removed. Docker cannot tell whose a volume is, as any
//! checkout may reuse a project's name, so volumes are always kept.
//!
//! A project is Archon's when one of its containers' config files or
//! working directory lies inside the worktree, also once symlinks are
//! resolved, which lies inside Archon's worktree root. One with any
//! container from elsewhere is never touched. Ownership is checked again
//! just before a teardown, which then stops and removes only the containers
//! it checked, and the project's networks once none of its containers is left.
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
const STOP_TIMEOUT: Duration = Duration::from_secs(180);

/// A Compose project every container of which runs from inside the
/// worktree: the directory it runs from, and each container's ID with the
/// places it names.
#[derive(Debug, PartialEq, Eq)]
pub struct Project {
    pub dir: PathBuf,
    pub containers: BTreeMap<String, Vec<PathBuf>>,
}

/// The Compose projects with a container inside one of `trees`; those that
/// also run from elsewhere are left out and returned apart.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub inside: BTreeMap<String, Project>,
    pub shared: Vec<String>,
}

/// Pick Archon's projects from `docker ps` lines of container ID, project,
/// comma-joined config files and working directory, tab-separated. A
/// container counts as inside when every place it names lies inside one of
/// `trees`, as named and once resolved; one naming no place may run from
/// anywhere.
pub fn select(listing: &str, trees: &[PathBuf]) -> Selection {
    assert!(!trees.is_empty(), "a teardown names its worktree");
    assert!(
        trees.iter().all(|t| t.is_absolute()),
        "worktrees are absolute"
    );
    let mut inside: BTreeMap<String, Project> = BTreeMap::new();
    let mut outside = BTreeSet::new();
    for container in listing.lines().filter_map(Container::parse) {
        let dir = container.dir();
        let Some(dir) = dir.filter(|_| container.places().all(|p| lies_within(p, trees))) else {
            outside.insert(container.project.to_owned());
            continue;
        };
        let project = inside
            .entry(container.project.to_owned())
            .or_insert_with(|| Project {
                dir,
                containers: BTreeMap::new(),
            });
        let places = container.places().cloned().collect();
        project.containers.insert(container.id.to_owned(), places);
    }
    let shared: Vec<String> = outside
        .into_iter()
        .filter(|p| inside.contains_key(p))
        .collect();
    inside.retain(|project, _| !shared.contains(project));
    assert!(
        inside.values().all(|p| !p.containers.is_empty()),
        "a selected project has a container"
    );
    Selection { inside, shared }
}

/// One line of the `docker ps` listing.
struct Container<'a> {
    id: &'a str,
    project: &'a str,
    configs: Vec<PathBuf>,
    working_dir: Option<PathBuf>,
}

impl<'a> Container<'a> {
    /// `None` for a line no project labels, or whose ID is not the hex one
    /// Docker prints: it goes to `docker stop` as an argument.
    fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split('\t');
        let id = fields.next()?.trim();
        let project = fields.next()?.trim();
        let hex = !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex || project.is_empty() {
            return None;
        }
        let configs = fields.next().unwrap_or_default().split(',');
        let configs = configs.map(str::trim).filter(|c| !c.is_empty());
        let working_dir = fields.next().map(str::trim).filter(|w| !w.is_empty());
        Some(Self {
            id,
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
    if !trees.iter().any(|tree| is_within(place, tree)) {
        return false;
    }
    let Some(real) = resolved(place) else {
        return false;
    };
    trees.iter().any(|tree| is_within(&real, tree))
}

/// `path` with its longest existing part replaced by its real path; the
/// rest, which no symlink can redirect yet, is kept as named. `None` unless
/// each part it skips is plainly absent: a dangling or unreadable one may
/// lead anywhere, and so may a relative path.
fn resolved(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let absent = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(real) => {
                assert!(real.is_absolute(), "a real path is absolute");
                return Some(rest.iter().rev().fold(real, |path, name| path.join(name)));
            }
            Err(error) if !absent(&error) => return None,
            Err(_) => {}
        }
        match existing.symlink_metadata() {
            Err(error) if absent(&error) => {}
            _ => return None,
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return None;
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
    let listing = match docker.list(None).await {
        Ok(listing) => listing,
        Err(error) => {
            tracing::debug!(%error, "docker is not available; no compose stack is looked for");
            return Ok(());
        }
    };
    let trees = trees_of(tree);
    let selection = select(&listing, &trees);
    warn_shared(&selection.shared, tree);
    let mut failures = Vec::new();
    for project in selection.inside.keys() {
        tracing::info!(%project, %item, tree = %tree.display(), "tearing down a compose stack an item left");
        if let Err(error) = stop_project(&docker, project, &trees).await {
            failures.push(format!("{project}: {error}"));
        }
    }
    assert!(
        failures.len() <= selection.inside.len(),
        "one note per project"
    );
    report_failures(ctx, item, tree, when, &failures).await
}

fn warn_shared(shared: &[String], tree: &Path) {
    for project in shared {
        tracing::warn!(%project, tree = %tree.display(), "a compose project also runs from outside the worktree; left running");
    }
}

/// List `project` again and, if each of its containers still runs from
/// `trees`, stop and remove those containers, then its networks once none
/// of its containers is left. One that changed since is left running.
async fn stop_project(
    docker: &Docker<'_>,
    project: &str,
    trees: &[PathBuf],
) -> Result<(), ArchonError> {
    let selection = select(&docker.list(Some(project)).await?, trees);
    warn_shared(&selection.shared, &trees[0]);
    let Some(verified) = selection.inside.get(project) else {
        tracing::info!(%project, "the compose project is gone or changed since it was listed; left alone");
        return Ok(());
    };
    assert!(
        verified.dir.is_absolute(),
        "compose records absolute directories"
    );
    let ids: BTreeSet<String> = verified.containers.keys().cloned().collect();
    assert!(!ids.is_empty(), "a selected project has a container");
    let args: Vec<&str> = ids.iter().map(String::as_str).collect();
    docker
        .run(&[&["stop"], &args[..]].concat(), STOP_TIMEOUT)
        .await?;
    remove_stopped(docker, project, &ids).await
}

/// Remove those of `stopped` still there (a `run --rm` one removes itself
/// once stopped), then `project`'s networks if none of its containers is left.
async fn remove_stopped(
    docker: &Docker<'_>,
    project: &str,
    stopped: &BTreeSet<String>,
) -> Result<(), ArchonError> {
    let present = container_ids(&docker.list(Some(project)).await?);
    let ids: Vec<&str> = stopped.intersection(&present).map(String::as_str).collect();
    if !ids.is_empty() {
        docker
            .run(&[&["rm"], &ids[..]].concat(), LIST_TIMEOUT)
            .await?;
    }
    if !container_ids(&docker.list(Some(project)).await?).is_empty() {
        tracing::info!(%project, "other containers of the compose project remain; its networks are kept");
        return Ok(());
    }
    let networks = docker.networks(project).await?;
    if networks.is_empty() {
        return Ok(());
    }
    let networks: Vec<&str> = networks.iter().map(String::as_str).collect();
    docker
        .run(&[&["network", "rm"], &networks[..]].concat(), LIST_TIMEOUT)
        .await
        .map(drop)
}

/// The IDs of the containers in a `docker ps` listing.
fn container_ids(listing: &str) -> BTreeSet<String> {
    let containers = listing.lines().filter_map(Container::parse);
    containers.map(|c| c.id.to_owned()).collect()
}

/// The `docker ps` listing [`select`] reads, of every Compose container;
/// `None` when `docker` is missing or its daemon unreachable.
pub async fn list(docker: &Path) -> Option<String> {
    Docker::new(docker).list(None).await.ok()
}

/// The command an operator runs to stop `project`, whichever checkout runs it.
pub fn down_command(project: &str) -> String {
    assert!(!project.is_empty(), "compose projects are named");
    format!("docker compose --project-name {project} down --remove-orphans")
}

/// `tree` lies strictly inside `root` as given, and still once both are
/// resolved: a symlinked worktree must not lead to a checkout elsewhere.
fn in_worktree_root(root: &Path, tree: &Path) -> bool {
    let inside = |tree: &Path, root: &Path| tree != root && is_within(tree, root);
    let Ok(real_root) = root.canonicalize() else {
        return false;
    };
    let named = inside(tree, root) || inside(tree, &real_root);
    named && resolved(tree).is_some_and(|real| inside(&real, &real_root))
}

/// `tree` as given and as resolved, the `trees` [`select`] takes: Compose
/// records the directory an agent ran it in, which may be either.
pub fn trees_of(tree: &Path) -> Vec<PathBuf> {
    let mut trees = vec![tree.to_path_buf()];
    // One that stopped resolving leaves no place below it resolving either.
    if let Some(real) = resolved(tree)
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

/// The `docker` CLI, each call bounded.
struct Docker<'a> {
    binary: &'a Path,
}

impl<'a> Docker<'a> {
    fn new(binary: &'a Path) -> Self {
        Self { binary }
    }

    /// Each Compose container's ID, project, config files and working
    /// directory; only `project`'s when one is named.
    async fn list(&self, project: Option<&str>) -> Result<String, ArchonError> {
        let format = format!(
            "{{{{.ID}}}}\t{{{{.Label \"{PROJECT_LABEL}\"}}}}\t{{{{.Label \"{PROJECT_LABEL}.config_files\"}}}}\t{{{{.Label \"{PROJECT_LABEL}.working_dir\"}}}}"
        );
        let filter = match project {
            Some(project) => format!("label={PROJECT_LABEL}={project}"),
            None => format!("label={PROJECT_LABEL}"),
        };
        let args = [
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            &filter,
            "--format",
            &format,
        ];
        self.run(&args, LIST_TIMEOUT).await
    }

    /// The IDs of `project`'s networks.
    async fn networks(&self, project: &str) -> Result<Vec<String>, ArchonError> {
        let filter = format!("label={PROJECT_LABEL}={project}");
        let args = ["network", "ls", "--filter", &filter, "--format", "{{.ID}}"];
        let listing = self.run(&args, LIST_TIMEOUT).await?;
        let ids: Vec<String> = listing.split_whitespace().map(str::to_owned).collect();
        if let Some(odd) = ids
            .iter()
            .find(|id| !id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ArchonError::Unavailable(format!(
                "docker network ls printed {odd:?}, not a network ID"
            )));
        }
        Ok(ids)
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

    fn ids(project: &Project) -> Vec<&str> {
        project.containers.keys().map(String::as_str).collect()
    }

    #[test]
    fn a_project_whose_config_lies_in_the_worktree_is_selected() {
        let listing = "a1\treview-1\t/data/worktrees/app/qi_1/compose.yml,/data/worktrees/app/qi_1/compose.dev.yml\t/data/worktrees/app/qi_1\n\
                       b2\treview-1\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n";
        let selection = select(listing, &trees());
        let review = &selection.inside["review-1"];
        assert_eq!(review.dir, Path::new("/data/worktrees/app/qi_1"));
        assert_eq!(ids(review), ["a1", "b2"]);
        assert_eq!(review.containers["b2"].len(), 2, "config and working dir");
        assert_eq!(selection.inside.len(), 1);
        assert!(selection.shared.is_empty());
    }

    #[test]
    fn projects_from_elsewhere_or_a_sibling_worktree_are_left_alone() {
        let listing = "a1\tmine\t/home/op/app/compose.yml\t/home/op/app\n\
                       a2\tsibling\t/data/worktrees/app/qi_10/compose.yml\t/data/worktrees/app/qi_10\n\
                       a3\tclimbing\t/data/worktrees/app/qi_1/../../../etc/compose.yml\t\n\
                       a4\t\t\t\n\
                       --rm\tflagged\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n";
        let selection = select(listing, &trees());
        assert!(selection.inside.is_empty(), "{selection:?}");
        assert!(selection.shared.is_empty(), "{selection:?}");
    }

    #[test]
    fn a_project_that_also_runs_from_outside_is_never_touched() {
        let listing = "a1\tapp\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n\
                       a2\tapp\t/home/op/app/compose.yml\t/home/op/app\n\
                       a3\tunplaced\t/data/worktrees/app/qi_1/compose.yml\t/data/worktrees/app/qi_1\n\
                       a4\tunplaced\t\t\n\
                       a5\tnested\t\t/data/worktrees/app/qi_1/services/db\n";
        let selection = select(listing, &trees());
        assert_eq!(
            selection.shared,
            vec!["app".to_owned(), "unplaced".to_owned()]
        );
        let nested = &selection.inside["nested"];
        assert_eq!(
            nested.dir,
            Path::new("/data/worktrees/app/qi_1/services/db")
        );
        assert_eq!(ids(nested), ["a5"]);
        assert_eq!(selection.inside.len(), 1);
    }

    #[test]
    fn a_dangling_symlink_in_the_worktree_vouches_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("worktrees/app/qi_1");
        std::fs::create_dir_all(&tree).unwrap();
        let gone = dir.path().join("moved-away");
        std::os::unix::fs::symlink(&gone, tree.join("ext")).unwrap();
        let place = tree.join("ext/compose.yml");
        let listing = format!(
            "a1\ttheirs\t{}\t{}\n",
            place.display(),
            tree.join("ext").display()
        );
        let selection = select(&listing, &trees_of(&tree));
        assert!(selection.inside.is_empty(), "{selection:?}");
        assert_eq!(resolved(&place), None);
        assert_eq!(
            resolved(&tree.join("db/x")),
            Some(tree.canonicalize().unwrap().join("db/x"))
        );
    }

    #[test]
    fn an_unreadable_or_relative_place_is_never_inside() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("worktrees/app/qi_1");
        let locked = tree.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::os::unix::fs::symlink(dir.path(), locked.join("out")).unwrap();
        let mode = |bits| std::fs::Permissions::from_mode(bits);
        std::fs::set_permissions(&locked, mode(0o000)).unwrap();
        let place = locked.join("out/compose.yml");
        let readable = std::fs::symlink_metadata(&place).is_ok();
        let unreadable = resolved(&place);
        std::fs::set_permissions(&locked, mode(0o755)).unwrap();
        // Root reads through any permission; then the link itself resolves out.
        if !readable {
            assert_eq!(unreadable, None);
        }
        assert!(!lies_within(&place, &trees_of(&tree)));
        let relative = "a1\tapp\tcompose.yml\tsome/dir\n";
        assert!(select(relative, &trees_of(&tree)).inside.is_empty());
        assert_eq!(resolved(Path::new("some/dir")), None);
    }

    #[test]
    fn a_worktree_symlinked_out_of_the_root_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktrees");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(root.join("app/qi_1")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&checkout, root.join("app/qi_2")).unwrap();
        std::os::unix::fs::symlink(&checkout, root.join("linked")).unwrap();
        assert!(in_worktree_root(&root, &root.join("app/qi_1")));
        assert!(!in_worktree_root(&root, &root.join("app/qi_2")));
        assert!(!in_worktree_root(&root, &root.join("linked/qi_3")));
        assert!(!in_worktree_root(&root, &checkout));
        assert!(!in_worktree_root(&root, &root));
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
                "c0\t{project}\t{}\t{}\n",
                dir.join("compose.yml").display(),
                dir.display()
            )
        };
        let listing = line("theirs", &tree.join("ext")) + &line("ours", &tree.join("db"));
        let selection = select(&listing, &trees_of(&tree));
        let ours: Vec<&String> = selection.inside.keys().collect();
        assert_eq!(ours, ["ours"]);
        assert_eq!(selection.inside["ours"].dir, tree.join("db"));
        assert!(selection.shared.is_empty(), "{selection:?}");
    }
}
