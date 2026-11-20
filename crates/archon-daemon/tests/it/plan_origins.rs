//! Where every plan field came from: each writer records the source, who
//! chose it and when, and `get_item` and `archon status` show it.
use crate::common;

use std::collections::BTreeMap;

use archon_core::model::{ModelChoiceDraft, ModelPlanDraft};
use archon_core::plan_origin::{Authority, EntryOrigin, PlanSource, PlannedPlan};
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use archon_core::queue::QueueItem;
use archon_core::steer::Steer;
use common::{Harness, MODEL, PIPELINE, REPO_NAME, REVIEW_MODEL};

type Origins = BTreeMap<String, Vec<EntryOrigin>>;

fn named_model(stage: &str, model: &str) -> ModelPlanDraft {
    let choice = ModelChoiceDraft {
        model: Some(model.parse().unwrap()),
        ..Default::default()
    };
    ModelPlanDraft([(stage.to_owned(), vec![choice])].into())
}

fn enqueue(plan: ModelPlanDraft) -> Request {
    Request::Enqueue(EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: "Origins".into(),
        brief: "Apply the supplied fix.".into(),
        priority: Default::default(),
        depends_on: vec![],
        tags: vec![],
        model_plan: plan,
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    })
}

async fn item(reply: Result<Reply, archon_core::error::ArchonError>) -> QueueItem {
    match reply.expect("accepted") {
        Reply::Item(item) => item,
        other => panic!("replied {other:?}"),
    }
}

/// The origins `get_item` shows for `item`.
pub(crate) async fn shown(h: &Harness, item: &QueueItem) -> Origins {
    let request = Request::GetItem {
        item: item.id.clone(),
    };
    let Reply::ItemDetail(detail) = h.cli(request).await.expect("detail") else {
        panic!("get_item replies with the detail");
    };
    assert_eq!(
        detail.plan_origins.keys().collect::<Vec<_>>(),
        detail.item.model_plan.0.keys().collect::<Vec<_>>(),
        "an origin for every entry"
    );
    detail.plan_origins
}

pub(crate) async fn unattributed(h: &Harness) -> u32 {
    let Reply::Status(status) = h
        .cli(Request::Status(Default::default()))
        .await
        .expect("status")
    else {
        panic!("status replies with the status");
    };
    status.unattributed_plan_fields
}

fn sources(origin: &EntryOrigin) -> (PlanSource, PlanSource) {
    (origin.model.source, origin.effort.source)
}

#[tokio::test]
async fn an_enqueue_naming_one_model_pins_it_and_the_daemon_fills_the_rest() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(named_model("implement", MODEL))).await).await;
    let origins = shown(&h, &queued).await;
    let implement = &origins["implement"][0];
    assert_eq!(
        (implement.model.source, implement.model.by),
        (PlanSource::Explicit, Authority::Operator)
    );
    assert_eq!(
        (implement.effort.source, implement.effort.by),
        (PlanSource::Sizer, Authority::Daemon),
        "the effort nobody named is sized from the stage's default"
    );
    let was = implement.effort.note.as_deref().unwrap_or_default();
    assert_eq!(was, "the stage's default effort; was fallback: medium");
    assert!(implement.model.at.is_some() && implement.effort.at.is_some());
    let filled = [PlanSource::Learned, PlanSource::Tier, PlanSource::Fallback];
    for (key, entries) in origins.iter().filter(|(key, _)| *key != "implement") {
        for origin in entries {
            assert!(filled.contains(&origin.model.source), "{key}: {origin:?}");
            assert_eq!(origin.model.by, Authority::Daemon, "{key}");
            assert!(!origin.model.is_pin() && !origin.effort.is_pin(), "{key}");
        }
    }
    assert!(origins.len() > 1, "{origins:?}");
    assert_eq!(unattributed(&h).await, 0);
}

