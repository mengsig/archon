//! Real branches with a durable fake GitHub service.
mod common;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use common::initiative::{commit, finish_item, install_gh, prs, set_pr};
use common::{Harness, PIPELINE, REPO_NAME, git, plan};
use serde_json::json;

fn advance_branch(h: &Harness, branch: &str, file: &str, content: &str) -> String {
    let checkout = git(&h.repo, &["rev-parse", "HEAD"]);
    let edit = h.dir.path().join("parent-edit");
    git(
        &h.repo,
        &["worktree", "add", edit.to_str().unwrap(), branch],
    );
    assert_eq!(git(&edit, &["branch", "--show-current"]), branch);
    let sha = commit(&edit, file, content);
    assert_eq!(git(&h.repo, &["rev-parse", "HEAD"]), checkout);
    sha
}

async fn child(h: &Harness, parents: &[&QueueItem]) -> QueueItem {
    let req = EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: "Child change".into(),
        brief: "Build on the parent.".into(),
        priority: Default::default(),
        depends_on: parents.iter().map(|p| p.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: plan(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    match h.cli(Request::Enqueue(req)).await.unwrap() {
        Reply::Item(i) => i,
        other => panic!("{other:?}"),
    }
}

fn add_remote(h: &Harness) {
    let remote = h.dir.path().join("remote.git");
    git(&h.repo, &["init", "--bare", remote.to_str().unwrap()]);
    git(
        &h.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&h.repo, &["push", "origin", "main"]);
}

async fn stack_pair() -> (Harness, QueueItem, QueueItem) {
    let h = Harness::new().await;
    install_gh(&h);
    add_remote(&h);
    {
        let mut store = h.ctx.store.lock().await;
        let mut repo = store.repo(&RepoRef(REPO_NAME.into())).unwrap();
        repo.base_branch = None;
        store.update_repo(&repo).unwrap();
    }
    let parent = h.enqueue("Parent change").await;
    let child = child(&h, &[&parent]).await;
    h.dispatch().await;
    let tree = h.run(&parent.id).await;
    commit(&tree.workspace, "parent.txt", "parent\n");
    finish_item(&h, &parent.id).await;
    let child_tree = h.run(&child.id).await;
    assert!(child_tree.workspace.join("parent.txt").exists());
    assert_eq!(git(&h.repo, &["branch", "--show-current"]), "main");
    (h, parent, child)
}

#[tokio::test]
async fn child_forks_parent_targets_it_and_retargets_after_merge_across_restart() {
    let (mut h, parent, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    assert_eq!(prs(&h)[&child_run.branch]["baseRefName"], parent_run.branch);
    set_pr(&h, &parent_run.branch, "state", json!("MERGED"));
    h.restart();
    h.dispatch().await;
    assert_eq!(prs(&h)[&child_run.branch]["baseRefName"], "main");
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&child.id)
            .unwrap()
            .is_none()
    );
    h.restart();
    h.dispatch().await;
    assert_eq!(prs(&h)[&child_run.branch]["baseRefName"], "main");
}

#[tokio::test]
async fn a_moved_parent_is_merged_forward_and_pushed_without_rewriting_child() {
    let (h, parent, child) = stack_pair().await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    let child_head = commit(&child_run.workspace, "child.txt", "child\n");
    finish_item(&h, &child.id).await;
    let new_parent = advance_branch(&h, &parent_run.branch, "parent2.txt", "new parent\n");
    h.dispatch().await;
    let merged = git(&h.repo, &["rev-parse", &child_run.branch]);
    assert_ne!(merged, child_head);
    git(
        &h.repo,
        &["merge-base", "--is-ancestor", &child_head, &merged],
    );
    git(
        &h.repo,
        &["merge-base", "--is-ancestor", &new_parent, &merged],
    );
    assert_eq!(
        git(
            &h.repo,
            &[
                "rev-parse",
                &format!("refs/remotes/origin/{}", child_run.branch)
            ]
        ),
        merged
    );
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&child.id)
            .unwrap()
            .unwrap()
            .1,
        new_parent
    );
}

