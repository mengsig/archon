//! `archon merge` and `archon diff` for a `ship = local` repository: the
//! item's branch is fast-forwarded into its base, never forced, and only
//! when the operator asked or confirmed. Real git; no GitHub.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::merge::{LocalReview, MergeAuthority, MergeRequest, MergeState};
use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use common::delivery::shipped_by;
use common::{Harness, REPO_NAME, git};

async fn delivered_locally(h: &Harness) -> (QueueItem, String, String) {
    set_local(h).await;
    deliver_local(h, "a local change", Vec::new()).await
}

async fn set_local(h: &Harness) {
    let local = RepoPatch {
        ship: Some(ShipMode::Local),
        ..RepoPatch::default()
    };
    let update = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: local,
    };
    h.cli(update).await.expect("ship = local");
}

async fn deliver_local(
    h: &Harness,
    title: &str,
    depends_on: Vec<archon_core::id::QueueItemId>,
) -> (QueueItem, String, String) {
    let request = EnqueueRequest {
        onto: None,
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: title.into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on,
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        stage_timeouts: Default::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(ReviewLevel::None),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    let Ok(Reply::Item(item)) = h.cli(Request::Enqueue(request)).await else {
        panic!("enqueue failed");
    };
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it", serde_json::json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let branch = h.run(&item.id).await.branch;
    let head = git(&h.repo, &["rev-parse", &branch]);
    (item, branch, head)
}

fn merge_item(item: &QueueItem) -> Request {
    Request::MergeItem {
        item: item.id.clone(),
        ignore_checks: false,
        chat: false,
    }
}

async fn merged(h: &Harness, item: &QueueItem) -> MergeRequest {
    match h.cli(merge_item(item)).await {
        Ok(Reply::Merge(merge)) => *merge,
        other => panic!("a merge request: {other:?}"),
    }
}

fn refusal(merge: &MergeRequest) -> &str {
    match &merge.state {
        MergeState::Refused { reason } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn review(h: &Harness, item: &QueueItem, full: bool) -> Result<LocalReview, ArchonError> {
    let request = Request::ItemDiff {
        item: item.id.clone(),
        full,
    };
    match h.cli(request).await? {
        Reply::LocalReview(review) => Ok(*review),
        other => panic!("a review: {other:?}"),
    }
}

#[tokio::test]
async fn a_clean_fast_forward_lands_the_branch_and_keeps_it() {
    let h = Harness::new().await;
    let (item, branch, head) = delivered_locally(&h).await;
    let before = git(&h.repo, &["rev-parse", "main"]);
    assert_ne!(before, head);

    let shown = review(&h, &item, true).await.expect("review");
    assert_eq!(shown.branch, branch);
    assert_eq!(shown.base, "main");
    assert!(shown.diffstat.contains("change.txt"), "{}", shown.diffstat);
    assert!(shown.blocker.is_none(), "{:?}", shown.blocker);

    let merge = merged(&h, &item).await;
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), head);
    assert_eq!(git(&h.repo, &["rev-parse", &branch]), head, "branch kept");
    assert!(h.repo.join("change.txt").exists(), "the checkout followed");
}

/// A second merge finds the base holding the branch: the operator hears it
/// as news, not as a blocker waiting for them.
#[tokio::test]
async fn merging_a_landed_item_again_is_news_not_a_blocker() {
    use archon_core::notify::{FeedEntry, NotificationKind};
    let h = Harness::new().await;
    let (item, _, _) = delivered_locally(&h).await;
    let first = merged(&h, &item).await;
    assert!(
        matches!(first.state, MergeState::Merged { .. }),
        "{first:?}"
    );

    let again = merged(&h, &item).await;
    assert!(refusal(&again).ends_with("nothing to merge"), "{again:?}");
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    let kinds: Vec<NotificationKind> = feed
        .iter()
        .filter_map(|entry| match &entry.entry {
            FeedEntry::Notification(n) if n.item.as_ref() == Some(&item.id) => Some(n.kind),
            _ => None,
        })
        .collect();
    assert!(!kinds.contains(&NotificationKind::Blocker), "{kinds:?}");
    assert!(
        kinds.ends_with(&[NotificationKind::Outcome, NotificationKind::Outcome]),
        "the merge and the refusal are both outcomes: {kinds:?}"
    );
}

#[tokio::test]
async fn a_moved_base_is_refused_with_the_refresh_command() {
    let h = Harness::new().await;
    let (item, _, _) = delivered_locally(&h).await;
    std::fs::write(h.repo.join("other.txt"), "x\n").unwrap();
    git(&h.repo, &["add", "other.txt"]);
    git(&h.repo, &["commit", "--quiet", "-m", "main moved"]);
    let moved = git(&h.repo, &["rev-parse", "main"]);

    let merge = merged(&h, &item).await;
    let why = refusal(&merge);
    assert!(
        why.contains(&format!("archon queue refresh {}", item.id)),
        "{why}"
    );
    assert_eq!(
        git(&h.repo, &["rev-parse", "main"]),
        moved,
        "main untouched"
    );
}

#[tokio::test]
async fn a_dirty_checked_out_base_is_never_touched() {
    let h = Harness::new().await;
    let (item, _, _) = delivered_locally(&h).await;
    std::fs::write(h.repo.join("README.md"), "edited by the user\n").unwrap();
    let before = git(&h.repo, &["rev-parse", "main"]);

    let merge = merged(&h, &item).await;
    assert!(refusal(&merge).contains("local changes"), "{merge:?}");
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), before);
    let kept = std::fs::read_to_string(h.repo.join("README.md")).unwrap();
    assert_eq!(kept, "edited by the user\n");
}

