//! Tearing down the Docker Compose stacks agents leave in item worktrees,
//! against a fake `docker` that logs its calls and prints canned listings.

use crate::link_common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::config::Config;
use archon_core::id::QueueItemId;
use archon_daemon::compose::{self, Volumes};
use archon_daemon::executor;
use link_common::Harness;
use tempfile::TempDir;

const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case "$1" in
  ps) cat "$dir/ps" 2>/dev/null ;;
  volume) [ "$2" = ls ] && cat "$dir/volumes" 2>/dev/null ;;
  compose)
    if [ -e "$dir/down.exit" ]; then
      echo "cannot stop the containers" >&2
      exit 1
    fi
    rm -f "$dir/ps" ;;
esac
exit 0
"#;

pub(crate) struct FakeDocker {
    dir: TempDir,
}

impl FakeDocker {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("docker");
        std::fs::write(&binary, FAKE_DOCKER).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    pub(crate) fn binary(&self) -> PathBuf {
        self.dir.path().join("docker")
    }

    pub(crate) fn lists(&self, name: &str, lines: &[String]) {
        std::fs::write(self.dir.path().join(name), lines.join("\n") + "\n").unwrap();
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }

    pub(crate) fn downs(&self) -> Vec<String> {
        let calls = self.calls();
        calls
            .into_iter()
            .filter(|c| c.starts_with("compose"))
            .collect()
    }
}

async fn harness_with(docker: &Path) -> Harness {
    let mut config = Config::default();
    config.worktrees.docker = docker.to_path_buf();
    Harness::with(config, None).await
}

/// One `docker ps` line: project, config files, working directory.
pub(crate) fn container(project: &str, dir: &Path) -> String {
    let configs = format!(
        "{},{}",
        dir.join("compose.yml").display(),
        dir.join("compose.dev.yml").display()
    );
    format!("{project}\t{configs}\t{}", dir.display())
}

fn item_tree(harness: &Harness, item: &QueueItemId) -> PathBuf {
    let tree = harness.ctx.paths.worktrees.join("app").join(item.as_str());
    std::fs::create_dir_all(&tree).unwrap();
    tree
}

/// Only a project every container of which runs from the worktree is torn
/// down, its volumes kept; removing the worktree later removes them.
#[tokio::test]
async fn only_the_worktrees_own_projects_go_and_their_volumes_with_the_worktree() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    let sibling = harness.ctx.paths.worktrees.join("app/qi_other");
    let operator = PathBuf::from("/home/op/app");
    docker.lists(
        "ps",
        &[
            container("review-1", &tree),
            container("review-1", &tree.join("services")),
            container("mine", &operator),
            container("sibling", &sibling),
            container("both", &tree),
            container("both", &operator),
        ],
    );

    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Keep,
        "after its run ended",
    )
    .await
    .unwrap();
    assert_eq!(
        docker.downs(),
        ["compose --project-name review-1 down --remove-orphans"],
        "{:?}",
        docker.calls()
    );
    assert!(harness.system_notes().await.is_empty());

    docker.lists("volumes", &["review-1_db".to_owned()]);
    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Remove,
        "when gc removed it",
    )
    .await
    .unwrap();
    let calls = docker.calls();
    let label = "label=com.docker.compose.project=review-1";
    assert!(
        calls.contains(&format!("volume ls --quiet --filter {label}")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"volume rm review-1_db".to_owned()),
        "{calls:?}"
    );
    let rms = calls.iter().filter(|c| c.starts_with("volume rm")).count();
    assert_eq!(rms, 1, "{calls:?}");

    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Remove,
        "when gc removed it",
    )
    .await
    .unwrap();
    let again = docker.calls();
    assert_eq!(again.len(), calls.len() + 1, "only a listing: {again:?}");
}

