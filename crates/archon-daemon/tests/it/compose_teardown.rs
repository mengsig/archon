//! Tearing down the Docker Compose stacks agents leave in item worktrees,
//! against a fake `docker` that logs its calls and prints canned listings.

use crate::{common, link_common};

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_daemon::compose;
use archon_daemon::executor;
use link_common::Harness;
use tempfile::TempDir;

/// `ps` prints the canned containers, a project's alone when filtered by
/// it; `ps.after.N` replaces them after the Nth `ps`. `rm` drops the
/// containers it names. `block.<command>` holds that command once until
/// `release` appears, ten seconds at most.
const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
if [ -e "$dir/block.$1" ]; then
  rm -f "$dir/block.$1"
  touch "$dir/blocked.$1"
  i=0
  while [ ! -e "$dir/release" ] && [ "$i" -lt 200 ]; do sleep 0.05; i=$((i + 1)); done
fi
case "$1" in
  ps)
    project=$(echo "$*" | sed -n 's/.*label=com\.docker\.compose\.project=\([^ ]*\).*/\1/p')
    if [ -n "$project" ]; then
      awk -F '\t' -v p="$project" '$2 == p' "$dir/ps" 2>/dev/null
    else
      cat "$dir/ps" 2>/dev/null
    fi
    n=$(grep -c '^ps' "$dir/calls")
    if [ -e "$dir/ps.after.$n" ]; then mv "$dir/ps.after.$n" "$dir/ps"; fi ;;
  stop|rm)
    if [ -e "$dir/$1.exit" ]; then
      echo "cannot $1 the containers" >&2
      exit 1
    fi
    if [ "$1" = rm ] && [ -e "$dir/ps" ]; then
      shift
      awk -F '\t' -v ids=" $* " 'index(ids, " " $1 " ") == 0' "$dir/ps" > "$dir/ps.left"
      mv "$dir/ps.left" "$dir/ps"
    fi ;;
  network)
    if [ "$2" = ls ]; then cat "$dir/networks" 2>/dev/null; fi ;;
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

    /// The calls that stop or remove something.
    pub(crate) fn removals(&self) -> Vec<String> {
        let calls = self.calls();
        let removes = |c: &String| {
            ["stop ", "rm ", "network rm "]
                .iter()
                .any(|v| c.starts_with(v))
        };
        calls.into_iter().filter(removes).collect()
    }

    fn fails(&self, command: &str) {
        std::fs::write(self.dir.path().join(format!("{command}.exit")), "1").unwrap();
    }

    fn blocks(&self, command: &str) {
        std::fs::write(self.dir.path().join(format!("block.{command}")), "").unwrap();
    }

    /// Wait, five seconds at most, until a `command` is held by [`Self::blocks`].
    async fn held(&self, command: &str) {
        let held = self.dir.path().join(format!("blocked.{command}"));
        for _ in 0..500 {
            if held.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("docker {command} was never called: {:?}", self.calls());
    }

    fn release(&self) {
        std::fs::write(self.dir.path().join("release"), "").unwrap();
    }
}

async fn harness_with(docker: &Path) -> Harness {
    let mut config = Config::default();
    config.worktrees.docker = docker.to_path_buf();
    Harness::with(config, None).await
}

/// One `docker ps` line: container ID, project, config files, working
/// directory.
pub(crate) fn container(id: &str, project: &str, dir: &Path) -> String {
    let configs = format!(
        "{},{}",
        dir.join("compose.yml").display(),
        dir.join("compose.dev.yml").display()
    );
    format!("{id}\t{project}\t{configs}\t{}", dir.display())
}

fn item_tree(harness: &Harness, item: &QueueItemId) -> PathBuf {
    let tree = harness.ctx.paths.worktrees.join("app").join(item.as_str());
    std::fs::create_dir_all(&tree).unwrap();
    tree
}

/// Only a project every container of which runs from the worktree is torn
/// down, its containers and then its networks; no volume is ever removed,
/// as Docker cannot tell whose it is.
#[tokio::test]
async fn only_the_worktrees_own_projects_go_and_volumes_are_kept() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    let sibling = harness.ctx.paths.worktrees.join("app/qi_other");
    let operator = PathBuf::from("/home/op/app");
    docker.lists(
        "ps",
        &[
            container("a1", "review-1", &tree),
            container("a2", "review-1", &tree.join("services")),
            container("b1", "mine", &operator),
            container("c1", "sibling", &sibling),
            container("d1", "both", &tree),
            container("d2", "both", &operator),
        ],
    );
    docker.lists("networks", &["e1".to_owned()]);
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    assert_eq!(
        docker.removals(),
        ["stop a1 a2", "rm a1 a2", "network rm e1"],
        "{:?}",
        docker.calls()
    );
    let calls = docker.calls();
    assert!(!calls.iter().any(|c| c.contains("volume")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("compose")), "{calls:?}");
    assert!(harness.system_notes().await.is_empty());
}

