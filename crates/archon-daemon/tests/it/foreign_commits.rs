//! Which commits made under another identity a delivery admits (#334): those
//! `origin`'s base holds, the repository's own base included for a stack,
//! never one only another remote branch holds.

use crate::common;

use std::path::Path;
use std::process::Command;

use archon_core::autonomy::AutonomyPatch;
use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::Attribution;
use archon_core::run::{PipelineRun, StageOutcome};
use common::delivery::{FakeGh, add_origin, change, origin_of, pushed, record};
use common::{Harness, REPO_NAME, git};
use serde_json::Value;

/// An address the repository's identity is not.
const OUTSIDER: &str = "outsider@elsewhere.invalid";

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// A `direct` item depending on `parents`, dispatched: its agent runs.
async fn started(h: &Harness, parents: &[&QueueItem]) -> (QueueItem, PipelineRun) {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: "a change through direct".into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: parents.iter().map(|p| p.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch::default(),
        origin: None,
    };
    let item = match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    };
    h.dispatch().await;
    let run = h.run(&item.id).await;
    (item, run)
}

/// The one running stage completes with `structured`; then a dispatch.
async fn settle_with(h: &Harness, structured: Value) {
    let outcome = StageOutcome::Completed {
        summary: "Changed it".into(),
        structured: Some(structured),
    };
    let settled = h.settle_running(|_| outcome.clone()).await;
    assert_eq!(settled.len(), 1, "one agent ran");
    h.dispatch().await;
}

async fn failed(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the item fails: {other:?}"),
    }
}

async fn succeeded(h: &Harness, item: &QueueItem) {
    let state = h.state(&item.id).await;
    assert!(
        matches!(state, QueueState::Succeeded { .. }),
        "the item ships: {state:?}"
    );
}

/// Commit `file` in `tree` under [`OUTSIDER`], with `message`; its id.
fn foreign_commit(tree: &Path, file: &str, message: &str) -> String {
    std::fs::write(tree.join(file), "theirs\n").unwrap();
    git(tree, &["add", file]);
    let made = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["commit", "--quiet", "-m", message])
        .env("GIT_AUTHOR_NAME", "Outsider")
        .env("GIT_AUTHOR_EMAIL", OUTSIDER)
        .env("GIT_COMMITTER_NAME", "Outsider")
        .env("GIT_COMMITTER_EMAIL", OUTSIDER)
        .status()
        .unwrap();
    assert!(made.success(), "the foreign commit is made");
    git(tree, &["rev-parse", "HEAD"])
}

/// One commit of `file` in `workspace` under the repository's identity, and
/// the ship record that names it.
fn own_change(workspace: &Path, file: &str) -> Value {
    std::fs::write(workspace.join(file), "ours\n").unwrap();
    git(workspace, &["add", file]);
    git(workspace, &["commit", "--quiet", "-m", "Our change"]);
    record(workspace)
}

/// Push a commit by [`OUTSIDER`] to `origin`'s `main`, as a merge on GitHub
/// by another author does; its id.
fn outsider_merges_on_origin(h: &Harness) -> String {
    let upstream = h.dir.path().join("upstream");
    let origin = origin_of(h);
    let (origin, path) = (origin.to_str().unwrap(), upstream.to_str().unwrap());
    git(h.dir.path(), &["clone", "--quiet", origin, path]);
    let theirs = foreign_commit(&upstream, "merged.txt", "Merged on GitHub");
    git(&upstream, &["push", "--quiet", "origin", "main"]);
    theirs
}

fn holds(tree: &Path, commit: &str) -> bool {
    let ran = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .unwrap();
    ran.success()
}

