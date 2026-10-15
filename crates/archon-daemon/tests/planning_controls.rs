//! Stage effort defaults, the reason an effort above them needs, and per-stage timeouts.
mod common;

use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelChoiceDraft, ModelPlanDraft};
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use archon_core::queue::QueueItem;
use archon_core::steer::Steer;
use common::{Harness, MODEL, PIPELINE, REPO_NAME};

fn draft(stage: &str, effort: Option<Effort>) -> ModelPlanDraft {
    let choice = ModelChoiceDraft {
        model: MODEL.parse().unwrap(),
        effort,
    };
    ModelPlanDraft([(stage.to_owned(), vec![choice])].into())
}

fn reason(stage: &str) -> Option<archon_core::model::ModelReasons> {
    Some([(stage.to_owned(), "hard to verify by tests".to_owned())].into())
}

fn enqueue(plan: ModelPlanDraft, reasons: Option<archon_core::model::ModelReasons>) -> Request {
    Request::Enqueue(EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: "Plan".into(),
        brief: "Apply the supplied fix.".into(),
        priority: Default::default(),
        depends_on: vec![],
        tags: vec![],
        model_plan: plan,
        model_reasons: reasons,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    })
}

async fn item(h: &Harness, request: Request) -> QueueItem {
    match h.cli(request).await.expect("accepted") {
        Reply::Item(item) => item,
        other => panic!("replied {other:?}"),
    }
}