/// A project that gained a container from elsewhere after it was listed is
/// left running: ownership is checked again just before the teardown.
#[tokio::test]
async fn a_project_that_changed_since_its_listing_is_left_running() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    let operator = PathBuf::from("/home/op/app");
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    let joined = [
        container("a1", "review-1", &tree),
        container("b1", "review-1", &operator),
    ];
    docker.lists("ps.after.1", &joined);
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    assert!(docker.removals().is_empty(), "{:?}", docker.calls());
    assert_eq!(
        docker.calls().len(),
        2,
        "two listings: {:?}",
        docker.calls()
    );
}

/// Only the containers checked are stopped: one that joins the project
/// meanwhile is not, and keeps the project's networks.
#[tokio::test]
async fn a_container_that_joins_during_the_teardown_is_not_stopped() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    let operator = PathBuf::from("/home/op/app");
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    let joined = [
        container("a1", "review-1", &tree),
        container("b1", "review-1", &operator),
    ];
    docker.lists("ps.after.2", &joined);
    docker.lists("networks", &["e1".to_owned()]);
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    assert_eq!(
        docker.removals(),
        ["stop a1", "rm a1"],
        "{:?}",
        docker.calls()
    );
    assert!(harness.system_notes().await.is_empty());
}

/// A container that removed itself once stopped, as `compose run --rm`
/// ones do, is not removed again, and the teardown succeeds.
#[tokio::test]
async fn a_container_gone_once_stopped_is_not_removed_again() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    let listed = [
        container("a1", "review-1", &tree),
        container("a2", "review-1", &tree),
    ];
    docker.lists("ps", &listed);
    docker.lists("ps.after.2", &[container("a1", "review-1", &tree)]);
    docker.lists("networks", &["e1".to_owned()]);
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    assert_eq!(
        docker.removals(),
        ["stop a1 a2", "rm a1", "network rm e1"],
        "{:?}",
        docker.calls()
    );
    assert!(harness.system_notes().await.is_empty());
}

/// Once the worktree is gone nothing is stopped: a project whose labels
/// name it may have run from elsewhere through a symlink that went with it.
#[tokio::test]
async fn nothing_is_stopped_once_the_worktree_is_gone() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    docker.lists("ps", &[container("a1", "theirs", &tree.join("ext"))]);
    std::fs::remove_dir_all(&tree).unwrap();
    compose::tear_down(&harness.ctx, &item, &tree, "now")
        .await
        .unwrap();
    assert!(docker.calls().is_empty(), "{:?}", docker.calls());
}

/// A directory outside Archon's worktree root is never looked at.
#[tokio::test]
async fn a_tree_outside_the_worktree_root_is_never_looked_at() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let elsewhere = harness.dir.path().join("checkout");
    docker.lists("ps", &[container("a1", "mine", &elsewhere)]);
    for tree in [elsewhere.clone(), harness.ctx.paths.worktrees.clone()] {
        compose::tear_down(&harness.ctx, &item, &tree, "now")
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
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    assert!(harness.system_notes().await.is_empty());
}

/// A teardown that fails is told in the feed and fails nothing; what it
/// could not stop is not removed.
#[tokio::test]
async fn a_failed_teardown_is_told_in_the_feed() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let tree = item_tree(&harness, &item);
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    docker.fails("stop");
    compose::tear_down(&harness.ctx, &item, &tree, "after its run ended")
        .await
        .unwrap();
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("Could not tear down the Docker Compose stack"),
        "{notes:?}"
    );
    assert!(notes[0].contains("cannot stop the containers"), "{notes:?}");
    assert_eq!(docker.removals(), ["stop a1"], "{:?}", docker.calls());
}

/// A running stage's item, its run moved into its own worktree.
async fn running_in(harness: &Harness) -> (QueueItemId, PathBuf) {
    let stage =
        link_common::running_stage(&harness.ctx, harness.dir.path(), "agt_0401", "tsk_0401").await;
    let mut store = harness.ctx.store.lock().await;
    let mut run = store.run(&stage.run).unwrap();
    let worktrees = &harness.ctx.paths.worktrees;
    run.workspace = worktrees.join("app").join(run.item.as_str());
    store.update_run(&run).unwrap();
    std::fs::create_dir_all(&run.workspace).unwrap();
    (run.item, run.workspace)
}

/// Ending a run tears down the stacks its worktree left, keeping volumes.
#[tokio::test]
async fn ending_a_run_tears_down_its_worktrees_stacks() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let (item, tree) = running_in(&harness).await;
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    assert_eq!(docker.removals(), ["stop a1", "rm a1"]);
    assert!(
        !harness.ctx.collecting.contains(&item),
        "the teardown let go of the worktree"
    );
}

