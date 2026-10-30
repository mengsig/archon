//! A contributor's pull request from a fork: a `review` item onto it pushes
//! nothing, records the head its reviewers passed, and lets the operator merge
//! the pull request at exactly that head; an item stacked on it ships those
//! commits unchanged beneath its own and nothing else foreign. A bare
//! `origin` holding the fork's head as `refs/pull/200/head`, and a fake `gh`.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::merge::{MergeRequest, MergeState};
use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::pull_request::OntoTarget;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::StageOutcome;
use archon_daemon::executor::coordination;
use common::delivery::{FakeGh, add_origin, origin_of, pushed, record, shipped_by};
use common::{Harness, REPO_NAME, REVIEW_MODEL, git};
use serde_json::{Value, json};

const FORK_PR: &str = "https://github.com/o/r/pull/200";
const CONTRIBUTOR: &str = "contributor@example.com";

fn green() -> Value {
    json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}])
}

/// A harness with an `origin`, a scripted `gh`, and pull request #200 from
/// `contributor/r`'s `patch`: one commit by [`CONTRIBUTOR`] past `main`,
/// which `origin` holds only as `refs/pull/200/head`. Returns that head.
async fn fork_pull_request() -> (Harness, FakeGh, String) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    let head = contributor_commit(&h, "patch.txt", "the contributor's patch\n");
    gh.headed(
        200,
        &json!({
            "id": "PR_node200",
            "number": 200,
            "url": FORK_PR,
            "state": "OPEN",
            "headRefName": "patch",
            "headRefOid": head,
            "baseRefName": "main",
            "isCrossRepository": true,
            "headRepository": {"name": "r"},
            "headRepositoryOwner": {"login": "contributor"},
        }),
    );
    gh.show(&head, "MERGEABLE", green());
    (h, gh, head)
}

/// One commit by [`CONTRIBUTOR`] on top of what the fork's clone holds,
/// made in a clone the harness repository never sees, and published as
/// GitHub publishes a fork's head: `origin`'s `refs/pull/200/head`.
fn contributor_commit(h: &Harness, file: &str, text: &str) -> String {
    let fork = h.dir.path().join("fork");
    if !fork.exists() {
        let origin = origin_of(h);
        let (from, to) = (origin.to_str().unwrap(), fork.to_str().unwrap());
        git(h.dir.path(), &["clone", "--quiet", from, to]);
    }
    std::fs::write(fork.join(file), text).unwrap();
    git(&fork, &["add", file]);
    let identity = ["-c", "user.name=Contributor", "-c"];
    let email = format!("user.email={CONTRIBUTOR}");
    let commit = [
        &identity[..],
        &[email.as_str(), "commit", "--quiet", "-m", file],
    ]
    .concat();
    git(&fork, &commit);
    git(
        &fork,
        &[
            "push",
            "--quiet",
            "--force",
            "origin",
            "HEAD:refs/pull/200/head",
        ],
    );
    git(&fork, &["rev-parse", "HEAD"])
}

fn enqueue_request(title: &str, pipeline: &str, review: ReviewLevel) -> EnqueueRequest {
    EnqueueRequest {
        onto: None,
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        stage_timeouts: Default::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(review),
            ..AutonomyPatch::default()
        },
        origin: None,
    }
}

async fn enqueue(h: &Harness, request: EnqueueRequest) -> Result<QueueItem, ArchonError> {
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

/// A `review` item onto #200 at `review`, dispatched: its lead runs on a
/// branch at the fork's head, told what it reviews.
async fn review_item(h: &Harness, head: &str, review: ReviewLevel) -> QueueItem {
    let mut asked = enqueue_request("review #200", "review", review);
    asked.onto = Some(OntoTarget::Pr(200));
    let item = enqueue(h, asked).await.expect("a fork's pull request");
    let onto = item.onto.as_deref().expect("frozen");
    assert!(onto.review_only && onto.is_fork(), "{onto:?}");
    assert_eq!(onto.head_sha, head);
    h.dispatch().await;
    let workspace = h.run(&item.id).await.workspace;
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head);
    let told = &h.backend.requests().remove(0).instructions;
    assert!(told.contains("\n## Pull request under review\n"), "{told}");
    assert!(told.contains(FORK_PR), "{told}");
    item
}