async fn refusal(h: &Harness, request: Request) -> String {
    match h.cli(request).await.expect_err("refused") {
        ArchonError::Invalid(message) => message,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

async fn default_of(h: &Harness, queued: &QueueItem, stage: &str) -> Effort {
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: queued.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail");
    };
    detail.default_efforts[stage]
}

#[tokio::test]
async fn an_omitted_effort_is_the_stage_default_not_high() {
    let h = Harness::new().await;
    let queued = item(&h, enqueue(draft("implement", None), None)).await;
    let default = default_of(&h, &queued, "implement").await;
    assert_eq!(queued.model_plan.0["implement"][0].effort, default);
    assert_ne!(default, Effort::High);
}

#[tokio::test]
async fn an_effort_above_the_default_needs_a_reason_for_that_stage() {
    let h = Harness::new().await;
    let queued = item(&h, enqueue(draft("implement", None), None)).await;
    let default = default_of(&h, &queued, "implement").await;
    assert_eq!(default, Effort::Medium, "the test pipeline's default");

    let high = || draft("implement", Some(Effort::High));
    let error = refusal(&h, enqueue(high(), None)).await;
    assert!(
        error.contains("implement") && error.contains("medium"),
        "{error}"
    );
    let elsewhere = refusal(&h, enqueue(high(), reason("review"))).await;
    assert!(elsewhere.contains("implement"), "{elsewhere}");

    let patch = |reasons| Request::UpdateItem {
        item: queued.id.clone(),
        patch: ItemPatch {
            model_plan: Some(high()),
            model_reasons: reasons,
            ..Default::default()
        },
    };
    assert!(refusal(&h, patch(None)).await.contains("implement"));
    h.cli(patch(reason("implement")))
        .await
        .expect("with reason");

    let steer = |effort| Request::SteerItem {
        item: queued.id.clone(),
        steer: Steer {
            stage: Some("implement".into()),
            effort: Some(effort),
            ..Steer::default()
        },
    };
    h.cli(steer(Effort::Low)).await.expect("lower needs none");

    let accepted = item(&h, enqueue(high(), reason("implement"))).await;
    assert_eq!(accepted.model_plan.0["implement"][0].effort, Effort::High);
}

#[tokio::test]
async fn a_lower_effort_needs_no_reason() {
    let h = Harness::new().await;
    let low = item(&h, enqueue(draft("implement", Some(Effort::Low)), None)).await;
    assert_eq!(low.model_plan.0["implement"][0].effort, Effort::Low);
}

#[tokio::test]
async fn a_steer_above_the_default_is_refused_without_a_reason() {
    let h = Harness::new().await;
    let queued = item(&h, enqueue(draft("implement", None), None)).await;
    let error = refusal(
        &h,
        Request::SteerItem {
            item: queued.id.clone(),
            steer: Steer {
                stage: Some("implement".into()),
                effort: Some(Effort::XHigh),
                ..Steer::default()
            },
        },
    )
    .await;
    assert!(error.contains("implement"), "{error}");
}

fn with_timeouts(minutes: &[(&str, u32)]) -> Request {
    let Request::Enqueue(mut request) = enqueue(ModelPlanDraft::default(), None) else {
        unreachable!()
    };
    request.stage_timeouts = minutes.iter().map(|&(s, m)| (s.to_owned(), m)).collect();
    Request::Enqueue(request)
}

#[tokio::test]
async fn stage_timeouts_stay_within_the_configured_maximum() {
    let h = Harness::new().await;
    let queued = item(&h, with_timeouts(&[("implement", 120)])).await;
    assert_eq!(queued.stage_timeouts["implement"], 120);
    assert_eq!(h.item(&queued.id).await.stage_timeouts["implement"], 120);

    let most = archon_core::config::Config::default()
        .limits
        .max_stage_timeout_minutes;
    for minutes in [0, most + 1] {
        let error = refusal(&h, with_timeouts(&[("implement", minutes)])).await;
        assert!(
            error.contains("implement") && error.contains(&most.to_string()),
            "{error}"
        );
    }
    let unknown = refusal(&h, with_timeouts(&[("nowhere", 30)])).await;
    assert!(unknown.contains("nowhere"), "{unknown}");

    let update = |minutes| Request::UpdateItem {
        item: queued.id.clone(),
        patch: ItemPatch {
            stage_timeouts: [("implement".to_owned(), minutes)].into(),
            ..Default::default()
        },
    };
    h.cli(update(90)).await.expect("within the maximum");
    assert_eq!(h.item(&queued.id).await.stage_timeouts["implement"], 90);
    assert!(refusal(&h, update(most + 1)).await.contains("implement"));
}

#[tokio::test]
async fn a_steer_merges_bounded_timeouts_for_future_spawns() {
    let h = Harness::new().await;
    let queued = item(&h, with_timeouts(&[("implement", 120)])).await;
    let steer = |minutes| Request::SteerItem {
        item: queued.id.clone(),
        steer: Steer {
            stage_timeouts: [("implement".to_owned(), minutes)].into(),
            ..Steer::default()
        },
    };
    h.cli(steer(180)).await.expect("timeout steer accepted");
    assert_eq!(h.item(&queued.id).await.stage_timeouts["implement"], 180);
    for minutes in [0, 1441] {
        assert!(refusal(&h, steer(minutes)).await.contains("implement"));
    }
    assert_eq!(h.item(&queued.id).await.stage_timeouts["implement"], 180);
}

#[tokio::test]
async fn an_override_is_the_spawned_agents_deadline() {
    let h = Harness::new().await;
    item(&h, with_timeouts(&[("implement", 180)])).await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    assert_eq!(
        h.backend.requests()[0].deadline,
        Some(stage.started_at + chrono::Duration::minutes(180))
    );
}

#[tokio::test]
async fn retrying_with_above_default_effort_requires_a_reason() {
    let h = Harness::new().await;
    let queued = item(&h, enqueue(draft("implement", None), None)).await;
    h.cli(Request::CancelItem {
        item: queued.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    let retry = |reasons| Request::RetryItem {
        item: queued.id.clone(),
        model_plan: Some(draft("implement", Some(Effort::High))),
        model_reasons: reasons,
        fresh: false,
        brief_append: None,
    };
    assert!(refusal(&h, retry(None)).await.contains("implement"));
    h.cli(retry(reason("implement")))
        .await
        .expect("reasoned retry");
}

const REVIEWER: &str = "claude-code/claude-haiku-5-5";

fn reviewer_enqueue() -> Request {
    let Request::Enqueue(mut request) = enqueue(draft("implement", None), None) else {
        unreachable!()
    };
    request.pipeline = Some("direct".into());
    request.autonomy.review = Some(archon_core::autonomy::ReviewLevel::Standard);
    request.model_plan.0.insert(
        "implement.review".into(),
        vec![ModelChoiceDraft {
            model: REVIEWER.parse().unwrap(),
            effort: Some(Effort::Low),
        }],
    );
    Request::Enqueue(request)
}

#[tokio::test]
async fn reviewer_keys_pass_through_enqueue_and_reach_the_stage_contract() {
    let h = Harness::new().await;
    let queued = item(&h, reviewer_enqueue()).await;
    assert_eq!(
        queued.model_plan.0["implement.review"][0].model.to_string(),
        REVIEWER
    );
    h.dispatch().await;
    let request = &h.backend.requests()[0];
    assert!(
        request.instructions.contains(REVIEWER),
        "{}",
        request.instructions
    );
}

#[tokio::test]
async fn enqueue_refuses_a_reviewer_outside_the_model_policy() {
    let mut config = archon_core::config::Config::default();
    config.models.deny.push(REVIEWER.parse().unwrap());
    let h = Harness::with_config(config).await;
    let error = refusal(&h, reviewer_enqueue()).await;
    assert!(error.contains(REVIEWER), "{error}");
    assert!(h.running().await.is_empty());
}

#[tokio::test]
async fn a_running_runs_timeout_steer_applies_to_the_next_spawn() {
    let h = Harness::new().await;
    let queued = item(&h, with_timeouts(&[("implement", 120)])).await;
    h.dispatch().await;
    let deadline = h.backend.requests()[0].deadline;
    h.cli(Request::SteerItem {
        item: queued.id.clone(),
        steer: Steer {
            stage_timeouts: [("implement".into(), 180), ("review".into(), 75)].into(),
            ..Default::default()
        },
    })
    .await
    .expect("running timeout steer");
    assert_eq!(h.item(&queued.id).await.stage_timeouts["implement"], 180);
    assert!(
        h.backend.updates().is_empty(),
        "the current deadline stays put"
    );
    assert_eq!(h.backend.requests()[0].deadline, deadline);
    h.settle_running(|_| common::completed("implemented")).await;
    h.dispatch().await;
    let review = h.running().await.remove(0);
    let request = h
        .backend
        .requests()
        .into_iter()
        .find(|r| r.name.contains("review"))
        .expect("review spawn");
    assert_eq!(
        request.deadline,
        Some(review.started_at + chrono::Duration::minutes(75))
    );
}
