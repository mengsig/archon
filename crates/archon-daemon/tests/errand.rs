//! The built-in errand dispatches into scratch, with an optional repository,
//! and completes without needing an HTML report or a shipping stage.

mod common;

use std::collections::BTreeMap;

use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageState};
use common::{Harness, MODEL, REPO_NAME, choice, git};

async fn text_errand(with_repo: bool) {
    let h = Harness::new().await;
    std::fs::write(h.repo.join("README.md"), "operator's uncommitted edit\n")
        .expect("edit checkout");
    let before_status = git(&h.repo, &["status", "--porcelain"]);
    let before_worktrees = git(&h.repo, &["worktree", "list", "--porcelain"]);
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: with_repo.then(|| RepoRef(REPO_NAME.into())),
        pipeline: Some("errand".into()),
        title: "summarize the options".into(),
        brief: "Give a short summary in text.".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan(BTreeMap::from([(
            "errand".into(),
            vec![choice(MODEL, Effort::Medium)],
        )]))
        .into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.expect("enqueue") else {
        panic!("enqueue replies with the item");
    };
    h.dispatch().await;
    let scratch = h.ctx.paths.scratch.join(item.id.as_str());
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(requests[0].workspace, scratch);
    assert_eq!(
        requests[0]
            .objective
            .contains(h.repo.to_str().expect("UTF-8 path")),
        with_repo,
        "an attached checkout is available for research"
    );
    assert_eq!(h.run(&item.id).await.workspace, scratch);
    assert!(scratch.is_dir());

    let summary = "Option A fits the request; option B needs more setup.";
    h.settle_running(|stage| {
        assert_eq!(stage.stage_id, "errand");
        StageOutcome::Completed {
            summary: summary.into(),
            structured: Some(serde_json::json!({"reports": [], "sources": []})),
        }
    })
    .await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let stages = h.stages(&item.id).await;
    assert_eq!(stages.len(), 1, "no ship stage");
    assert_eq!(stages[0].state, StageState::Completed);
    assert_eq!(stages[0].summary.as_deref(), Some(summary));
    assert_eq!(
        stages[0].structured.as_ref().unwrap()["reports"],
        serde_json::json!([])
    );
    assert!(scratch.is_dir(), "scratch remains for gc");
    assert!(
        !scratch.join(".archon/reports").exists(),
        "no page is required"
    );
    assert_eq!(git(&h.repo, &["status", "--porcelain"]), before_status);
    assert_eq!(
        git(&h.repo, &["worktree", "list", "--porcelain"]),
        before_worktrees
    );
    assert_eq!(
        std::fs::read_to_string(h.repo.join("README.md")).expect("read checkout"),
        "operator's uncommitted edit\n"
    );
}

#[tokio::test]
async fn errand_without_a_repository_completes_with_empty_reports() {
    text_errand(false).await;
}

#[tokio::test]
async fn errand_with_a_repository_uses_scratch_and_preserves_the_checkout() {
    text_errand(true).await;
}