/// The review lead completes, its one reviewer passing `head` from the
/// pull request's fork point.
async fn pass_review(h: &Harness, item: &QueueItem, head: &str) {
    let lead = h.running().await.remove(0);
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let workspace = h.run(&item.id).await.workspace;
    let base = git(&workspace, &["merge-base", "HEAD", "origin/main"]);
    let verdict = json!({"verdict": "pass", "head": head, "base": base, "findings": []});
    h.backend.complete_delegated(&reviewer, verdict);
    let passed = StageOutcome::Completed {
        summary: format!("Reviewed {FORK_PR} at {head}; checks green"),
        structured: Some(json!({"review": {"verdict": "pass", "reviewers": [reviewer.as_str()]}})),
    };
    h.settle_running(|_| passed.clone()).await;
    h.dispatch().await;
}

/// A succeeded review of #200 that passed `head`, with merges granted.
async fn reviewed_fork() -> (Harness, FakeGh, String, QueueItem) {
    let (h, gh, head) = fork_pull_request().await;
    grant_merge(&h).await;
    let item = review_item(&h, &head, ReviewLevel::Standard).await;
    pass_review(&h, &item, &head).await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    (h, gh, head, item)
}

async fn grant_merge(h: &Harness) {
    let patch = RepoPatch {
        grant_merge: Some(true),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

fn request_merge(item: &QueueItem) -> Request {
    Request::RequestMerge {
        item: item.id.clone(),
        reason: Some("the operator said: merge #200".into()),
    }
}

fn merge_item(item: &QueueItem, ignore_checks: bool) -> Request {
    Request::MergeItem {
        item: item.id.clone(),
        ignore_checks,
        chat: false,
    }
}

async fn merges(h: &Harness, item: &QueueItem) -> Vec<MergeRequest> {
    h.ctx.store.lock().await.merges_for_item(&item.id).unwrap()
}

fn merge_of(reply: Result<Reply, ArchonError>) -> MergeRequest {
    match reply {
        Ok(Reply::Merge(merge)) => *merge,
        other => panic!("a merge request: {other:?}"),
    }
}

/// Every ref `origin` holds, as `<commit> <name>` lines.
fn refs_of(h: &Harness) -> String {
    git(
        &origin_of(h),
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    )
}

/// Nothing went to the fork: `origin` holds the fork's head where it was,
/// and no `gh` call wrote to #200 other than the merge.
fn nothing_pushed_to_the_fork(h: &Harness, gh: &FakeGh, head: &str) {
    let refs = refs_of(h);
    assert!(
        refs.contains(&format!("{head} refs/pull/200/head")),
        "{refs}"
    );
    assert_eq!(pushed(&origin_of(h), "patch"), None, "{refs}");
    for call in gh.calls() {
        let writes = call.starts_with("pr edit") || call.starts_with("pr create");
        assert!(!(writes && call.contains("/pull/200")), "{call}");
    }
}

#[tokio::test]
async fn a_passed_review_of_a_fork_pull_request_lets_request_merge_merge_it_at_that_head() {
    let (h, gh, head, item) = reviewed_fork().await;
    let before = refs_of(&h);

    let merge = merge_of(h.orchestrator(request_merge(&item)).await);

    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(
        (merge.pr_url.as_str(), merge.head.as_str()),
        (FORK_PR, head.as_str())
    );
    let merges: Vec<String> = gh
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("pr merge"))
        .collect();
    let pinned = format!("pr merge {FORK_PR} --match-head-commit {head} --squash");
    assert_eq!(merges, [pinned]);
    assert!(gh.merged());
    assert_eq!(refs_of(&h), before, "the review pushed nothing");
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

#[tokio::test]
async fn a_head_that_moved_after_the_review_is_never_merged() {
    let (h, gh, head, item) = reviewed_fork().await;
    let moved = contributor_commit(&h, "later.txt", "pushed after the review\n");
    gh.show(&moved, "MERGEABLE", green());

    let refused = h.orchestrator(request_merge(&item)).await.unwrap_err();
    let why = refused.to_string();
    assert!(
        why.contains("moved to") && why.contains(&head[..7]),
        "{why}"
    );
    assert!(why.contains(&moved[..7]), "{why}");

    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    let MergeState::Refused { reason } = &merge.state else {
        panic!("the operator's merge is refused too: {merge:?}");
    };
    assert!(reason.contains("moved to"), "{reason}");
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr merge")));
}

/// A refresh would refreeze or merge the moved head under the old review:
/// it is refused, the frozen head stays, and the merge stays refused.
#[tokio::test]
async fn a_review_item_is_never_refreshed_onto_a_moved_head() {
    let (h, gh, head, item) = reviewed_fork().await;
    let moved = contributor_commit(&h, "later.txt", "pushed after the review\n");
    gh.show(&moved, "MERGEABLE", green());

    let refresh = Request::RefreshItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        stage_timeouts: Default::default(),
    };
    let refused = h.orchestrator(refresh).await.unwrap_err().to_string();
    assert!(refused.contains("queue a new `review` item"), "{refused}");
    assert!(refused.contains(&head), "{refused}");

    let now = h.item(&item.id).await;
    assert_eq!(now.onto.as_deref().unwrap().head_sha, head, "still frozen");
    assert!(h.orchestrator(request_merge(&item)).await.is_err());
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr merge")));
    nothing_pushed_to_the_fork(&h, &gh, &moved);
}

#[tokio::test]
async fn another_s_pull_request_merges_only_with_its_checks_green() {
    let (h, gh, head, item) = reviewed_fork().await;
    let red = json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}]);
    gh.show(&head, "MERGEABLE", red);

    let merge = merge_of(h.cli(merge_item(&item, true)).await);

    let MergeState::Refused { reason } = &merge.state else {
        panic!("refused: {merge:?}");
    };
    assert!(
        reason.contains("merges only with every check green"),
        "{reason}"
    );
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr merge")));
}