/// A run that ends while another item, such as a restack fix or refresh,
/// still works in the same worktree leaves the stacks there running.
#[tokio::test]
async fn a_run_ending_beside_a_live_sharer_leaves_the_stacks_alone() {
    let docker = FakeDocker::new();
    let harness = harness_with(&docker.binary()).await;
    let (owner, tree) = running_in(&harness).await;
    let mut beside = link_common::another_running_run(&harness.ctx).await;
    beside.workspace = tree.clone();
    harness.ctx.store.lock().await.update_run(&beside).unwrap();
    let sharer = beside.item;
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    executor::cancel_item(&harness.ctx, &owner, "done")
        .await
        .unwrap();
    assert!(docker.calls().is_empty(), "{:?}", docker.calls());
    assert!(!harness.ctx.collecting.contains(&owner));
    executor::cancel_item(&harness.ctx, &sharer, "done")
        .await
        .unwrap();
    assert_eq!(docker.removals(), ["stop a1", "rm a1"]);
}

async fn run_harness(docker: &FakeDocker) -> common::Harness {
    let mut config = Config::default();
    config.worktrees.docker = docker.binary();
    common::Harness::with_config(config).await
}

fn retry(item: &QueueItem, fresh: bool) -> Request {
    Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh,
        brief_append: None,
        stage_timeouts: Default::default(),
    }
}

/// An item whose run started in its worktree and was cancelled.
async fn cancelled_run(h: &common::Harness) -> (QueueItem, PathBuf) {
    let item = h.enqueue("starts a stack").await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    (item, tree)
}

/// A retry queued while the ended run's teardown still stops its stack
/// waits for it: started at once, it could start a stack the teardown kills.
#[tokio::test]
async fn a_retry_waits_for_the_teardown_of_its_worktrees_stacks() {
    let docker = FakeDocker::new();
    let h = run_harness(&docker).await;
    let item = h.enqueue("starts a stack").await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    docker.blocks("stop");
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    let meanwhile = async {
        docker.held("stop").await;
        h.cli(retry(&item, false)).await.expect("retry");
        h.dispatch().await;
        let held = h.state(&item.id).await;
        docker.release();
        held
    };
    let (cancelled, held) = tokio::join!(h.cli(cancel), meanwhile);
    cancelled.expect("cancel");
    assert_eq!(
        held,
        QueueState::Queued,
        "no run starts during the teardown"
    );
    assert_eq!(docker.removals(), ["stop a1", "rm a1"]);
    h.dispatch().await;
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    assert_eq!(runs.len(), 2, "the retry starts once the teardown is done");
}

/// A fresh retry tears down its worktree's stacks without the run table, so
/// a slow Docker holds up no other item, and holds the worktree meanwhile.
#[tokio::test]
async fn a_fresh_retry_tears_down_without_holding_the_run_table() {
    let docker = FakeDocker::new();
    let h = run_harness(&docker).await;
    let (item, tree) = cancelled_run(&h).await;
    docker.lists("ps", &[container("a1", "review-1", &tree)]);
    docker.blocks("ps");
    let meanwhile = async {
        docker.held("ps").await;
        let table = tokio::time::timeout(Duration::from_secs(5), h.ctx.runs.lock()).await;
        let free = table.is_ok();
        drop(table);
        let second = h.cli(retry(&item, true)).await;
        docker.release();
        (free, second)
    };
    let (first, (free, second)) = tokio::join!(h.cli(retry(&item, true)), meanwhile);
    first.expect("fresh retry");
    assert!(free, "the run table is free while docker answers");
    assert!(
        matches!(&second, Err(ArchonError::Unavailable(why)) if why.contains("teardown")),
        "{second:?}"
    );
    assert_eq!(docker.removals(), ["stop a1", "rm a1"]);
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert!(!h.ctx.collecting.contains(&item.id), "the mark went");
}

/// A start refused after it added its worktree removes it under the run
/// table without asking Docker: no agent ran there to start a stack.
#[tokio::test]
async fn an_abandoned_start_asks_docker_nothing() {
    let docker = FakeDocker::new();
    let h = run_harness(&docker).await;
    let item = h.enqueue("refused after its worktree").await;
    let path = h.repo.join(".archon/pipelines").join(common::PIPELINE);
    let toml = std::fs::read_to_string(path.join("pipeline.toml")).expect("pipeline");
    let ship = "[[stage]]\nid = \"ship\"";
    assert!(toml.contains(ship), "ship is the last stage");
    let docs = "[[stage]]\nid = \"docs\"\nkind = \"work\"\nprompt = \"implement.md\"\ntimeout_minutes = 30\n\n";
    h.add_pipeline(
        common::PIPELINE,
        &toml.replace(ship, &format!("{docs}{ship}")),
    );
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(
        matches!(&state, QueueState::Failed { run: None, error } if error.contains("docs")),
        "{state:?}"
    );
    assert!(docker.calls().is_empty(), "{:?}", docker.calls());
}
