//! Independent runs proceed in parallel: one item's slow Aether call does
//! not hold back the start of another, and deliveries into different
//! repositories run at once (ARCHITECTURE §7.3).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoAdd, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::StageOutcome;
use archon_daemon::{dispatcher, executor};
use common::delivery::{FakeGh, add_origin, origin_of, pushed, shipped_by};
use common::{Harness, REPO_NAME, git, init_repo};
use serde_json::json;

/// Whether an agent was asked for with `text` in its objective.
fn requested(h: &Harness, text: &str) -> bool {
    let requests = h.backend.requests();
    requests.iter().any(|r| r.objective.contains(text))
}

#[tokio::test]
async fn a_stalled_spawn_holds_back_no_other_run() {
    let h = Harness::new().await;
    let slow = h.enqueue("slow item").await;
    let fast = h.enqueue("fast item").await;
    let gate = h.backend.stall_spawns("brief for slow item");
    executor::coordination::reconcile(&h.ctx)
        .await
        .expect("coordination pass");
    let ctx = h.ctx.clone();
    let dispatch = tokio::spawn(async move { dispatcher::dispatch_once(&ctx).await });

    let fast_spawned = async {
        while !requested(&h, "brief for fast item") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), fast_spawned)
        .await
        .expect("the fast item's agent is asked for while the slow one's waits");
    assert!(
        !requested(&h, "brief for slow item"),
        "the slow spawn is still held"
    );

    gate.add_permits(1);
    dispatch
        .await
        .expect("dispatch task")
        .expect("dispatch pass");
    assert!(requested(&h, "brief for slow item"));
    for item in [&slow, &fast] {
        let state = h.state(&item.id).await;
        assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
    }
}

/// A second registered repository, `other`, with an `origin` of its own;
/// returns that bare `origin`.
async fn other_repository(h: &Harness) -> PathBuf {
    let root = h.dir.path().canonicalize().expect("root");
    let repo = init_repo(&root.join("other"));
    let origin = root.join("other-origin.git");
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(
        &origin,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().expect("utf-8")],
    );
    git(&repo, &["push", "--quiet", "origin", "main"]);
    let add = RepoAdd {
        path: repo,
        name: Some("other".into()),
        ship: Some(ShipMode::Pr),
        base_branch: Some("main".into()),
        default_pipeline: None,
        auto_merge: false,
    };
    let added = h.cli(Request::RepoAdd(add)).await.expect("repo add");
    assert!(matches!(added, Reply::Repo(_)), "{added:?}");
    origin
}

async fn quick_pr_in(h: &Harness, repo: &str) -> QueueItem {
    let request = EnqueueRequest {
        repo: Some(RepoRef(repo.into())),
        pipeline: Some("direct".into()),
        title: format!("a change in {repo}"),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// `item` shipped: `origin` holds the commit its ship record named.
async fn assert_shipped(h: &Harness, item: &QueueItem, origin: &Path, commit: &str) {
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let run = h.run(&item.id).await;
    assert!(run.shipped, "the run records its delivery");
    assert_eq!(pushed(origin, &run.branch).as_deref(), Some(commit));
}

/// The commit a stage's ship record names.
fn recorded_commit(outcome: &StageOutcome) -> String {
    let StageOutcome::Completed {
        structured: Some(record),
        ..
    } = outcome
    else {
        panic!("a ship record: {outcome:?}");
    };
    record["commit"].as_str().expect("a commit").to_owned()
}

/// Two repositories' deliveries run at once through archond's shared git
/// directory, made on first use; a settle that arrives twice ships once.
#[tokio::test]
async fn two_repositories_deliver_at_once_and_each_run_ships_once() {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    let other_origin = other_repository(&h).await;
    let first = quick_pr_in(&h, REPO_NAME).await;
    let second = quick_pr_in(&h, "other").await;
    h.dispatch().await;
    let mut settles = Vec::new();
    for item in [&first, &second] {
        let stage = h.stages(&item.id).await.remove(0);
        let outcome = shipped_by(&h, &stage, "Changed it; tests pass", json!({})).await;
        settles.push((stage.aether_agent.clone().expect("an agent"), outcome));
    }
    let [(a, done_a), (b, done_b)] = <[_; 2]>::try_from(settles).expect("two settles");
    let commits = [recorded_commit(&done_a), recorded_commit(&done_b)];
    let (one, two, again) = tokio::join!(
        executor::stage_settled(&h.ctx, &a, done_a.clone()),
        executor::stage_settled(&h.ctx, &b, done_b),
        executor::stage_settled(&h.ctx, &a, done_a),
    );
    for settled in [one, two, again] {
        settled.expect("a settle, repeated or not, succeeds");
    }
    h.dispatch().await;

    assert_shipped(&h, &first, &origin_of(&h), &commits[0]).await;
    assert_shipped(&h, &second, &other_origin, &commits[1]).await;
    assert_eq!(gh.count("pr create"), 2, "one pull request per run");
}