#[tokio::test]
async fn history_unrelated_to_the_base_is_refused() {
    let h = Harness::new().await;
    let (item, _, _) = delivered_locally(&h).await;
    let tree = git(&h.repo, &["hash-object", "-t", "tree", "/dev/null"]);
    let orphan = git(&h.repo, &["commit-tree", &tree, "-m", "unrelated"]);
    git(&h.repo, &["checkout", "--quiet", "--detach"]);
    git(&h.repo, &["update-ref", "refs/heads/main", &orphan]);

    let merge = merged(&h, &item).await;
    assert!(refusal(&merge).contains("shares no history"), "{merge:?}");
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), orphan);
}

#[tokio::test]
async fn an_agents_request_needs_the_operators_confirmation() {
    let h = Harness::new().await;
    let (item, _, head) = delivered_locally(&h).await;
    let before = git(&h.repo, &["rev-parse", "main"]);
    let request = Request::RequestMerge {
        item: item.id.clone(),
        reason: Some("done".into()),
    };
    let Ok(Reply::Merge(waiting)) = h.cli(request).await else {
        panic!("a waiting request");
    };
    assert_eq!(waiting.state, MergeState::AwaitingOperator);
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), before, "nothing yet");

    let MergeAuthority::Confirmed { question, .. } = waiting.authority.clone() else {
        panic!("confirmed by a question: {waiting:?}");
    };
    let answer = Request::Answer {
        chat: false,
        question,
        text: "yes".into(),
    };
    h.cli(answer).await.expect("the operator confirms");
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), head);
}

#[tokio::test]
async fn an_ignored_local_file_the_branch_would_replace_is_never_overwritten() {
    let h = Harness::new().await;
    let (item, _, _) = delivered_locally(&h).await;
    let exclude = h.repo.join(".git/info/exclude");
    std::fs::write(&exclude, "change.txt\n").unwrap();
    std::fs::write(h.repo.join("change.txt"), "USER DATA\n").unwrap();
    let before = git(&h.repo, &["rev-parse", "main"]);

    let merge = merged(&h, &item).await;
    assert!(
        matches!(merge.state, MergeState::Refused { .. }),
        "{merge:?}"
    );
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), before);
    let kept = std::fs::read_to_string(h.repo.join("change.txt")).unwrap();
    assert_eq!(kept, "USER DATA\n");
}

#[tokio::test]
async fn the_users_merge_options_cannot_turn_the_fast_forward_into_a_squash() {
    let h = Harness::new().await;
    let (item, _, head) = delivered_locally(&h).await;
    git(&h.repo, &["config", "branch.main.mergeOptions", "--squash"]);

    let merge = merged(&h, &item).await;
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), head);
    assert_eq!(git(&h.repo, &["status", "--porcelain"]), "", "index clean");
}

/// A parent delivered and merged, and a child stacked on it and delivered.
async fn stacked_pair(h: &Harness) -> (QueueItem, QueueItem) {
    set_local(h).await;
    let (parent, _, _) = deliver_local(h, "the parent", Vec::new()).await;
    let (child, _, _) = deliver_local(h, "the child", vec![parent.id.clone()]).await;
    assert!(
        !h.ctx.store.lock().await.stack_parents(&child.id).unwrap()[0].merged,
        "the child was stacked on an unmerged parent"
    );
    (parent, child)
}