/// A check that fails while the merge method is read stops the merge: the
/// checks are read last, just before `gh pr merge`.
#[tokio::test]
async fn a_check_that_fails_just_before_the_merge_stops_it() {
    let (h, gh, _head, item) = reviewed_fork().await;
    let red = json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}]);
    gh.checks_after_repo_view(&red);

    let merge = merge_of(h.cli(merge_item(&item, false)).await);

    let MergeState::Refused { reason } = &merge.state else {
        panic!("refused: {merge:?}");
    };
    assert!(reason.contains("1 failed (ci)"), "{reason}");
    assert!(gh.calls().iter().any(|c| c.starts_with("repo view")));
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr merge")));
}

/// Under `grant_merge`, a request made while checks run waits for them, as
/// for Archon's own pull requests, and merges once they are green.
#[tokio::test]
async fn a_granted_merge_of_a_reviewed_pull_request_waits_for_pending_checks() {
    let (h, gh, head, item) = reviewed_fork().await;
    let running = json!([{"__typename": "CheckRun", "name": "ci", "status": "IN_PROGRESS", "conclusion": null}]);
    gh.show(&head, "MERGEABLE", running);
    let waiting = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(waiting.state, MergeState::AwaitingChecks);

    coordination::reconcile(&h.ctx).await.unwrap();
    let [still] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(still.state, MergeState::AwaitingChecks, "still pending");
    assert!(!gh.merged());

    gh.show(&head, "MERGEABLE", green());
    coordination::reconcile(&h.ctx).await.unwrap();
    let [done] = merges(&h, &item)
        .await
        .try_into()
        .expect("the same request");
    assert_eq!(
        (&done.id, done.state.kind()),
        (&waiting.id, "merged"),
        "{done:?}"
    );
    assert!(gh.merged());
}

