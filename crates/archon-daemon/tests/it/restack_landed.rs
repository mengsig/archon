//! A parent whose pull request GitHub merged as a squash or a rebase: its
//! child, which holds the parent's own commits, is merged over the landing
//! on `main` and pushed, never rebased or force-pushed (ARCHITECTURE §8.8).

use crate::common;
use crate::restack::{restack_fixes, stack_pair};

use archon_core::notify::NotificationKind;
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueItem;
use common::initiative::{commit, finish_item, prs, set_pr};
use common::{Harness, git};
use serde_json::json;

/// How GitHub merges the parent's pull request into `origin`'s `main`.
#[derive(Clone, Copy)]
enum Merged {
    Squash,
    /// After another commit landed on `main`.
    RebaseAfterMainMoved,
    /// A squash the maintainer added `extra.txt` and `parent_txt` to.
    EditedSquash {
        parent_txt: &'static str,
    },
}

/// The parent and child of a stack, the child finished and delivered; one
/// that rewrote `parent.txt` conflicts with a plain merge of a squash.
async fn finished_stack(child_parent_txt: Option<&str>) -> (Harness, QueueItem, QueueItem) {
    let (h, parent, child) = stack_pair().await;
    let child_run = h.run(&child.id).await;
    if let Some(content) = child_parent_txt {
        commit(&child_run.workspace, "parent.txt", content);
    }
    commit(&child_run.workspace, "child.txt", "child\n");
    finish_item(&h, &child.id).await;
    (h, parent, child)
}

/// GitHub merging `branch` into `origin`'s `main` as `how`; the new `main`.
fn merge_on_github(h: &Harness, branch: &str, how: Merged) -> String {
    let remote = h.dir.path().join("remote.git");
    let github = h.dir.path().join("github");
    let (from, to) = (remote.to_str().unwrap(), github.to_str().unwrap());
    git(
        h.dir.path(),
        &["clone", "--quiet", "--branch", "main", from, to],
    );
    let repo = h.repo.to_str().unwrap();
    git(
        &github,
        &["fetch", "--quiet", repo, &format!("{branch}:parent")],
    );
    match how {
        Merged::Squash => squash(&github),
        Merged::RebaseAfterMainMoved => {
            commit(&github, "main-moved.txt", "moved\n");
            git(
                &github,
                &["-c", "user.name=GitHub", "cherry-pick", "main..parent"],
            );
        }
        Merged::EditedSquash { parent_txt } => {
            git(&github, &["merge", "--quiet", "--squash", "parent"]);
            std::fs::write(github.join("extra.txt"), "extra\n").unwrap();
            std::fs::write(github.join("parent.txt"), parent_txt).unwrap();
            git(&github, &["add", "extra.txt", "parent.txt"]);
            git(&github, &["commit", "--quiet", "-m", "Parent, edited (#1)"]);
        }
    }
    git(&github, &["push", "--quiet", "origin", "main"]);
    set_pr(h, branch, "state", json!("MERGED"));
    git(&github, &["rev-parse", "HEAD"])
}

fn squash(github: &std::path::Path) {
    git(github, &["merge", "--quiet", "--squash", "parent"]);
    git(github, &["commit", "--quiet", "-m", "Parent (#1)"]);
}

/// `rev` in `repo`, or `None` when it names nothing.
fn rev(repo: &std::path::Path, rev: &str) -> Option<String> {
    let name = format!("refs/heads/{rev}");
    let listed = git(repo, &["for-each-ref", "--format=%(objectname)", &name]);
    (!listed.is_empty()).then_some(listed)
}

fn file_at(repo: &std::path::Path, commit: &str, file: &str) -> String {
    git(repo, &["show", &format!("{commit}:{file}")])
}

/// The child's branch before and after the restack, here and on `origin`,
/// checked to be a merge of `main` that `origin` took as a fast-forward.
struct Restacked {
    before: String,
    after: String,
}

