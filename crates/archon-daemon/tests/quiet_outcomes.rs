//! Which finished items wake the orchestrator (ARCHITECTURE §5.4): the
//! operator gets every outcome in the feed from the daemon. A success wakes
//! the orchestrator unless `wake_on_success` is off; then only work for it does.

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

/// An instance that wakes the orchestrator only when it has work to do.
fn quiet() -> Config {
    let mut config = Config::default();
    config.orchestrator.wake_on_success = false;
    config
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
    let h = Harness::with_config(quiet()).await;
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
    let news = finished_news(&h, &item).await;
    let TriggerEvent::ItemFinished {
        state,
        pr_url,
        lesson_hint,
        ..
    } = news
    else {
        unreachable!("finished_news returns item_finished");
    };
    assert_eq!(state, "succeeded");
    assert_eq!(lesson_hint, None, "an uncorrected success teaches nothing");
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
    let h = Harness::with_config(quiet()).await;
    let item = enqueued(&h, request("fix the login bug")).await;
    h.dispatch().await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "the build does not compile".into(),
        infra: false,
    })
    .await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert_eq!(h.notices(NotificationKind::Outcome).await.len(), 1);
    let news = finished_news(&h, &item).await;
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "a failure is diagnosed now"
    );
    let TriggerEvent::ItemFinished { lesson_hint, .. } = news else {
        unreachable!("finished_news returns item_finished");
    };
    let hint = lesson_hint.expect("the turn it wakes is asked what the failure teaches");
    assert!(hint.contains("record_lesson"), "{hint}");
    let learned_from = format!("Run of {}: failed; failure: other", item.id);
    assert!(hint.contains(&learned_from), "{hint}");
}

#[tokio::test]
async fn an_outcome_the_operator_must_act_on_wakes_the_orchestrator() {
    let h = Harness::with_config(quiet()).await;
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
    let h = Harness::with_config(quiet()).await;
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
    let h = Harness::with_config(quiet()).await;
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
async fn by_default_a_success_wakes_the_orchestrator_so_the_chat_shows_it() {
    let h = Harness::new().await;
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
    finished_news(&h, &item).await;
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "Aether's chat shows only what the orchestrator says"
    );
}

/// The cost the orchestrator is told for `item` once its provider settled
/// $4.25 after the run finished: `pick` reads it from the batch Archon keys.
async fn told_after_settlement(
    h: &Harness,
    item: &QueueItem,
    pick: impl Fn(&TriggerEvent) -> Option<f64>,
) -> f64 {
    let stage = h.stages(&item.id).await.remove(0);
    h.ctx
        .store
        .lock()
        .await
        .record_spend(&archon_queue::store::SpendRow {
            agent: stage.aether_agent.unwrap(),
            item: Some(item.id.clone()),
            cost_usd: 4.25,
            cost_basis: archon_core::cost::CostBasis::ProviderReported,
            cost_complete: Some(true),
            estimated_cost_usd: 0.0,
            input_tokens: 1,
            output_tokens: 1,
            at: archon_core::now(),
        })
        .unwrap();
    // No Aether is connected, so the flush keys a batch that stays in flight to be read.
    let ctx = h.ctx.clone();
    let flush = tokio::spawn(async move { triggers::flush_quiet(&ctx).await });
    let delivery = tokio::spawn(triggers::deliver_loop(h.ctx.clone()));
    let told = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if !batch_keyed(h).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
            let held = triggers::held(&h.ctx).await.unwrap();
            let costs: Vec<f64> = held.iter().filter_map(&pick).collect();
            assert_eq!(costs.len(), 1, "one outcome of the item is told: {held:?}");
            break costs[0];
        }
    })
    .await
    .expect("the batch is keyed");
    delivery.abort();
    flush.abort();
    told
}

/// Whether held news was keyed into a batch for the orchestrator.
async fn batch_keyed(h: &Harness) -> bool {
    let raw = h.kv("trigger_outbox").await.unwrap_or_default();
    let outbox: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    !outbox["in_flight"].is_null()
}

#[tokio::test]
async fn quiet_completion_news_uses_the_later_settled_cost() {
    let h = Harness::with_config(quiet()).await;
    let item = enqueued(&h, request("late cost")).await;
    succeed(&h, &item).await;
    let told = told_after_settlement(&h, &item, |event| match event {
        TriggerEvent::ItemsFinished { items, .. } => {
            items.iter().find(|i| i.item == item.id).map(|i| i.cost_usd)
        }
        _ => None,
    })
    .await;
    assert_eq!(told, 4.25);
}

/// A success wakes the orchestrator by default; what it is told it cost is
/// the settled figure, as in the quiet list.
#[tokio::test]
async fn a_waking_success_tells_the_later_settled_cost() {
    let h = Harness::new().await;
    let item = enqueued(&h, request("late cost, woken")).await;
    succeed(&h, &item).await;
    let told = told_after_settlement(&h, &item, |event| match event {
        TriggerEvent::ItemFinished {
            item: id, cost_usd, ..
        } if *id == item.id => Some(*cost_usd),
        _ => None,
    })
    .await;
    assert_eq!(told, 4.25);
}