#[tokio::test]
async fn a_review_item_without_a_review_on_record_merges_nothing() {
    let (h, gh, head) = fork_pull_request().await;
    grant_merge(&h).await;
    let item = review_item(&h, &head, ReviewLevel::None).await;
    let read = StageOutcome::Completed {
        summary: "Read it; checks green".into(),
        structured: None,
    };
    h.settle_running(|_| read.clone()).await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");

    let refused = h.orchestrator(request_merge(&item)).await.unwrap_err();

    assert!(
        refused
            .to_string()
            .contains("holds no review that passed it"),
        "{refused}"
    );
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr merge")));
}

#[tokio::test]
async fn a_review_whose_branch_moved_off_the_frozen_head_is_refused() {
    let (h, gh, head) = fork_pull_request().await;
    let item = review_item(&h, &head, ReviewLevel::Standard).await;
    let lead = h.running().await.remove(0);
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let named = json!({"review": {"verdict": "pass", "reviewers": [reviewer.as_str()]}});
    let changed = shipped_by(&h, &lead, "Fixed it while reviewing", named).await;
    let StageOutcome::Completed {
        structured: Some(made),
        ..
    } = &changed
    else {
        panic!("a change: {changed:?}");
    };
    let verdict = json!({"verdict": "pass", "head": made["commit"], "findings": []});
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| changed.clone()).await;
    h.dispatch().await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("a review item commits nothing"), "{error}");
    assert!(error.contains(&head), "{error}");
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

/// An item that `depends_on` the passed review, dispatched: its branch
/// starts at the reviewed head, beneath which it is told to change nothing.
async fn stacked_on(h: &Harness, review: &QueueItem, head: &str) -> QueueItem {
    let mut asked = enqueue_request("build on #200", "direct", ReviewLevel::None);
    asked.depends_on = vec![review.id.clone()];
    let item = enqueue(h, asked).await.expect("stacked");
    h.dispatch().await;
    let workspace = h.run(&item.id).await.workspace;
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head);
    let requests = h.backend.requests();
    let told = &requests.last().unwrap().instructions;
    assert!(
        told.contains("## Stacked on a reviewed pull request"),
        "{told}"
    );
    item
}

/// The stacked item's stage completes with `commit` having shaped its worktree.
async fn settle_stacked(h: &Harness, item: &QueueItem, commit: impl FnOnce(&std::path::Path)) {
    let workspace = h.run(&item.id).await.workspace;
    commit(&workspace);
    let made = StageOutcome::Completed {
        summary: "Built on #200; cargo test passes".into(),
        structured: Some(record(&workspace)),
    };
    h.settle_running(|_| made.clone()).await;
    h.dispatch().await;
}

/// One more commit in `workspace` under the repository's git identity.
fn own_commit(workspace: &std::path::Path) {
    std::fs::write(workspace.join("own.txt"), "built on the patch\n").unwrap();
    git(workspace, &["add", "own.txt"]);
    git(
        workspace,
        &["commit", "--quiet", "-m", "Build on the patch"],
    );
}

#[tokio::test]
async fn a_stack_on_reviewed_fork_commits_ships_them_unchanged_in_a_pull_request_of_its_own() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;

    settle_stacked(&h, &item, own_commit).await;

    let state = h.state(&item.id).await;
    let QueueState::Succeeded { summary, .. } = &state else {
        panic!("the stack ships: {state:?}");
    };
    assert!(
        summary.contains("https://github.com/o/r/pull/1"),
        "{summary}"
    );
    let run = h.run(&item.id).await;
    let shipped = pushed(&origin_of(&h), &run.branch).expect("its own branch");
    git(
        &origin_of(&h),
        &["merge-base", "--is-ancestor", &head, &shipped],
    );
    assert!(
        gh.calls()
            .iter()
            .any(|c| c.starts_with("pr create --base main"))
    );
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