/// A kept project whose name another checkout now runs under keeps its
/// volumes: they may be that checkout's.
#[tokio::test]
async fn kept_volumes_stay_when_another_checkout_reuses_the_project_name() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    docker.lists("ps", &[container("app", &tree)]);
    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Keep,
        "after its run ended",
    )
    .await
    .unwrap();
    docker.lists("ps", &[container("app", Path::new("/home/op/app"))]);
    docker.lists("volumes", &["app_db".to_owned()]);
    for _ in 0..2 {
        compose::tear_down(
            &harness.ctx,
            &item,
            &tree,
            Volumes::Remove,
            "when gc removed it",
        )
        .await
        .unwrap();
    }
    let calls = docker.calls();
    assert!(!calls.iter().any(|c| c.starts_with("volume")), "{calls:?}");
    assert_eq!(docker.downs().len(), 1, "{calls:?}");
    assert!(harness.system_notes().await.is_empty());
}

/// A stack still running when its worktree goes is torn down with its
/// volumes.
#[tokio::test]
async fn a_stack_running_when_its_worktree_goes_loses_its_volumes_too() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    docker.lists("ps", &[container("ccso-review-06yr", &tree)]);
    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Remove,
        "when gc removed it",
    )
    .await
    .unwrap();
    assert_eq!(
        docker.downs(),
        ["compose --project-name ccso-review-06yr down --remove-orphans --volumes"]
    );
    let calls = docker.calls();
    assert!(!calls.iter().any(|c| c.starts_with("volume")), "{calls:?}");
}

/// A directory outside Archon's worktree root is never looked at.
#[tokio::test]
async fn a_tree_outside_the_worktree_root_is_never_looked_at() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let elsewhere = harness.dir.path().join("checkout");
    docker.lists("ps", &[container("mine", &elsewhere)]);
    for tree in [elsewhere.clone(), harness.ctx.paths.worktrees.clone()] {
        compose::tear_down(&harness.ctx, &item, &tree, Volumes::Remove, "now")
            .await
            .unwrap();
    }
    assert!(docker.calls().is_empty(), "{:?}", docker.calls());
}

/// Without Docker nothing is torn down and nothing is said.
#[tokio::test]
async fn without_docker_a_teardown_is_skipped_quietly() {
    let missing = tempfile::tempdir().unwrap();
    let harness = harness_with(&missing.path().join("docker")).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Keep,
        "after its run ended",
    )
    .await
    .unwrap();
    assert!(harness.system_notes().await.is_empty());
}

/// A teardown that fails is told in the feed and fails nothing.
#[tokio::test]
async fn a_failed_teardown_is_told_in_the_feed() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    docker.lists("ps", &[container("review-1", &tree)]);
    std::fs::write(docker.dir.path().join("down.exit"), "1").unwrap();
    compose::tear_down(
        &harness.ctx,
        &item,
        &tree,
        Volumes::Keep,
        "after its run ended",
    )
    .await
    .unwrap();
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("Could not tear down the Docker Compose stack"),
        "{notes:?}"
    );
    assert!(notes[0].contains("cannot stop the containers"), "{notes:?}");
}

/// Ending a run tears down the stacks its worktree left, keeping volumes.
#[tokio::test]
async fn ending_a_run_tears_down_its_worktrees_stacks() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let stage =
        link_common::running_stage(&harness.ctx, harness.dir.path(), "agt_0401", "tsk_0401").await;
    let (item, tree) = {
        let mut store = harness.ctx.store.lock().await;
        let mut run = store.run(&stage.run).unwrap();
        run.workspace = harness
            .ctx
            .paths
            .worktrees
            .join("app")
            .join(run.item.as_str());
        store.update_run(&run).unwrap();
        (run.item, run.workspace)
    };
    std::fs::create_dir_all(&tree).unwrap();
    docker.lists("ps", &[container("review-1", &tree)]);
    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    harness
        .eventually("the run's stack is torn down", || async {
            !docker.downs().is_empty()
        })
        .await;
    assert_eq!(
        docker.downs(),
        ["compose --project-name review-1 down --remove-orphans"]
    );
}