async fn restacked(h: &Harness, child: &QueueItem, before: &str, main: &str) -> Restacked {
    let branch = h.run(&child.id).await.branch;
    let after = git(&h.repo, &["rev-parse", &branch]);
    assert_ne!(after, before, "the child was restacked");
    let parents = git(&h.repo, &["rev-parse", &format!("{after}^@")]);
    assert_eq!(parents, format!("{before}\n{main}"), "a merge of `main`");
    let remote = h.dir.path().join("remote.git");
    assert_eq!(
        rev(&remote, &branch).as_deref(),
        Some(after.as_str()),
        "pushed"
    );
    git(&remote, &["merge-base", "--is-ancestor", before, &after]);
    assert!(restack_fixes(h, &child.id).await.is_empty(), "no fix item");
    assert_eq!(prs(h)[&branch]["baseRefName"], "main");
    Restacked {
        before: before.to_owned(),
        after,
    }
}

async fn child_head(h: &Harness, child: &QueueItem) -> String {
    git(&h.repo, &["rev-parse", &h.run(&child.id).await.branch])
}

async fn landed(h: &Harness, child: &QueueItem) -> Option<String> {
    let parents = h.ctx.store.lock().await.stack_parents(&child.id).unwrap();
    assert!(parents[0].merged, "{parents:?}");
    parents[0].landed.clone()
}

/// The squash's tree is the parent head's: the child records `merge -s
/// ours` of it, keeping its own tree, where a plain merge conflicts.
#[tokio::test]
async fn a_squash_merged_parent_restacks_its_child_content_neutrally() {
    let (h, parent, child) = finished_stack(Some("parent\nchild\n")).await;
    let before = child_head(&h, &child).await;
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, Merged::Squash);
    h.dispatch().await;

    let done = restacked(&h, &child, &before, &main).await;
    let tree = |rev: &str| git(&h.repo, &["rev-parse", &format!("{rev}^{{tree}}")]);
    assert_eq!(tree(&done.after), tree(&done.before), "content-neutral");
    assert_eq!(landed(&h, &child).await, Some(main));
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    h.dispatch().await;
    assert_eq!(child_head(&h, &child).await, done.after, "restacked once");
}

/// A rebase after `main` moved has no commit of the parent's tree: the
/// child is merged over the parent's head, taking only `main`'s own change.
#[tokio::test]
async fn a_rebase_merged_parent_restacks_its_child_without_conflict() {
    let (h, parent, child) = finished_stack(Some("parent\nchild\n")).await;
    let before = child_head(&h, &child).await;
    let how = Merged::RebaseAfterMainMoved;
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, how);
    h.dispatch().await;

    let done = restacked(&h, &child, &before, &main).await;
    assert_eq!(file_at(&h.repo, &done.after, "parent.txt"), "parent\nchild");
    assert_eq!(file_at(&h.repo, &done.after, "main-moved.txt"), "moved");
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
}

/// A child still running when its parent is squash-merged is left alone
/// while agents work in it, then restacked over the landing once delivered.
#[tokio::test]
async fn a_child_running_when_its_parent_squash_merges_is_restacked_once_finished() {
    let (h, parent, child) = stack_pair().await;
    let workspace = h.run(&child.id).await.workspace;
    commit(&workspace, "parent.txt", "parent\nchild\n");
    let before = commit(&workspace, "child.txt", "child\n");
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, Merged::Squash);
    h.dispatch().await;
    assert_eq!(
        child_head(&h, &child).await,
        before,
        "untouched while running"
    );
    assert_eq!(landed(&h, &child).await, None, "not restacked yet");

    finish_item(&h, &child.id).await;
    h.dispatch().await;
    let done = restacked(&h, &child, &before, &main).await;
    let tree = |rev: &str| git(&h.repo, &["rev-parse", &format!("{rev}^{{tree}}")]);
    assert_eq!(tree(&done.after), tree(&done.before), "content-neutral");
    assert_eq!(landed(&h, &child).await, Some(main));
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
}