#[tokio::test]
async fn a_stack_that_rewrites_the_contributor_s_commit_is_refused() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;

    settle_stacked(&h, &item, |workspace| {
        std::fs::write(workspace.join("patch.txt"), "reworded by Archon\n").unwrap();
        git(
            workspace,
            &["commit", "--quiet", "--amend", "-am", "patch.txt, reworded"],
        );
        own_commit(workspace);
    })
    .await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains(&format!("does not hold {head}")), "{error}");
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr create")));
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

/// One commit in `workspace` by `other@example.com`, whom no review passed.
fn other_commit(workspace: &std::path::Path) {
    std::fs::write(workspace.join("other.txt"), "someone else\n").unwrap();
    git(workspace, &["add", "other.txt"]);
    let identity = [
        "-c",
        "user.name=Other",
        "-c",
        "user.email=other@example.com",
    ];
    let commit = [&identity[..], &["commit", "--quiet", "-m", "Other"]].concat();
    git(workspace, &commit);
}

#[tokio::test]
async fn a_stack_carrying_another_foreign_commit_is_refused() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;

    settle_stacked(&h, &item, |workspace| {
        own_commit(workspace);
        other_commit(workspace);
    })
    .await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("other@example.com"), "{error}");
    assert!(
        !error.contains(CONTRIBUTOR),
        "the reviewed commit is not foreign: {error}"
    );
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr create")));
}

#[tokio::test]
async fn a_stack_carrying_a_foreign_commit_a_remote_branch_holds_is_refused() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;

    settle_stacked(&h, &item, |workspace| {
        other_commit(workspace);
        git(
            workspace,
            &["update-ref", "refs/remotes/origin/unrelated", "HEAD"],
        );
        own_commit(workspace);
    })
    .await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("other@example.com"), "{error}");
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr create")));
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

/// Local base refs are the worktree's to move: only `origin`'s base head,
/// read at delivery, exempts a commit from the identity check.
#[tokio::test]
async fn a_stack_whose_base_refs_were_moved_over_a_foreign_commit_is_refused() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;

    settle_stacked(&h, &item, |workspace| {
        other_commit(workspace);
        for base in ["refs/heads/main", "refs/remotes/origin/main"] {
            git(workspace, &["update-ref", base, "HEAD"]);
        }
        own_commit(workspace);
    })
    .await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("other@example.com"), "{error}");
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr create")));
    nothing_pushed_to_the_fork(&h, &gh, &head);
}

#[tokio::test]
async fn a_stack_on_a_review_whose_pull_request_moved_since_is_refused_at_ship() {
    let (h, gh, head, review) = reviewed_fork().await;
    let item = stacked_on(&h, &review, &head).await;
    let moved = contributor_commit(&h, "later.txt", "pushed after the review\n");
    gh.show(&moved, "MERGEABLE", green());

    settle_stacked(&h, &item, own_commit).await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(
        error.contains(&format!("its head moved to {moved}")),
        "{error}"
    );
    assert!(!gh.calls().iter().any(|c| c.starts_with("pr create")));
}

#[tokio::test]
async fn a_stack_on_a_review_and_another_unmerged_parent_is_refused() {
    let (h, _gh, _head, review) = reviewed_fork().await;
    let parent = enqueue(&h, enqueue_request("a parent", "direct", ReviewLevel::None))
        .await
        .unwrap();
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let built = shipped_by(&h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    let state = h.state(&parent.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");

    let mut asked = enqueue_request("on both", "direct", ReviewLevel::None);
    asked.depends_on = vec![review.id.clone(), parent.id.clone()];
    let item = enqueue(&h, asked).await.unwrap();
    h.dispatch().await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("cannot also stack"), "{error}");
    assert!(
        error.contains(parent.id.as_str()) && error.contains(FORK_PR),
        "{error}"
    );
}
