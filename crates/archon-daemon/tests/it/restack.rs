//! Real branches with a durable fake GitHub service.
use crate::common;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_daemon::executor::coordination::reconcile;
use common::delivery::plant_everywhere;
use common::initiative::{commit, finish_item, gh_calls, install_gh, prs, set_pr};
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
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: "Child change".into(),
        brief: "Build on the parent.".into(),
        priority: Default::default(),
        depends_on: parents.iter().map(|p| p.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: plan().into(),
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

/// What a fix agent does: merge `sha`, resolve `file` as `content`, commit.
fn merge_by_hand(tree: &std::path::Path, sha: &str, file: &str, content: &str) {
    let merging = std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args([
            "-c",
            "user.name=Archon Test",
            "-c",
            "user.email=test@archon.invalid",
        ])
        .args(["merge", "--quiet", sha])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        !merging.status.success(),
        "the fix meets the conflict Archon met"
    );
    std::fs::write(tree.join(file), content).unwrap();
    git(tree, &["add", file]);
    git(tree, &["commit", "--quiet", "--no-edit"]);
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

/// The restack fixes recorded for `item`: each parent commit and its fix.
async fn restack_fixes(h: &Harness, item: &QueueItemId) -> Vec<(String, String)> {
    let prefix = format!("restack_fix:{item}:");
    h.ctx.store.lock().await.kv_prefix(&prefix).unwrap()
}

async fn stack_pair() -> (Harness, QueueItem, QueueItem) {
    stack_pair_with(Config::default()).await
}

async fn stack_pair_with(config: Config) -> (Harness, QueueItem, QueueItem) {
    let h = Harness::with_config(config).await;
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
    let remote = h.dir.path().join("remote.git");
    let pushed = format!("refs/heads/{}", child_run.branch);
    assert_eq!(git(&remote, &["rev-parse", &pushed]), merged);
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
    let child_head = commit(&child_run.workspace, "parent.txt", "child edits\n");
    finish_item(&h, &child.id).await;
    let new_parent = advance_branch(&h, &parent_run.branch, "parent.txt", "parent edits\n");
    h.dispatch().await;
    let branch = format!("refs/heads/{}", child_run.branch);
    assert_eq!(
        git(&h.repo, &["rev-parse", &branch]),
        child_head,
        "a conflict writes nothing"
    );
    assert!(git(&child_run.workspace, &["status", "--porcelain"]).is_empty());
    let fixes = restack_fixes(&h, &child.id).await;
    assert_eq!(fixes.len(), 1);
    let fix_id = QueueItemId::parse(&fixes[0].1).unwrap();
    assert_eq!(h.run(&fix_id).await.branch, child_run.branch);
    let brief = h.item(&fix_id).await.brief;
    assert!(brief.contains("parent.txt"), "{brief}");
    assert!(
        brief.contains(&format!("git merge {new_parent}")),
        "{brief}"
    );
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
    let fixes = restack_fixes(&h, &child.id).await;
    assert_eq!(fixes.len(), 1);
    let tree = h.run(&fix_id).await;
    merge_by_hand(&tree.workspace, &new_parent, "parent.txt", "combined\n");
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
    archon_daemon::executor::coordination::reconcile(&h.ctx)
        .await
        .expect("a GitHub outage is one stack's blocker, not the pass's failure");
    assert!(h.state(&next.id).await.is_active());
    let stuck = h
        .kv(&format!("coordination_blocker:stack:{}", child.id))
        .await;
    assert!(stuck.is_some_and(|s| s.contains("\"failures\":1")));
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
    assert!(restack_fixes(&h, &child.id).await.is_empty());
}

/// The GitHub calls one maintenance pass made.
async fn pass(h: &Harness) -> Vec<Vec<String>> {
    let before = gh_calls(h).len();
    reconcile(&h.ctx).await.unwrap();
    gh_calls(h)[before..].to_vec()
}

/// One pass asks GitHub once for the repository's open pull requests; one
/// that merged is looked up once, then never asked about again.
#[tokio::test]
async fn a_pass_lists_open_pull_requests_once_and_a_settled_one_never_again() {
    let (h, parent, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    let batch =
        |call: &Vec<String>| call[..2] == ["pr", "list"] && !call.contains(&"--head".into());
    let calls = pass(&h).await;
    assert!(calls.len() == 1 && batch(&calls[0]), "{calls:?}");

    set_pr(&h, &parent_run.branch, "state", json!("MERGED"));
    let calls = pass(&h).await;
    assert_eq!(calls.iter().filter(|c| batch(c)).count(), 1, "{calls:?}");
    assert_eq!(prs(&h)[&child_run.branch]["baseRefName"], "main");
    let calls = pass(&h).await;
    assert!(
        calls.len() == 1 && batch(&calls[0]),
        "the merged parent is settled: {calls:?}"
    );

    set_pr(&h, &child_run.branch, "state", json!("MERGED"));
    pass(&h).await;
    assert_eq!(
        pass(&h).await,
        Vec::<Vec<String>>::new(),
        "nothing is open to ask about"
    );
}

/// A restack fix works on its item's branch: it waits while the item runs,
/// and the item is not retried while its fix is still to finish.
#[tokio::test]
async fn a_restack_fix_and_its_item_never_run_at_once() {
    let (h, parent, child) = stack_pair().await;
    let parent_run = h.run(&parent.id).await;
    let sha = git(&h.repo, &["rev-parse", &parent_run.branch]);
    let fix = {
        let mut store = h.ctx.store.lock().await;
        let item = store.item(&child.id).unwrap();
        let run = store.runs_for_item(&child.id).unwrap().pop().unwrap();
        let stacked = store.stack_parents(&child.id).unwrap().remove(0);
        store
            .enqueue_stack_fix(&item, &run, &stacked, &sha, "parent.txt")
            .unwrap()
    };
    h.dispatch().await;
    assert_eq!(
        h.state(&fix).await,
        QueueState::Queued,
        "it waits for the running item"
    );

    finish_item(&h, &child.id).await;
    let retried = h
        .cli(Request::RetryItem {
            item: child.id.clone(),
            model_plan: None,
            model_reasons: None,
            fresh: false,
            brief_append: None,
        })
        .await;
    let Err(ArchonError::InvalidTransition(why)) = retried else {
        panic!("the retry is refused: {retried:?}");
    };
    assert!(why.contains("restack fix still to finish"), "{why}");
    h.dispatch().await;
    assert!(h.state(&fix).await.is_active(), "{:?}", h.state(&fix).await);
}

/// Admitted in one pass, whose starts run in parallel lanes, a restack fix
/// and its item still never start together: the fix goes first.
#[tokio::test]
async fn a_restack_fix_and_its_item_admitted_together_start_one_at_a_time() {
    let (h, parent, child) = stack_pair().await;
    finish_item(&h, &child.id).await;
    let sha = git(&h.repo, &["rev-parse", &h.run(&parent.id).await.branch]);
    let fix = {
        let mut store = h.ctx.store.lock().await;
        let item = store.item(&child.id).unwrap();
        let run = store.runs_for_item(&child.id).unwrap().pop().unwrap();
        let stacked = store.stack_parents(&child.id).unwrap().remove(0);
        let fix = store
            .enqueue_stack_fix(&item, &run, &stacked, &sha, "parent.txt")
            .unwrap();
        // As a revival would: the item is queued again beside its fix.
        let now = archon_core::now();
        store
            .transition_item(&child.id, QueueState::Queued, now)
            .unwrap();
        fix
    };
    archon_daemon::dispatcher::dispatch_once(&h.ctx)
        .await
        .expect("dispatch pass");

    assert!(h.state(&fix).await.is_active(), "{:?}", h.state(&fix).await);
    assert_eq!(
        h.state(&child.id).await,
        QueueState::Queued,
        "the item waits for its fix"
    );
    let live = h.ctx.store.lock().await.runs_for_item(&child.id).unwrap();
    assert!(
        live.iter().all(|run| run.state.is_terminal()),
        "no second run"
    );
}

/// One stacked item whose maintenance fails, its branch checked out where
/// archond may not move it, is its own blocker, told once: the next item
/// is still restacked, on this pass and every later one.
#[tokio::test]
async fn one_stuck_item_is_told_once_and_holds_up_no_other() {
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
    let stuck = child(&h, &[&parent]).await;
    let moving = child(&h, &[&parent]).await;
    h.dispatch().await;
    commit(&h.run(&parent.id).await.workspace, "parent.txt", "parent\n");
    finish_item(&h, &parent.id).await;
    finish_item(&h, &stuck.id).await;
    finish_item(&h, &moving.id).await;
    let parent_run = h.run(&parent.id).await;
    let stuck_run = h.run(&stuck.id).await;
    let elsewhere = h.dir.path().join("operator-checkout");
    let elsewhere = elsewhere.to_str().unwrap();
    git(&h.repo, &["worktree", "add", elsewhere, &stuck_run.branch]);
    let new_parent = advance_branch(&h, &parent_run.branch, "parent2.txt", "new parent\n");

    let what = format!("Restacking item {} is stuck", stuck.id);
    for passes in 1..=3 {
        reconcile(&h.ctx).await.expect("the pass goes on");
        let merged = git(&h.repo, &["rev-parse", &h.run(&moving.id).await.branch]);
        git(
            &h.repo,
            &["merge-base", "--is-ancestor", &new_parent, &merged],
        );
        let blocker = h
            .kv(&format!("coordination_blocker:stack:{}", stuck.id))
            .await;
        assert!(blocker.is_some_and(|b| b.contains(&format!("\"failures\":{passes}"))));
        let blockers = h.notices(NotificationKind::Blocker).await;
        let told = blockers.iter().filter(|n| n.contains(&what)).count();
        assert_eq!(told, 1, "told once, on pass {passes}: {blockers:?}");
    }
    git(&h.repo, &["worktree", "remove", elsewhere]);
    reconcile(&h.ctx).await.unwrap();
    let merged = git(&h.repo, &["rev-parse", &stuck_run.branch]);
    git(
        &h.repo,
        &["merge-base", "--is-ancestor", &new_parent, &merged],
    );
    assert_eq!(
        h.kv(&format!("coordination_blocker:stack:{}", stuck.id))
            .await,
        None
    );
}

/// A restack merges in the shadow, moves the kept worktree with its branch
/// and pushes, running nothing the repository, the worktree or the remote
/// configures.
#[tokio::test]
async fn a_restack_runs_no_program_the_repository_or_its_remote_configures() {
    let mut config = Config::default();
    config.worktrees.keep_on_success = true;
    let (h, parent, child) = stack_pair_with(config).await;
    let parent_run = h.run(&parent.id).await;
    let child_run = h.run(&child.id).await;
    let attributes = "* filter=planted merge=planted\n";
    std::fs::write(child_run.workspace.join(".gitattributes"), attributes).unwrap();
    git(&child_run.workspace, &["add", ".gitattributes"]);
    commit(&child_run.workspace, "child.txt", "child\n");
    finish_item(&h, &child.id).await;
    assert!(child_run.workspace.exists(), "kept on success");
    let new_parent = commit(&parent_run.workspace, "parent2.txt", "new parent\n");
    let marker = h.dir.path().join("planted-ran");
    let remote = h.dir.path().join("remote.git");
    plant_everywhere(&h.repo, &remote, &marker);

    reconcile(&h.ctx).await.unwrap();
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(ran.is_empty(), "a planted program ran as the daemon: {ran}");
    let merged = git(&h.repo, &["rev-parse", &child_run.branch]);
    git(
        &h.repo,
        &["merge-base", "--is-ancestor", &new_parent, &merged],
    );
    let moved = git(&child_run.workspace, &["rev-parse", "HEAD"]);
    assert_eq!(moved, merged, "the kept worktree moved with its branch");
    assert!(child_run.workspace.join("parent2.txt").exists());
    let pushed = format!("refs/heads/{}", child_run.branch);
    assert_eq!(git(&remote, &["rev-parse", &pushed]), merged);
}
