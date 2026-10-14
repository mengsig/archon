//! Which finished items wake the orchestrator (ARCHITECTURE §5.4): the
//! operator gets every outcome in the feed from the daemon, and the
//! orchestrator is woken only when it has something to do about one.

mod common;

use archon_core::config::Config;
use archon_core::github::GithubThread;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{NOTIFY_TAG, QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use archon_daemon::triggers::{self, TriggerEvent};
use common::{Harness, ORCHESTRATOR, PIPELINE, REPO_NAME, completed, plan, verdict};

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

fn request(title: &str) -> EnqueueRequest {
    EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    }
}

async fn enqueued(h: &Harness, request: EnqueueRequest) -> QueueItem {
    match h.orchestrator(Request::Enqueue(request)).await {
        Ok(Reply::Item(item)) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// Run the item's pipeline to the end with every stage passing.
async fn succeed(h: &Harness, item: &QueueItem) {
    h.dispatch().await;
    for _ in ["implement", "review", "ship"] {
        h.settle_running(passing).await;
        h.dispatch().await;
    }
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
}

/// The item's `item_finished` news as held for the orchestrator.
async fn finished_news(h: &Harness, item: &QueueItem) -> TriggerEvent {
    let held = triggers::held(&h.ctx).await.expect("held news");
    let found = held.into_iter().find(
        |event| matches!(event, TriggerEvent::ItemFinished { item: id, .. } if *id == item.id),
    );
    found.expect("a finished item is always news, waking or not")
}

#[tokio::test]
async fn a_routine_success_reaches_the_feed_and_does_not_wake_the_orchestrator() {
    let h = Harness::new().await;
    let item = enqueued(&h, request("bump a dependency")).await;
    succeed(&h, &item).await;

    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert_eq!(
        outcomes.len(),
        1,
        "the daemon tells the operator itself: {outcomes:?}"
    );
    assert!(
        outcomes[0].contains("opened a pull request"),
        "{}",
        outcomes[0]
    );
    let TriggerEvent::ItemFinished { state, pr_url, .. } = finished_news(&h, &item).await else {
        unreachable!("finished_news returns item_finished");
    };
    assert_eq!(state, "succeeded");
    assert_eq!(
        pr_url.as_deref(),
        Some("https://github.com/o/r/pull/1"),
        "the pull request the daemon found, not the agent's claim"
    );
    assert!(
        !triggers::due(&h.ctx).await.unwrap(),
        "a routine success waits for the orchestrator's next turn"
    );
}

#[tokio::test]
async fn a_failure_wakes_the_orchestrator() {
    let h = Harness::new().await;
    let item = enqueued(&h, request("fix the login bug")).await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "the build does not compile".into(),
        infra: false,
    })
    .await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_eq!(h.notices(NotificationKind::Outcome).await.len(), 1);
    finished_news(&h, &item).await;
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "a failure is diagnosed now"
    );
}

#[tokio::test]
async fn an_outcome_the_operator_must_act_on_wakes_the_orchestrator() {
    let h = Harness::new().await;
    let item = enqueued(&h, request("needs a credential")).await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "BLOCKED: the provider quota is exhausted. Raise the quota.".into(),
        infra: false,
    })
    .await;
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
    finished_news(&h, &item).await;
    assert!(triggers::due(&h.ctx).await.unwrap());
}

#[tokio::test]
async fn an_item_tagged_notify_wakes_the_orchestrator_when_it_succeeds() {
    let h = Harness::new().await;
    let mut asked = request("the first of two steps");
    asked.tags = vec!["release".into(), NOTIFY_TAG.into()];
    let item = enqueued(&h, asked).await;
    succeed(&h, &item).await;
    finished_news(&h, &item).await;
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "whoever queued it asked to be told"
    );
}

#[tokio::test]
async fn an_item_asked_for_on_github_wakes_the_orchestrator_to_reply_there() {
    let h = Harness::new().await;
    let thread: GithubThread =
        serde_json::from_value(serde_json::json!({"repository": "Owner/Repo", "number": 12}))
            .unwrap();
    let mut asked = request("fix the flaky test");
    asked.origin = Some(thread.clone());
    let item = enqueued(&h, asked).await;
    succeed(&h, &item).await;
    let TriggerEvent::ItemFinished { origin, .. } = finished_news(&h, &item).await else {
        unreachable!("finished_news returns item_finished");
    };
    assert_eq!(origin, Some(thread), "the reply has its thread");
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "the thread is owed a reply"
    );
}

#[tokio::test]
async fn a_cancelled_item_is_quiet_news() {
    let h = Harness::new().await;
    let item = enqueued(&h, request("no longer wanted")).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("superseded".into()),
    };
    h.cli(cancel).await.expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    finished_news(&h, &item).await;
    assert!(
        !triggers::due(&h.ctx).await.unwrap(),
        "whoever cancelled it knows"
    );
}

#[tokio::test]
async fn wake_on_success_restores_a_trigger_per_outcome() {
    let mut config = Config::default();
    config.orchestrator.wake_on_success = true;
    let h = Harness::with_config(config).await;
    assert_eq!(
        h.kv(archon_queue::store::keys::ORCHESTRATOR_AGENT).await,
        None
    );
    let item = enqueued(&h, request("bump a dependency")).await;
    assert_eq!(
        h.kv(archon_queue::store::keys::ORCHESTRATOR_AGENT)
            .await
            .as_deref(),
        Some(ORCHESTRATOR)
    );
    succeed(&h, &item).await;
    assert!(triggers::due(&h.ctx).await.unwrap());
}