#[tokio::test]
async fn the_orchestrator_is_recorded_as_the_one_who_chose() {
    let h = Harness::new().await;
    let queued = item(
        h.orchestrator(enqueue(named_model("review", REVIEW_MODEL)))
            .await,
    )
    .await;
    let origins = shown(&h, &queued).await;
    for origin in &origins["review"] {
        assert_eq!(
            (origin.model.source, origin.model.by),
            (PlanSource::Explicit, Authority::Orchestrator)
        );
    }
    assert_eq!(origins["implement"][0].model.by, Authority::Daemon);
}

/// `request` as `archon` sends it for the agent its environment names.
async fn forwarded_by_the_cli(h: &Harness, request: Request) -> QueueItem {
    h.ctx
        .store
        .lock()
        .await
        .kv_set(
            archon_queue::store::keys::ORCHESTRATOR_AGENT,
            common::ORCHESTRATOR,
        )
        .unwrap();
    let forwarded = Request::ForAgent {
        caller: archon_core::aether_ids::AetherAgentId::parse(common::ORCHESTRATOR).unwrap(),
        request: Box::new(request),
    };
    item(h.cli(forwarded).await).await
}

#[tokio::test]
async fn the_orchestrator_forwarded_by_the_cli_is_recorded_as_the_one_who_chose() {
    let h = Harness::new().await;
    let queued = forwarded_by_the_cli(&h, enqueue(named_model("review", REVIEW_MODEL))).await;
    let review = &shown(&h, &queued).await["review"][0];
    assert_eq!(
        (review.model.source, review.model.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    let steer = Request::SteerItem {
        item: queued.id.clone(),
        steer: Steer {
            stage: Some("implement".into()),
            model: Some(MODEL.parse().unwrap()),
            ..Steer::default()
        },
    };
    forwarded_by_the_cli(&h, steer).await;
    let implement = &shown(&h, &queued).await["implement"][0];
    assert_eq!(
        (implement.model.source, implement.model.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    let mine = item(h.cli(enqueue(named_model("review", REVIEW_MODEL))).await).await;
    assert_eq!(
        shown(&h, &mine).await["review"][0].model.by,
        Authority::Operator,
        "the operator's own CLI call"
    );
}

#[tokio::test]
async fn a_retry_that_keeps_the_plan_keeps_its_origins() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(named_model("implement", MODEL))).await).await;
    let before = shown(&h, &queued).await;
    let cancel = Request::CancelItem {
        item: queued.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let retry = Request::RetryItem {
        item: queued.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    let retried = item(h.cli(retry).await).await;
    assert_eq!(retried.id, queued.id);
    assert_eq!(*retried.model_plan, *queued.model_plan);
    assert_eq!(shown(&h, &retried).await, before);
    assert_eq!(unattributed(&h).await, 0);
}

#[tokio::test]
async fn an_update_pins_what_it_names_and_keeps_the_rest() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(ModelPlanDraft::default())).await).await;
    let before = shown(&h, &queued).await;
    let patch = ItemPatch {
        model_plan: Some(named_model("implement", REVIEW_MODEL)),
        ..ItemPatch::default()
    };
    let update = Request::UpdateItem {
        item: queued.id.clone(),
        patch,
    };
    item(h.orchestrator(update).await).await;
    let after = shown(&h, &queued).await;
    let implement = &after["implement"][0];
    assert_eq!(
        (implement.model.source, implement.model.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    assert_ne!(after["implement"], before["implement"]);
    assert_eq!(unattributed(&h).await, 0);
}

#[tokio::test]
async fn a_steer_naming_the_stored_effort_still_records_a_pin() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(named_model("implement", MODEL))).await).await;
    let stored = queued.model_plan.choices_for("implement").unwrap()[0].effort;
    let before = &shown(&h, &queued).await["implement"][0];
    assert!(!before.effort.is_pin(), "{before:?}");
    let asked = Steer {
        stage: Some("implement".into()),
        effort: Some(stored),
        ..Steer::default()
    };
    let request = Request::SteerItem {
        item: queued.id.clone(),
        steer: asked,
    };
    let steered = item(h.orchestrator(request).await).await;
    assert_eq!(
        *steered.model_plan, *queued.model_plan,
        "the value is the same"
    );
    let after = &shown(&h, &queued).await["implement"][0];
    assert_eq!(
        (after.effort.source, after.effort.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    assert_eq!(after.model, before.model, "the model was not named");
    let note = after.effort.note.as_deref().unwrap_or_default();
    assert!(note.starts_with("was sizer"), "{note}");
    assert_eq!(unattributed(&h).await, 0);
}

#[tokio::test]
async fn an_item_stored_before_origins_reads_as_legacy_pins() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(ModelPlanDraft::default())).await).await;
    let mut stored = h.item(&queued.id).await;
    stored.model_plan = PlannedPlan::legacy(stored.model_plan.into_plan());
    h.ctx.store.lock().await.update_item(&stored).unwrap();
    let raw = serde_json::to_value(h.item(&queued.id).await).unwrap();
    assert!(
        raw.get("model_origins").is_none(),
        "stored as 1.6.3 stores it"
    );
    for entries in shown(&h, &queued).await.values() {
        for origin in entries {
            assert_eq!(
                sources(origin),
                (PlanSource::LegacyUnknown, PlanSource::LegacyUnknown)
            );
            assert!(origin.model.is_pin() && origin.effort.is_pin());
        }
    }
    assert_eq!(
        unattributed(&h).await,
        0,
        "a legacy field is not unattributed"
    );
}