#[tokio::test]
async fn a_child_is_refused_with_the_parent_to_merge_first_until_it_merges() {
    let h = Harness::new().await;
    let (parent, child) = stacked_pair(&h).await;
    let why = review(&h, &child, false)
        .await
        .expect_err("parent unmerged");
    assert!(
        why.to_string()
            .contains(&format!("run `archon merge {}` first", parent.id)),
        "{why}"
    );
}

#[tokio::test]
async fn a_child_merges_and_diffs_after_its_parent_was_fast_forwarded() {
    let h = Harness::new().await;
    let (parent, child) = stacked_pair(&h).await;
    let landed = merged(&h, &parent).await;
    assert!(
        matches!(landed.state, MergeState::Merged { .. }),
        "{landed:?}"
    );

    let shown = review(&h, &child, true).await.expect("child review");
    assert_eq!(shown.base, "main");
    assert!(shown.blocker.is_none(), "{:?}", shown.blocker);
    let merge = merged(&h, &child).await;
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    let parents = h.ctx.store.lock().await.stack_parents(&child.id).unwrap();
    assert!(parents.iter().all(|p| p.merged), "recorded as merged");
}

#[tokio::test]
async fn a_parent_fast_forwarded_by_hand_counts_as_merged_from_git() {
    let h = Harness::new().await;
    let (parent, child) = stacked_pair(&h).await;
    let branch = h.run(&parent.id).await.branch;
    git(&h.repo, &["merge", "--ff-only", &branch]);

    let shown = review(&h, &child, true).await.expect("child review");
    assert_eq!(shown.base, "main");
    let merge = merged(&h, &child).await;
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
}

#[tokio::test]
async fn a_child_of_a_squash_merged_parent_is_told_to_refresh_not_to_merge_the_parent() {
    let h = Harness::new().await;
    let (parent, child) = stacked_pair(&h).await;
    let run = h.run(&parent.id).await;
    git(&h.repo, &["merge", "--squash", &run.branch]);
    git(&h.repo, &["commit", "--quiet", "-m", "squashed parent"]);
    let now = archon_core::now();
    let squashed = MergeRequest {
        id: archon_core::id::MergeRequestId::new(),
        item: parent.id.clone(),
        run: run.id.clone(),
        repo: h
            .ctx
            .store
            .lock()
            .await
            .item(&parent.id)
            .unwrap()
            .repo
            .unwrap(),
        pr_url: format!("local:{}", run.branch),
        head: git(&h.repo, &["rev-parse", &run.branch]),
        authority: MergeAuthority::Cli,
        ignore_checks: false,
        reason: None,
        waived_by: None,
        waited: false,
        mergeability_wait_since: None,
        checks: None,
        state: MergeState::Merged {
            method: "squash".into(),
            actor: None,
            observed: true,
        },
        requested_at: now,
        updated_at: now,
    };
    h.ctx.store.lock().await.insert_merge(&squashed).unwrap();

    let merge = merged(&h, &child).await;
    let why = refusal(&merge);
    assert!(!why.contains("not merged yet"), "{why}");
    assert!(
        why.contains(&format!("archon queue refresh {}", child.id)),
        "{why}"
    );
}

#[tokio::test]
async fn a_merge_of_another_branch_of_the_parent_does_not_settle_the_child() {
    let h = Harness::new().await;
    let (parent, child) = stacked_pair(&h).await;
    let run = h.run(&parent.id).await;
    let now = archon_core::now();
    let earlier = MergeRequest {
        id: archon_core::id::MergeRequestId::new(),
        item: parent.id.clone(),
        run: run.id.clone(),
        repo: h
            .ctx
            .store
            .lock()
            .await
            .item(&parent.id)
            .unwrap()
            .repo
            .unwrap(),
        pr_url: format!("local:{}", run.branch),
        head: git(&h.repo, &["rev-parse", &run.branch]),
        authority: MergeAuthority::Cli,
        ignore_checks: false,
        reason: None,
        waived_by: None,
        waited: false,
        mergeability_wait_since: None,
        checks: None,
        state: MergeState::Merged {
            method: "squash".into(),
            actor: None,
            observed: true,
        },
        requested_at: now,
        updated_at: now,
    };
    {
        let mut store = h.ctx.store.lock().await;
        store.insert_merge(&earlier).unwrap();
        let mut stacked = store.stack_parents(&child.id).unwrap().remove(0);
        stacked.parent_branch = "archon/a-fresh-retry-of-the-parent".into();
        store.save_stack_parent(&stacked).unwrap();
    }

    let why = review(&h, &child, false)
        .await
        .expect_err("parent unmerged");
    assert!(why.to_string().contains("not merged yet"), "{why}");
}
