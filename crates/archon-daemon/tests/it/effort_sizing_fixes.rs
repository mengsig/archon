//! Effort sizing across quota reroutes, replacements and concurrent updates
//! (A11), on the helpers of [`crate::effort_sizing`].
use std::time::Duration;

use crate::effort_sizing::*;
use archon_core::config::Config;
use archon_core::effort_sizing::RiskClass;
use archon_core::model::Effort::{High, Medium, XHigh};
use archon_core::plan_origin::{PlanSource, PlannedPlan};
use archon_core::protocol::{ClientKind, ItemPatch, Request};
use archon_core::queue::QueueItem;
use archon_core::steer::Steer;

/// The plan a 1.6.3 daemon stored for `item`: its choices and origins, no sizing.
fn never_sized(item: &QueueItem) -> PlannedPlan {
    let mut stored = serde_json::to_value(&item.model_plan).unwrap();
    let removed = stored.as_object_mut().unwrap().remove("effort_sizing");
    assert!(removed.is_some(), "the item was sized");
    let legacy: PlannedPlan = serde_json::from_value(stored).unwrap();
    assert!(legacy.sizing().is_none());
    legacy
}

async fn store(h: &Harness, item: &QueueItem) {
    h.ctx.store.lock().await.update_item(item).expect("stored");
}

/// E3': a quota move lowers a stored sized x_high after the driver read it;
/// the next spawn runs what the stored plan holds now, as a recovered run would.
#[tokio::test]
async fn a_spawn_runs_the_stored_plan_s_effort_after_it_was_lowered_under_the_driver() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    h.dispatch().await;
    assert_eq!(
        attempt(&h, &queued.id, "implement").await.choice.effort,
        XHigh
    );
    let mut moved = h.item(&queued.id).await;
    assert!(
        moved
            .model_plan
            .clip_sized(&moved.id, Some(High), archon_core::now())
    );
    store(&h, &moved).await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let review = attempt(&h, &queued.id, "review").await;
    assert_eq!(
        review.choice.effort, High,
        "the stored plan, not the driver's copy"
    );
    let (class, source, why) = recorded(&review);
    assert_eq!(
        (class, source),
        (Some(RiskClass::Critical), Some(PlanSource::Sizer))
    );
    assert_eq!(
        why,
        "sizer: critical -> x_high; clipped from x_high to the cap high"
    );
    moved = h.item(&queued.id).await;
    assert_eq!(efforts(&moved)[1], ("review", High));
}

/// A held run reserves no new spawn until the request holding it ends.
#[tokio::test]
async fn a_held_run_reserves_nothing_until_it_is_released() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    h.dispatch().await;
    let run = h.run(&queued.id).await.id;
    h.ctx.runs.lock().await.hold(&run);
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let started = |stages: Vec<archon_core::run::StageRun>| {
        stages.iter().any(|stage| stage.stage_id == "review")
    };
    assert!(!started(h.stages(&queued.id).await), "held");
    h.ctx.runs.lock().await.release(&run);
    assert!(!h.ctx.runs.lock().await.is_held(&run));
    h.dispatch().await;
    assert!(started(h.stages(&queued.id).await), "released");
}

/// Check 1: one update of a running item that amends its brief and sets its
/// tags sizes the stages not yet started once, for both, and holds nothing after.
#[tokio::test]
async fn a_running_update_with_a_note_and_tags_sizes_the_later_stages_for_both() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    h.dispatch().await;
    let patch = ItemPatch {
        brief_append: Some("It changes how a credential is stored.".into()),
        tags: Some(vec!["risk:critical".into()]),
        ..ItemPatch::default()
    };
    let updated = update(&h, &queued.id, patch).await;
    assert_eq!(
        efforts(&updated),
        [("implement", Medium), ("review", XHigh), ("ship", XHigh)]
    );
    let run = h.run(&queued.id).await.id;
    assert!(!h.ctx.runs.lock().await.is_held(&run), "released");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let review = attempt(&h, &queued.id, "review").await;
    assert_eq!(review.choice.effort, XHigh);
    assert_eq!(recorded(&review).0, Some(RiskClass::Critical));
}

/// Check 1: an update whose item gained a sizing while it waited for the run
/// table still sizes it for the new tags: eligibility is read under the lock.
#[tokio::test]
async fn an_update_sizes_a_plan_that_was_sized_while_it_waited_for_the_lock() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    let mut legacy = queued.clone();
    legacy.model_plan = never_sized(&queued);
    store(&h, &legacy).await;
    let table = h.ctx.runs.lock().await;
    let ctx = h.ctx.clone();
    let id = queued.id.clone();
    let tagged = tokio::spawn(async move {
        let patch = ItemPatch {
            tags: Some(vec!["risk:critical".into()]),
            ..ItemPatch::default()
        };
        let request = Request::UpdateItem { item: id, patch };
        archon_daemon::handlers::handle(&ctx, ClientKind::ArchonCli, request).await
    });
    // The update reads the legacy plan, then waits for the run table.
    tokio::time::sleep(Duration::from_millis(300)).await;
    store(&h, &queued).await;
    drop(table);
    let reply = tokio::time::timeout(Duration::from_secs(10), tagged).await;
    item(reply.expect("in time").expect("joined")).await;
    let stored = h.item(&queued.id).await;
    assert_eq!(class(&stored), Some(RiskClass::Critical));
    assert_eq!(efforts(&stored)[2], ("ship", XHigh));
}

/// Check 2: a retry's amendment leaves the carried sizing frozen, and a later
/// steer that changes no sizing input does not size it again from that brief.
#[tokio::test]
async fn a_steer_that_moves_no_sizing_input_leaves_a_retried_plan_frozen() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    cancel(&h, &queued.id).await;
    let request = Request::RetryItem {
        item: queued.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: Some("It changes how a credential is stored.".into()),
        stage_timeouts: Default::default(),
    };
    let retried = item(h.cli(request).await).await;
    assert_eq!(efforts(&retried), efforts(&queued), "carried frozen");
    let timeouts = Steer {
        stage_timeouts: [("implement".to_owned(), 40)].into(),
        ..Steer::default()
    };
    let steered = steer(&h, &queued.id, timeouts).await;
    assert_eq!(steered.model_plan.plan(), retried.model_plan.plan());
    assert_eq!(class(&steered), Some(RiskClass::Ordinary));
    let same_tags = ItemPatch {
        tags: Some(Vec::new()),
        ..ItemPatch::default()
    };
    let patched = update(&h, &queued.id, same_tags).await;
    assert_eq!(patched.model_plan.plan(), retried.model_plan.plan());

    // A note moves the brief: the item is sized for all it reads now.
    let noted = steer(&h, &queued.id, note("Keep the old layout.")).await;
    assert_eq!(class(&noted), Some(RiskClass::Elevated));
}