/// A squash that changed more than the parent proves no landing: `main` is
/// merged as git would, cleanly here, and a conflict is told, not forced.
#[tokio::test]
async fn a_base_that_changed_more_than_the_parent_is_merged_normally() {
    let (h, parent, child) = finished_stack(None).await;
    let before = child_head(&h, &child).await;
    let how = Merged::EditedSquash {
        parent_txt: "parent\n",
    };
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, how);
    h.dispatch().await;
    let done = restacked(&h, &child, &before, &main).await;
    assert_eq!(file_at(&h.repo, &done.after, "extra.txt"), "extra");
    assert_eq!(file_at(&h.repo, &done.after, "child.txt"), "child");

    let (h, parent, child) = finished_stack(Some("parent\nchild\n")).await;
    let before = child_head(&h, &child).await;
    let how = Merged::EditedSquash {
        parent_txt: "parent, edited\n",
    };
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, how);
    h.dispatch().await;
    h.dispatch().await;
    assert_eq!(child_head(&h, &child).await, before, "nothing written");
    let told = h.notices(NotificationKind::Blocker).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("parent.txt") && told[0].contains("refresh_item"),
        "{told:?}"
    );
    assert!(restack_fixes(&h, &child.id).await.is_empty(), "no fix item");
    assert_eq!(landed(&h, &child).await, Some(main));
}

/// `refresh_item` on a child whose parent was recorded merged before any
/// restack over it (an older daemon's): it restacks in place, queueing
/// nothing, where it refused before with "nothing to refresh".
#[tokio::test]
async fn a_refresh_restacks_a_child_of_a_squash_merged_parent_in_place() {
    let (h, parent, child) = finished_stack(Some("parent\nchild\n")).await;
    let before = child_head(&h, &child).await;
    {
        let mut store = h.ctx.store.lock().await;
        let mut recorded = store.stack_parents(&child.id).unwrap().remove(0);
        recorded.merged = true;
        recorded.landed = Some(before.clone());
        store.save_stack_parent(&recorded).unwrap();
    }
    set_pr(
        &h,
        &h.run(&child.id).await.branch,
        "baseRefName",
        json!("main"),
    );
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, Merged::Squash);

    let refresh = Request::RefreshItem {
        item: child.id.clone(),
        model_plan: None,
        model_reasons: None,
        stage_timeouts: Default::default(),
    };
    let Reply::Item(refreshed) = h.orchestrator(refresh).await.expect("refreshed") else {
        panic!("refresh replied no item");
    };
    assert_eq!(refreshed.id, child.id, "no refresh is queued");
    restacked(&h, &child, &before, &main).await;
    assert_eq!(landed(&h, &child).await, Some(main));
}

/// A restack whose push failed is pushed on the next pass: the child's
/// branch holding the landing is not taken for its pull request holding it.
#[tokio::test]
async fn a_restack_whose_push_failed_is_pushed_on_the_next_pass() {
    let (h, parent, child) = finished_stack(Some("parent\nchild\n")).await;
    let before = child_head(&h, &child).await;
    let branch = h.run(&child.id).await.branch;
    git(&h.repo, &["push", "--quiet", "origin", &branch]);
    set_pr(&h, &branch, "headRefOid", json!(before));
    let main = merge_on_github(&h, &h.run(&parent.id).await.branch, Merged::Squash);
    // `origin` cannot lock the branch, so the push fails as in an outage.
    let lock = h
        .dir
        .path()
        .join(format!("remote.git/refs/heads/{branch}.lock"));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "").unwrap();
    h.dispatch().await;
    assert_ne!(child_head(&h, &child).await, before, "merged here");
    let remote = h.dir.path().join("remote.git");
    assert_eq!(rev(&remote, &branch), Some(before.clone()), "not pushed");
    assert_eq!(landed(&h, &child).await, None, "not recorded");

    std::fs::remove_file(&lock).unwrap();
    h.dispatch().await;
    restacked(&h, &child, &before, &main).await;
    assert_eq!(landed(&h, &child).await, Some(main));
}