/// A foreign commit fetched into a remote-tracking ref that is not the base,
/// such as a pull request's head or another remote's branch, is refused and
/// named, and nothing leaves the machine.
#[tokio::test]
async fn a_foreign_commit_only_another_remote_branch_holds_is_refused() {
    for tracking in ["refs/remotes/origin/pull/7/head", "refs/remotes/fork/main"] {
        let (h, gh) = delivering().await;
        let (item, run) = started(&h, &[]).await;
        let theirs = foreign_commit(&run.workspace, "theirs.txt", "Their change");
        git(&run.workspace, &["update-ref", tracking, &theirs]);
        settle_with(&h, change(&run.workspace)).await;

        let error = failed(&h, &item).await;
        let foreign = "1 commit(s) on the run branch were not made under this repository's \
                       git identity (<test@archon.invalid>)";
        assert!(error.contains(foreign), "{tracking}: {error}");
        assert!(error.contains(&theirs[..12]), "{tracking}: {error}");
        assert!(error.contains(OUTSIDER), "{tracking}: {error}");
        assert_eq!(pushed(&origin_of(&h), &run.branch), None, "{tracking}");
        assert!(gh.calls().is_empty(), "{tracking}");
    }
}

/// Under the attribution policy `none`, an attributed commit only another
/// remote branch holds is the run's own too, and is refused.
#[tokio::test]
async fn an_attributed_commit_only_another_remote_branch_holds_is_refused() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, &[]).await;
    let patch = RepoPatch {
        attribution: Some(Attribution::None),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
    std::fs::write(run.workspace.join("theirs.txt"), "theirs\n").unwrap();
    git(&run.workspace, &["add", "theirs.txt"]);
    let message = "Their change\n\nCo-Authored-By: Claude <noreply@anthropic.com>";
    git(&run.workspace, &["commit", "--quiet", "-m", message]);
    let tip = git(&run.workspace, &["rev-parse", "HEAD"]);
    git(
        &run.workspace,
        &["update-ref", "refs/remotes/fork/main", &tip],
    );
    settle_with(&h, change(&run.workspace)).await;

    let error = failed(&h, &item).await;
    assert!(error.contains("attribution policy is `none`"), "{error}");
    assert!(error.contains(&format!("commit {}", &tip[..12])), "{error}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty());
}

/// A refresh-like merge of `origin`'s base brings another author's commit
/// beneath the run's own, and the delivery ships it.
#[tokio::test]
async fn a_foreign_commit_merged_from_origin_s_base_ships() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, &[]).await;
    let theirs = outsider_merges_on_origin(&h);
    git(&run.workspace, &["fetch", "--quiet", "origin"]);
    git(
        &run.workspace,
        &["merge", "--quiet", "--no-edit", "origin/main"],
    );
    assert!(holds(&run.workspace, &theirs), "the merge took it in");
    let shipped = own_change(&run.workspace, "ours.txt");
    settle_with(&h, shipped.clone()).await;

    succeeded(&h, &item).await;
    let commit = shipped["commit"].as_str().unwrap().to_owned();
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(commit));
    assert_eq!(gh.count("pr create"), 1);
}

/// A stack on two unmerged parents ships: the first's branch is its base, and
/// the second, forked after another author's merge on `origin`'s `main`,
/// brings that commit in, which the repository's base holds.
#[tokio::test]
async fn a_stack_on_unmerged_parents_ships_the_base_commits_they_bring() {
    let (h, gh) = delivering().await;
    let (one, one_run) = started(&h, &[]).await;
    settle_with(&h, own_change(&one_run.workspace, "one.txt")).await;
    succeeded(&h, &one).await;
    let theirs = outsider_merges_on_origin(&h);
    let (two, two_run) = started(&h, &[]).await;
    assert!(holds(&two_run.workspace, &theirs), "forked from origin");
    settle_with(&h, own_change(&two_run.workspace, "two.txt")).await;
    succeeded(&h, &two).await;

    let (child, child_run) = started(&h, &[&one, &two]).await;
    for file in ["one.txt", "two.txt", "merged.txt"] {
        assert!(child_run.workspace.join(file).exists(), "{file}");
    }
    let base = h.ctx.store.lock().await.stack_base(&child.id).unwrap();
    assert_eq!(base.map(|b| b.0), Some(one_run.branch.clone()));
    let shipped = own_change(&child_run.workspace, "child.txt");
    settle_with(&h, shipped.clone()).await;

    succeeded(&h, &child).await;
    let commit = shipped["commit"].as_str().unwrap().to_owned();
    assert_eq!(pushed(&origin_of(&h), &child_run.branch), Some(commit));
    assert_eq!(gh.count("pr create"), 3);
}
