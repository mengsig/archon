//! Reasons are item metadata, validated before mutation and replaced with plans.
mod common;

use archon_core::error::ArchonError;
use archon_core::model::ModelReasons;
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use common::{Harness, PIPELINE, REPO_NAME, plan};

fn reasons(stage: &str, reason: &str) -> Option<ModelReasons> {
    Some([(stage.into(), reason.into())].into())
}

fn enqueue(model_reasons: Option<ModelReasons>) -> Request {
    Request::Enqueue(EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: "Record choices".into(),
        brief: "Apply the supplied fix.".into(),
        priority: Default::default(),
        depends_on: vec![],
        tags: vec![],
        model_plan: plan().into(),
        model_reasons,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    })
}

#[tokio::test]
async fn reasons_persist_get_item_and_clear_only_when_the_plan_is_replaced() {
    let h = Harness::new().await;
    let expected = reasons("implement", "Apply the supplied fix");
    let Reply::Item(item) = h.cli(enqueue(expected.clone())).await.unwrap() else {
        panic!("item");
    };
    assert_eq!(h.item(&item.id).await.model_reasons, expected);
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail");
    };
    assert_eq!(detail.item.model_reasons, expected);
    let update = |patch| Request::UpdateItem {
        item: item.id.clone(),
        patch,
    };
    h.cli(update(ItemPatch {
        priority: Some(archon_core::queue::Priority::Low),
        ..Default::default()
    }))
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, expected);
    let replacement = reasons("review", "Independent check of the supplied fix");
    h.cli(update(ItemPatch {
        model_plan: Some(plan().into()),
        model_reasons: replacement.clone(),
        ..Default::default()
    }))
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, replacement);
    h.cli(update(ItemPatch {
        model_plan: Some(plan().into()),
        ..Default::default()
    }))
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, None);
    h.cli(update(ItemPatch {
        model_reasons: expected.clone(),
        ..Default::default()
    }))
    .await
    .unwrap();
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
    })
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, expected);
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: Some(plan().into()),
        model_reasons: replacement.clone(),
        fresh: false,
        brief_append: None,
    })
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, replacement);
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: Some(plan().into()),
        model_reasons: None,
        fresh: false,
        brief_append: None,
    })
    .await
    .unwrap();
    assert_eq!(h.item(&item.id).await.model_reasons, None);
}

#[tokio::test]
async fn invalid_reasons_write_nothing_through_all_three_requests() {
    let h = Harness::new().await;
    let item = h.enqueue("unchanged").await;
    for bad in [
        reasons("missing", "unknown stage"),
        reasons("implement", &"é".repeat(301)),
    ] {
        let requests = [
            enqueue(bad.clone()),
            Request::UpdateItem {
                item: item.id.clone(),
                patch: ItemPatch {
                    model_reasons: bad.clone(),
                    ..Default::default()
                },
            },
            Request::RetryItem {
                item: item.id.clone(),
                model_plan: None,
                model_reasons: bad.clone(),
                fresh: false,
                brief_append: None,
            },
        ];
        for request in requests {
            let error = h.cli(request).await.unwrap_err();
            assert!(matches!(error, ArchonError::Invalid(_)), "{error}");
            if bad.as_ref().unwrap().contains_key("missing") {
                for id in ["implement", "review", "ship"] {
                    assert!(error.to_string().contains(id), "{error}");
                }
            } else {
                assert!(error.to_string().contains("300"), "{error}");
            }
            assert_eq!(h.item(&item.id).await, item);
        }
    }
}

#[tokio::test]
async fn omitted_reasons_are_absent_on_the_wire_and_old_items_decode() {
    let h = Harness::new().await;
    let item = h.enqueue("legacy").await;
    assert_eq!(item.model_reasons, None);
    let wire = serde_json::to_value(&item).unwrap();
    assert!(wire.get("model_reasons").is_none());
    assert_eq!(
        serde_json::from_value::<archon_core::queue::QueueItem>(wire).unwrap(),
        item
    );
}
