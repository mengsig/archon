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
    let local = RepoPatch {
        ship: Some(ShipMode::Local),
        ..RepoPatch::default()
    };
    let update = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: local,
    };
    h.cli(update).await.expect("ship = local");
    let request = EnqueueRequest {
        onto: None,
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: "a local change".into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
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