#[tokio::test]
async fn a_merge_conflict_enqueues_one_fix_on_the_child_branch_and_survives_restart() {
    let (mut h, parent, child) = stack_pair().await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    commit(&child_run.workspace, "parent.txt", "child edits\n");
    finish_item(&h, &child.id).await;
    advance_branch(&h, &parent_run.branch, "parent.txt", "parent edits\n");
    h.dispatch().await;
    let fixes = h
        .ctx
        .store
        .lock()
        .await
        .kv_prefix(&format!("restack_fix:{}:", child.id))
        .unwrap();
    assert_eq!(fixes.len(), 1);
    let fix_id = archon_core::id::QueueItemId::parse(&fixes[0].1).unwrap();
    assert_eq!(h.run(&fix_id).await.branch, child_run.branch);
    assert!(h.item(&fix_id).await.brief.contains("parent.txt"));
    h.cli(Request::Gc {
        older_than_secs: Some(0),
        force: true,
    })
    .await
    .unwrap();
    assert!(
        child_run.workspace.exists(),
        "gc preserves the pending fix's workspace"
    );
    h.restart();
    archon_daemon::executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    let fixes = h
        .ctx
        .store
        .lock()
        .await
        .kv_prefix(&format!("restack_fix:{}:", child.id))
        .unwrap();
    assert_eq!(fixes.len(), 1);
    let tree = h.run(&fix_id).await;
    commit(&tree.workspace, "parent.txt", "combined\n");
    finish_item(&h, &fix_id).await;
    h.dispatch().await;
    assert!(matches!(
        h.state(&fix_id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(
        h.ctx.store.lock().await.stack_parents(&child.id).unwrap()[0]
            .conflict
            .is_none()
    );
}

#[tokio::test]
async fn several_parents_fork_the_first_and_merge_the_rest() {
    let h = Harness::new().await;
    install_gh(&h);
    let one = h.enqueue("First parent").await;
    let two = h.enqueue("Second parent").await;
    let joined = child(&h, &[&one, &two]).await;
    h.dispatch().await;
    let first = h.run(&one.id).await;
    commit(&first.workspace, "one.txt", "first\n");
    finish_item(&h, &one.id).await;
    h.dispatch().await;
    let second = h.run(&two.id).await;
    commit(&second.workspace, "two.txt", "second\n");
    finish_item(&h, &two.id).await;
    let run = h.run(&joined.id).await;
    assert!(run.workspace.join("one.txt").exists());
    assert!(run.workspace.join("two.txt").exists());
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&joined.id)
            .unwrap()
            .unwrap()
            .0,
        first.branch
    );
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .stack_parents(&joined.id)
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn queued_work_starts_without_waiting_for_github_maintenance() {
    let (h, _, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let next = h.enqueue("Independent work").await;
    std::fs::write(&h.ctx.config.github.binary, "#!/bin/sh\nexit 1\n").unwrap();
    archon_daemon::dispatcher::dispatch_once(&h.ctx)
        .await
        .unwrap();
    assert!(h.state(&next.id).await.is_active());
    assert!(
        archon_daemon::executor::coordination::reconcile(&h.ctx)
            .await
            .is_err()
    );
    assert!(h.state(&next.id).await.is_active());
}

#[tokio::test]
async fn a_remote_parent_head_is_fetched_and_merged_without_moving_the_local_parent() {
    let (h, parent, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    let local_head = git(&h.repo, &["rev-parse", &parent_run.branch]);
    git(&h.repo, &["push", "origin", &parent_run.branch]);
    let other = h.dir.path().join("other-clone");
    git(
        &h.repo,
        &[
            "clone",
            h.dir.path().join("remote.git").to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    git(&other, &["checkout", &parent_run.branch]);
    let remote_head = commit(&other, "remote-parent.txt", "from another checkout\n");
    git(&other, &["push", "origin", &parent_run.branch]);
    set_pr(&h, &parent_run.branch, "headRefOid", json!(remote_head));
    h.dispatch().await;
    assert_eq!(git(&h.repo, &["rev-parse", &parent_run.branch]), local_head);
    git(
        &h.repo,
        &[
            "merge-base",
            "--is-ancestor",
            &remote_head,
            &child_run.branch,
        ],
    );
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&child.id)
            .unwrap()
            .unwrap()
            .1,
        remote_head
    );
}

#[tokio::test]
async fn a_parent_merge_updates_the_base_while_the_child_is_still_working() {
    let (h, parent, child) = stack_pair().await;
    let parent_run = h.run(&parent.id).await;
    set_pr(&h, &parent_run.branch, "state", json!("MERGED"));
    h.dispatch().await;
    assert!(matches!(
        h.state(&child.id).await,
        QueueState::Running { .. }
    ));
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&child.id)
            .unwrap()
            .is_none()
    );
    finish_item(&h, &child.id).await;
    let ship = h
        .backend
        .requests()
        .into_iter()
        .find(|s| {
            s.labels["archon.item"] == child.id.as_str() && s.labels["archon.stage"] == "ship"
        })
        .unwrap();
    let rule = "the change's pull request is for `main`";
    assert!(ship.instructions.contains(rule), "{}", ship.instructions);
    assert_eq!(
        prs(&h)[h.run(&child.id).await.branch]["baseRefName"],
        "main"
    );
}

#[tokio::test]
async fn a_merged_child_is_not_changed_by_later_parent_commits() {
    let (h, parent, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    let before = git(&h.repo, &["rev-parse", &child_run.branch]);
    set_pr(&h, &child_run.branch, "state", json!("MERGED"));
    advance_branch(&h, &parent_run.branch, "after-child.txt", "later\n");
    h.dispatch().await;
    assert_eq!(git(&h.repo, &["rev-parse", &child_run.branch]), before);
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .kv_prefix(&format!("restack_fix:{}:", child.id))
            .unwrap()
            .is_empty()
    );
}
