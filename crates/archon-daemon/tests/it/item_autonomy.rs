//! An item's autonomy knobs changed after enqueue, through `update_item` and
//! `steer_item` (ARCHITECTURE §18.1): within the ceiling only, with the
//! stages and delivery they affect, and for a succeeded item the auto-merge
//! of its open pull request at the reviewed commit. GitHub is a scripted
//! `gh`; nothing here merges anything real.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, MergeMode, ReviewLevel, ToolsMode};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::model::ModelPlan;
use archon_core::protocol::{
    ClientKind, DeviceRole, EnqueueRequest, ItemPatch, Reply, RepoPatch, RepoRef, Request,
    WelcomeDevice,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::Steer;
use archon_daemon::handlers;
use common::delivery::{FakeGh, add_origin, shipped_by};
use common::{Harness, REPO_NAME, git};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";

fn knobs(list: &str) -> AutonomyPatch {
    AutonomyPatch::parse_list(list).expect("knobs")
}

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::with_config(Config::default()).await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// The repository's ceiling, by knob, replacing the default one.
async fn ceiling(h: &Harness, list: &str) {
    let patch = RepoPatch {
        autonomy_max: Some(knobs(list)),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

async fn enqueue(h: &Harness, autonomy: AutonomyPatch) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: "a change".into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        stage_timeouts: Default::default(),
        budget_usd: None,
        idempotency_key: None,
        autonomy,
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await {
        Ok(Reply::Item(item)) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

fn update(item: &QueueItem, list: &str) -> Request {
    Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            autonomy: Some(knobs(list)),
            ..ItemPatch::default()
        },
    }
}

fn web_change(item: &QueueItem, list: &str, steer: bool) -> Request {
    if steer {
        return Request::SteerItem {
            item: item.id.clone(),
            steer: Steer {
                note: Some("This must not be applied on refusal".into()),
                autonomy: Some(knobs(list)),
                ..Steer::default()
            },
        };
    }
    let mut request = update(item, list);
    if let Request::UpdateItem { patch, .. } = &mut request {
        patch.brief_append = Some("This must not be applied on refusal".into());
    }
    request
}

#[tokio::test]
async fn web_item_changes_obey_every_ceiling_without_partial_writes() {
    let (h, _gh) = delivering().await;
    ceiling(
        &h,
        "review=strict,proposal=lavish,report=before-pr,merge=manual,tools=guarded",
    )
    .await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    for steer in [false, true] {
        for knob in [
            "review=none",
            "proposal=off",
            "report=off",
            "merge=auto",
            "tools=yolo",
        ] {
            let result = handlers::handle(
                &h.ctx,
                ClientKind::ArchonWeb,
                web_change(&item, knob, steer),
            )
            .await;
            assert!(
                matches!(result, Err(ArchonError::Forbidden(_))),
                "{knob}, steer={steer}: {result:?}"
            );
            let stored = h.item(&item.id).await;
            assert_eq!(stored.autonomy, item.autonomy, "{knob}, steer={steer}");
            assert_eq!(stored.brief, item.brief, "{knob}, steer={steer}");
        }
    }
}

#[tokio::test]
async fn web_cannot_set_privileged_knobs_even_when_the_ceiling_allows_them() {
    let (h, _gh) = delivering().await;
    ceiling(&h, "merge=auto,tools=yolo").await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    for steer in [false, true] {
        for knob in ["merge=auto", "tools=yolo"] {
            let before = h.item(&item.id).await;
            let result = handlers::handle(
                &h.ctx,
                ClientKind::ArchonWeb,
                web_change(&item, knob, steer),
            )
            .await;
            assert!(
                matches!(result, Err(ArchonError::Forbidden(_))),
                "{knob}, steer={steer}: {result:?}"
            );
            let stored = h.item(&item.id).await;
            assert_eq!(stored.autonomy, before.autonomy);
            assert_eq!(stored.brief, before.brief);
        }
        let result = handlers::handle(
            &h.ctx,
            ClientKind::ArchonWeb,
            web_change(&item, "review=strict", steer),
        )
        .await;
        assert!(
            matches!(result, Ok(Reply::Item(_))),
            "steer={steer}: {result:?}"
        );
        assert_eq!(h.item(&item.id).await.autonomy.review, ReviewLevel::Strict);
    }
}

fn device(role: DeviceRole) -> WelcomeDevice {
    WelcomeDevice {
        id: "dev_01M621D9ZBJPTMDTV26KKQNBE1".into(),
        name: "phone".into(),
        role,
    }
}

/// An operator-role device sets what the operator's CLI sets, within the
/// ceiling; an observer device is told it observes.
#[tokio::test]
async fn an_operator_device_sets_privileged_knobs_within_the_ceiling_and_an_observer_cannot() {
    let (h, _gh) = delivering().await;
    ceiling(&h, "merge=auto,tools=yolo").await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    let observer = device(DeviceRole::Observer);
    let refused =
        handlers::handle_device(&h.ctx, &observer, web_change(&item, "merge=auto", false));
    let Err(ArchonError::Forbidden(message)) = refused.await else {
        panic!("an observer device set merge=auto");
    };
    assert!(message.contains("observer device"), "{message}");
    let operator = device(DeviceRole::Operator);
    for (knob, steer) in [("merge=auto", false), ("tools=yolo", true)] {
        let result =
            handlers::handle_device(&h.ctx, &operator, web_change(&item, knob, steer)).await;
        assert!(matches!(result, Ok(Reply::Item(_))), "{knob}: {result:?}");
    }
    let stored = h.item(&item.id).await;
    assert_eq!(stored.autonomy.merge, MergeMode::Auto);
    assert_eq!(stored.autonomy.tools, ToolsMode::Yolo);
    ceiling(&h, "merge=manual,tools=guarded").await;
    let fresh = enqueue(&h, AutonomyPatch::default()).await;
    let above = handlers::handle_device(&h.ctx, &operator, web_change(&fresh, "merge=auto", false));
    assert!(
        matches!(above.await, Err(ArchonError::Forbidden(_))),
        "the ceiling still binds"
    );
    assert_eq!(h.item(&fresh.id).await.autonomy, fresh.autonomy);
}

async fn updated(h: &Harness, item: &QueueItem, list: &str) -> QueueItem {
    match h.orchestrator(update(item, list)).await {
        Ok(Reply::Item(item)) => item,
        other => panic!("update replied {other:?}"),
    }
}

/// Deliver the running item as [`PR_ONE`] and return the commit shown.
async fn deliver(h: &Harness, item: &QueueItem) -> String {
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(
        matches!(&state, QueueState::Succeeded { summary, .. } if summary.contains(PR_ONE)),
        "the item shipped: {state:?}"
    );
    git(&h.repo, &["rev-parse", &h.run(&item.id).await.branch])
}

async fn shipped(h: &Harness, autonomy: AutonomyPatch) -> (QueueItem, String) {
    let item = enqueue(h, autonomy).await;
    h.dispatch().await;
    let head = deliver(h, &item).await;
    (item, head)
}

fn merge_calls(gh: &FakeGh) -> Vec<String> {
    let calls = gh.calls();
    calls
        .into_iter()
        .filter(|c| c.starts_with("pr merge"))
        .collect()
}

fn green() -> Value {
    json!([{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}])
}

#[tokio::test]
async fn a_knob_above_the_ceiling_is_refused_and_one_within_it_is_set() {
    let (h, _gh) = delivering().await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    let refused = h.orchestrator(update(&item, "merge=auto")).await;
    let Err(ArchonError::Forbidden(why)) = refused else {
        panic!("above the ceiling: {refused:?}");
    };
    assert!(
        why.contains("merge=auto") && why.contains("manual"),
        "{why}"
    );
    assert_eq!(h.item(&item.id).await.autonomy.merge, MergeMode::Manual);

    let yolo = updated(&h, &item, "tools=yolo").await;
    assert_eq!(yolo.autonomy.tools, ToolsMode::Yolo);
    ceiling(&h, "merge=auto").await;
    let auto = updated(&h, &item, "merge=auto").await;
    assert_eq!(
        (auto.autonomy.merge, auto.autonomy.tools),
        (MergeMode::Auto, ToolsMode::Yolo),
        "the knob set before stays"
    );
    assert_eq!(auto.autonomy.review, item.autonomy.review);
}

#[tokio::test]
async fn a_knob_that_a_lowered_ceiling_left_above_it_does_not_block_another() {
    let (h, _gh) = delivering().await;
    ceiling(&h, "merge=auto").await;
    let item = enqueue(&h, knobs("merge=auto")).await;
    ceiling(&h, "merge=manual").await;
    let lowered = updated(&h, &item, "tools=yolo").await;
    assert_eq!(lowered.autonomy.merge, MergeMode::Auto, "left as it was");
    assert_eq!(lowered.autonomy.tools, ToolsMode::Yolo);
}

#[tokio::test]
async fn a_queued_item_gets_the_stages_its_new_knobs_add_and_loses_those_they_drop() {
    let (h, _gh) = delivering().await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    assert!(item.model_plan.choices_for("propose").is_none());
    let with = updated(&h, &item, "proposal=lavish").await;
    assert!(with.model_plan.choices_for("propose").is_some());
    let without = updated(&h, &with, "proposal=off").await;
    assert!(without.model_plan.choices_for("propose").is_none());
    assert_eq!(without.model_plan, item.model_plan);
}

#[tokio::test]
async fn a_running_item_takes_merge_and_tools_but_not_stages_it_started_without() {
    let (h, gh) = delivering().await;
    ceiling(&h, "merge=auto").await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    h.dispatch().await;
    let stages = h.running().await;
    assert_eq!(stages.len(), 1, "running");

    let refused = h.orchestrator(update(&item, "proposal=lavish")).await;
    assert!(
        matches!(&refused, Err(ArchonError::InvalidTransition(why)) if why.contains("only merge and tools")),
        "{refused:?}"
    );
    let running = updated(&h, &item, "tools=yolo,merge=auto").await;
    assert_eq!(running.autonomy.merge, MergeMode::Auto);
    let run = h.run(&item.id).await;
    let vars = h.kv(&format!("run_vars:{}", run.id)).await.unwrap();
    assert!(vars.contains(r#""ship.auto_merge":"true""#), "{vars}");

    gh.fail_next("pr merge", "HTTP 502: Bad Gateway");
    deliver(&h, &item).await;
    let owed = h.kv(&format!("ship_automerge:{}", run.id)).await;
    assert!(
        owed.is_some(),
        "delivery owes the auto-merge it was told of"
    );
}

#[tokio::test]
async fn a_succeeded_item_turns_on_auto_merge_only_at_the_reviewed_head() {
    let (h, gh) = delivering().await;
    ceiling(&h, "merge=auto").await;
    let (item, head) = shipped(&h, AutonomyPatch::default()).await;
    gh.show(&head, "MERGEABLE", green());

    let patch = ItemPatch {
        priority: Some(Default::default()),
        autonomy: Some(knobs("merge=auto")),
        ..ItemPatch::default()
    };
    let request = Request::UpdateItem {
        item: item.id.clone(),
        patch,
    };
    let mixed = h.orchestrator(request).await;
    assert!(
        matches!(mixed, Err(ArchonError::InvalidTransition(_))),
        "{mixed:?}"
    );
    let review = h.orchestrator(update(&item, "review=strict")).await;
    assert!(
        matches!(review, Err(ArchonError::InvalidTransition(_))),
        "{review:?}"
    );
    assert!(merge_calls(&gh).is_empty());

    let moved = "1".repeat(40);
    gh.show(&moved, "MERGEABLE", green());
    let refused = h.orchestrator(update(&item, "merge=auto")).await;
    assert!(
        matches!(&refused, Err(ArchonError::InvalidTransition(why)) if why.contains("head moved")),
        "{refused:?}"
    );
    assert!(merge_calls(&gh).is_empty(), "nothing was written");
    assert_eq!(h.item(&item.id).await.autonomy.merge, MergeMode::Manual);

    gh.show(&head, "MERGEABLE", green());
    let auto = updated(&h, &item, "merge=auto").await;
    assert_eq!(auto.autonomy.merge, MergeMode::Auto);
    let pinned = format!("pr merge {PR_ONE} --auto --squash --match-head-commit {head}");
    assert_eq!(merge_calls(&gh), [pinned]);
    assert!(!gh.merged(), "delivered, not merged");
}

#[tokio::test]
async fn a_succeeded_item_above_the_ceiling_writes_nothing() {
    let (h, gh) = delivering().await;
    let (item, head) = shipped(&h, AutonomyPatch::default()).await;
    gh.show(&head, "MERGEABLE", green());
    let refused = h.orchestrator(update(&item, "merge=auto")).await;
    assert!(
        matches!(refused, Err(ArchonError::Forbidden(_))),
        "{refused:?}"
    );
    assert!(merge_calls(&gh).is_empty());
    assert_eq!(h.item(&item.id).await.autonomy.merge, MergeMode::Manual);

    let confirmed =
        handlers::update_item_autonomy_above_ceiling(&h.ctx, &item.id, knobs("merge=auto"));
    let Ok(Reply::Item(auto)) = confirmed.await else {
        panic!("a confirmed request lifts the ceiling");
    };
    assert_eq!(auto.autonomy.merge, MergeMode::Auto);
    assert_eq!(
        merge_calls(&gh).len(),
        1,
        "the confirmed head is pinned too"
    );
}

#[tokio::test]
async fn lowering_merge_withdraws_an_auto_merge_still_owed() {
    let (h, gh) = delivering().await;
    ceiling(&h, "merge=auto").await;
    gh.fail_next("pr merge", "HTTP 502: Bad Gateway");
    let (item, _) = shipped(&h, knobs("merge=auto")).await;
    let run = h.run(&item.id).await;
    let owed = format!("ship_automerge:{}", run.id);
    assert!(h.kv(&owed).await.is_some(), "owed after delivery");
    let manual = updated(&h, &item, "merge=manual").await;
    assert_eq!(manual.autonomy.merge, MergeMode::Manual);
    assert_eq!(h.kv(&owed).await, None, "no longer owed");
}

#[tokio::test]
async fn an_item_with_no_pull_request_takes_no_auto_merge() {
    let (h, _gh) = delivering().await;
    ceiling(&h, "merge=auto").await;
    let local = RepoPatch {
        ship: Some(archon_core::repo::ShipMode::Local),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: local,
    };
    h.cli(request).await.expect("repo update");
    let item = enqueue(&h, AutonomyPatch::default()).await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(&h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    let refused = h.orchestrator(update(&item, "merge=auto")).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(why)) if why.contains("no pull request")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn steer_item_takes_autonomy_alone_or_with_a_note_and_refuses_above_the_ceiling() {
    let (h, _gh) = delivering().await;
    let item = enqueue(&h, AutonomyPatch::default()).await;
    let steer = |autonomy: &str, note: Option<&str>| Request::SteerItem {
        item: item.id.clone(),
        steer: Steer {
            note: note.map(str::to_owned),
            autonomy: Some(knobs(autonomy)),
            ..Steer::default()
        },
    };
    let above = h
        .orchestrator(steer("merge=auto", Some("Mind the API.")))
        .await;
    assert!(matches!(above, Err(ArchonError::Forbidden(_))), "{above:?}");
    assert_eq!(
        h.item(&item.id).await.brief,
        item.brief,
        "nothing was steered"
    );

    let Ok(Reply::Item(alone)) = h.orchestrator(steer("tools=yolo", None)).await else {
        panic!("autonomy alone steers");
    };
    assert_eq!(alone.autonomy.tools, ToolsMode::Yolo);
    let both = h
        .orchestrator(steer("review=strict", Some("Mind the API.")))
        .await;
    let Ok(Reply::Item(both)) = both else {
        panic!("a note with autonomy steers");
    };
    assert_eq!(both.autonomy.review, ReviewLevel::Strict);
    assert!(both.brief.contains("Mind the API."), "{}", both.brief);
}

#[tokio::test]
async fn restaging_keeps_explicit_reviewer_choices_and_reasons() {
    let (h, _gh) = delivering().await;
    let item = enqueue(&h, knobs("review=standard")).await;
    let reviewers = vec![archon_core::model::ModelChoice {
        model: common::MODEL.parse().unwrap(),
        effort: archon_core::model::Effort::Low,
        tier: None,
    }];
    let mut plan = item.model_plan.clone();
    plan.0.insert("implement.review".into(), reviewers.clone());
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            model_plan: Some(plan.into()),
            model_reasons: Some(
                [(
                    "implement.review".into(),
                    "Use the selected reviewer".into(),
                )]
                .into(),
            ),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let revised = updated(&h, &item, "proposal=lavish").await;
    assert_eq!(revised.model_plan.0["implement.review"], reviewers);
    assert_eq!(
        revised.model_reasons.unwrap()["implement.review"],
        "Use the selected reviewer"
    );
}