/// A clone saved under a new id without `copy_for` goes through the store's
/// own guard: a test build fails loudly, a release build counts its pins.
#[tokio::test]
#[cfg_attr(debug_assertions, should_panic(expected = "origins written for"))]
async fn origins_copied_onto_another_item_are_counted_as_unattributed() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(ModelPlanDraft::default())).await).await;
    let fields: usize = queued.model_plan.0.values().map(Vec::len).sum();
    let clone = QueueItem {
        id: archon_core::id::QueueItemId::new(),
        ..h.item(&queued.id).await
    };
    assert!(clone.model_plan.fault(&clone.id).is_some());
    h.ctx.store.lock().await.insert_item(&clone, None).unwrap();
    assert!(unattributed(&h).await as usize >= 2 * fields);
    for entries in shown(&h, &clone).await.values() {
        assert!(
            entries
                .iter()
                .all(|o| sources(o) == (PlanSource::Unattributed, PlanSource::Unattributed))
        );
    }
}

#[tokio::test]
async fn an_update_without_a_plan_carries_every_origin() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(named_model("implement", MODEL))).await).await;
    let before = shown(&h, &queued).await;
    let patch = ItemPatch {
        title: Some("Renamed".into()),
        ..ItemPatch::default()
    };
    let update = Request::UpdateItem {
        item: queued.id.clone(),
        patch,
    };
    let updated = item(h.orchestrator(update).await).await;
    assert_eq!(updated.title, "Renamed");
    assert_eq!(shown(&h, &queued).await, before, "nothing re-attributed");
    assert_eq!(unattributed(&h).await, 0);
}

#[tokio::test]
async fn a_retry_with_a_plan_pins_what_it_names_for_the_one_who_retried() {
    let h = Harness::new().await;
    let queued = item(h.cli(enqueue(named_model("implement", MODEL))).await).await;
    let cancel = Request::CancelItem {
        item: queued.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let retry = Request::RetryItem {
        item: queued.id.clone(),
        model_plan: Some(named_model("implement", REVIEW_MODEL)),
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    item(h.orchestrator(retry).await).await;
    let after = shown(&h, &queued).await;
    let implement = &after["implement"][0];
    assert_eq!(
        (implement.model.source, implement.model.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    assert!(!implement.effort.is_pin(), "the effort was not named");
    assert_eq!(unattributed(&h).await, 0);
}
