//! The effort sizer (design 7.6, A11): an unpinned effort is sized from the
//! item's own title, brief, tags and review level and each stage's own
//! default, and the decision is frozen in the stored plan (ruling E3').
use std::collections::BTreeMap;

pub(crate) use crate::common::{Harness, MODEL, REPO_NAME, REVIEW_MODEL, completed};
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::config::Config;
use archon_core::effort_sizing::RiskClass;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{Effort, ModelChoiceDraft, ModelPlanDraft};
use archon_core::plan_origin::PlanSource;
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{AttemptSizing, StageRun};
use archon_core::steer::{InsertStage, Steer};
use archon_daemon::executor;

/// Stage defaults that differ, so a borrowed effort shows: `implement` and
/// `review` at medium, `ship` at low.
pub(crate) const SIZED: &str = "sized";
const SIZED_TOML: &str = r#"[pipeline]
name = "sized"
description = "stages with defaults of their own"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
effort = "medium"
timeout_minutes = 30

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
effort = "medium"
gate = "all_pass"
timeout_minutes = 30
on_fail = { goto = "implement", max_loops = 1 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
effort = "low"
timeout_minutes = 90
"#;

/// The pipeline a steer borrows `verify` from: its default is high, which no
/// stage of [`SIZED`] has.
pub(crate) const DONOR: &str = "sized-donor";
const DONOR_TOML: &str = r#"[pipeline]
name = "sized-donor"
description = "a stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "verify"
kind = "work"
prompt = "implement.md"
effort = "high"
timeout_minutes = 30
"#;

pub(crate) async fn harness(config: Config) -> Harness {
    let h = Harness::with_config(config).await;
    h.add_pipeline(SIZED, SIZED_TOML);
    h.add_pipeline(DONOR, DONOR_TOML);
    h
}

pub(crate) fn sizer_max(effort: Effort) -> Config {
    let mut config = Config::default();
    config.models.sizer_max_effort = Some(effort);
    config
}

pub(crate) fn sizing_off() -> Config {
    let mut config = Config::default();
    config.models.effort_sizing = Some(false);
    config
}

/// A model for every stage and an effort for none, so every effort is the daemon's.
pub(crate) fn models() -> ModelPlanDraft {
    let named = |model: &str| ModelChoiceDraft {
        model: Some(model.parse().unwrap()),
        ..Default::default()
    };
    let stages = [
        ("implement", MODEL),
        ("review", REVIEW_MODEL),
        ("ship", MODEL),
    ];
    ModelPlanDraft(
        stages
            .into_iter()
            .map(|(stage, model)| (stage.to_owned(), vec![named(model)]))
            .collect(),
    )
}

/// [`models`] with `effort` pinned on `stage`.
pub(crate) fn pinned(stage: &str, effort: Effort) -> ModelPlanDraft {
    let mut plan = models();
    plan.0.get_mut(stage).unwrap()[0].effort = Some(effort);
    plan
}

pub(crate) fn request(title: &str, tags: &[&str], plan: ModelPlanDraft) -> EnqueueRequest {
    EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(SIZED.into()),
        title: title.into(),
        brief: "Apply the supplied change.".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        model_plan: plan,
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    }
}

/// `asked` with the reason an effort above `implement`'s default needs.
pub(crate) fn reasoned(mut asked: EnqueueRequest) -> EnqueueRequest {
    asked.model_reasons = Some([("implement".to_owned(), "a hard change".to_owned())].into());
    asked
}

pub(crate) async fn item(reply: Result<Reply, ArchonError>) -> QueueItem {
    match reply.expect("accepted") {
        Reply::Item(item) => item,
        other => panic!("replied {other:?}"),
    }
}

pub(crate) async fn enqueue(h: &Harness, title: &str, tags: &[&str]) -> QueueItem {
    let queued = item(
        h.cli(Request::Enqueue(request(title, tags, models())))
            .await,
    )
    .await;
    assert_eq!(
        queued,
        h.item(&queued.id).await,
        "the reply is the item as stored"
    );
    queued
}

pub(crate) fn efforts(item: &QueueItem) -> Vec<(&str, Effort)> {
    let plan = &item.model_plan;
    plan.0
        .iter()
        .map(|(key, choices)| (key.as_str(), choices[0].effort))
        .collect()
}

pub(crate) fn class(item: &QueueItem) -> Option<RiskClass> {
    item.model_plan
        .sizing()
        .and_then(|sizing| sizing.risk_class)
}

pub(crate) fn reason(item: &QueueItem, key: &str) -> String {
    let reasons = item.model_plan.effort_reasons(&item.id);
    reasons
        .get(key)
        .map_or(String::new(), |slots| slots[0].clone())
}

pub(crate) fn effort_source(item: &QueueItem, key: &str) -> PlanSource {
    item.model_plan.origin(&item.id, key, 0).effort.source
}

pub(crate) async fn update(h: &Harness, id: &QueueItemId, patch: ItemPatch) -> QueueItem {
    let request = Request::UpdateItem {
        item: id.clone(),
        patch,
    };
    let updated = item(h.orchestrator(request).await).await;
    assert_eq!(updated, h.item(id).await, "the reply is the item as stored");
    updated
}

pub(crate) async fn steer(h: &Harness, id: &QueueItemId, steer: Steer) -> QueueItem {
    let request = Request::SteerItem {
        item: id.clone(),
        steer,
    };
    item(h.orchestrator(request).await).await
}

pub(crate) fn note(text: &str) -> Steer {
    Steer {
        note: Some(text.into()),
        ..Steer::default()
    }
}

pub(crate) fn insert_verify() -> Steer {
    Steer {
        insert_stage: Some(InsertStage {
            stage: "verify".into(),
            from: Some(DONOR.into()),
            after: Some("implement".into()),
        }),
        ..Steer::default()
    }
}

pub(crate) async fn cancel(h: &Harness, id: &QueueItemId) {
    let request = Request::CancelItem {
        item: id.clone(),
        reason: None,
    };
    h.cli(request).await.expect("cancel");
}

pub(crate) async fn retry(h: &Harness, id: &QueueItemId) -> Result<QueueItem, ArchonError> {
    let request = Request::RetryItem {
        item: id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    match h.cli(request).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("retry replied {other:?}"),
    }
}

/// The running attempt of `stage` of item `id`.
pub(crate) async fn attempt(h: &Harness, id: &QueueItemId, stage: &str) -> StageRun {
    let stages = h.stages(id).await;
    let found = stages.into_iter().rfind(|s| s.stage_id == stage);
    found.unwrap_or_else(|| panic!("no attempt of {stage}"))
}

pub(crate) fn recorded(stage: &StageRun) -> (Option<RiskClass>, Option<PlanSource>, &str) {
    let AttemptSizing {
        risk_class,
        effort_source,
        effort_reasons,
    } = &stage.sizing;
    (
        *risk_class,
        *effort_source,
        effort_reasons.as_deref().unwrap_or_default(),
    )
}

use Effort::{High, Low, Medium, XHigh};

#[tokio::test]
async fn an_enqueue_sizes_each_stage_from_its_own_default_and_the_item_s_class() {
    let h = harness(Config::default()).await;
    let ordinary = enqueue(&h, "Add a button", &[]).await;
    assert_eq!(class(&ordinary), Some(RiskClass::Ordinary));
    assert_eq!(
        efforts(&ordinary),
        [("implement", Medium), ("review", Medium), ("ship", Low)]
    );
    assert_eq!(reason(&ordinary, "ship"), "sizer: ordinary -> low");
    assert_eq!(effort_source(&ordinary, "implement"), PlanSource::Sizer);

    let elevated = enqueue(&h, "Fix the deadlock in the pump", &[]).await;
    assert_eq!(class(&elevated), Some(RiskClass::Elevated));
    assert_eq!(
        efforts(&elevated),
        [("implement", High), ("review", High), ("ship", High)]
    );
    assert_eq!(reason(&elevated, "implement"), "sizer: elevated -> high");

    let critical = enqueue(&h, "Add a button", &["risk:critical"]).await;
    assert_eq!(class(&critical), Some(RiskClass::Critical));
    assert_eq!(
        efforts(&critical),
        [("implement", XHigh), ("review", XHigh), ("ship", XHigh)]
    );
    assert_eq!(reason(&critical, "review"), "sizer: critical -> x_high");

    let mechanical = enqueue(&h, "docs: fix a typo", &[]).await;
    assert_eq!(class(&mechanical), Some(RiskClass::Mechanical));
    assert_eq!(
        efforts(&mechanical),
        [("implement", Low), ("review", Low), ("ship", Low)]
    );
}

#[tokio::test]
async fn the_sizer_s_own_cap_and_the_policy_s_cap_both_hold_a_sized_effort() {
    let h = harness(sizer_max(High)).await;
    let capped = enqueue(&h, "Add a button", &["risk:critical"]).await;
    assert_eq!(
        efforts(&capped),
        [("implement", High), ("review", High), ("ship", High)]
    );
    let why = "sizer: critical -> x_high; capped at high; consider a stronger tier";
    assert_eq!(reason(&capped, "implement"), why);

    let h = harness(Config::default()).await;
    h.repo_config("[models]\nmax_effort = \"medium\"\n");
    let held = enqueue(&h, "Add a button", &["risk:critical"]).await;
    assert_eq!(
        efforts(&held),
        [("implement", Medium), ("review", Medium), ("ship", Medium)]
    );
    assert!(
        reason(&held, "ship").contains("capped at medium"),
        "{}",
        reason(&held, "ship")
    );
}

/// Check 6: a pin keeps its effort, and the record says what the sizer advised.
#[tokio::test]
async fn a_pinned_effort_stays_and_records_what_the_sizer_advised() {
    let h = harness(Config::default()).await;
    let plan = pinned("implement", Medium);
    let asked = request("Add a button", &["risk:critical"], plan);
    let queued = item(h.cli(Request::Enqueue(asked)).await).await;
    assert_eq!(
        efforts(&queued),
        [("implement", Medium), ("review", XHigh), ("ship", XHigh)]
    );
    assert_eq!(effort_source(&queued, "implement"), PlanSource::Explicit);
    let why = "sizer: critical -> x_high; the plan pins medium";
    assert_eq!(reason(&queued, "implement"), why);
    h.dispatch().await;
    let running = attempt(&h, &queued.id, "implement").await;
    assert_eq!(running.choice.effort, Medium);
    assert_eq!(
        recorded(&running),
        (Some(RiskClass::Critical), Some(PlanSource::Explicit), why)
    );
}

/// Ruling (c): a pin over the policy's cap is refused as in 1.6.3, never clipped.
#[tokio::test]
async fn a_pin_over_the_policy_s_cap_is_refused_at_enqueue_and_at_a_retry() {
    let h = harness(Config::default()).await;
    let asked = reasoned(request("Add a button", &[], pinned("implement", XHigh)));
    let queued = item(h.cli(Request::Enqueue(asked.clone())).await).await;
    cancel(&h, &queued.id).await;
    h.repo_config("[models]\nmax_effort = \"high\"\n");
    let refused = retry(&h, &queued.id).await.expect_err("a pin over the cap");
    assert!(refused.to_string().contains("max_effort"), "{refused}");
    let kept = h.item(&queued.id).await;
    assert_eq!(
        efforts(&kept)[0],
        ("implement", XHigh),
        "the pin is never clipped"
    );
    assert!(
        matches!(kept.state, QueueState::Cancelled { .. }),
        "{:?}",
        kept.state
    );
    let refused = h
        .cli(Request::Enqueue(asked))
        .await
        .expect_err("a pin over the cap");
    assert!(refused.to_string().contains("max_effort"), "{refused}");
}

#[tokio::test]
async fn an_attempt_records_its_class_its_effort_source_and_the_reason() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    h.dispatch().await;
    let running = attempt(&h, &queued.id, "implement").await;
    assert_eq!(running.choice.effort, XHigh);
    let why = "sizer: critical -> x_high";
    assert_eq!(
        recorded(&running),
        (Some(RiskClass::Critical), Some(PlanSource::Sizer), why)
    );
    let stored = serde_json::to_value(&running).unwrap();
    assert_eq!(stored["risk_class"], "critical");
    assert_eq!(stored["effort_source"], "sizer");
    assert_eq!(stored["effort_reasons"], why);
}

/// Check 1: sizing reads the item's current title, tags, brief and review level.
#[tokio::test]
async fn an_update_or_a_steer_note_sizes_a_waiting_item_again() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    let titled = ItemPatch {
        title: Some("Fix the deadlock in the pump".into()),
        ..ItemPatch::default()
    };
    let renamed = update(&h, &queued.id, titled).await;
    assert_eq!(class(&renamed), Some(RiskClass::Elevated));
    assert_eq!(
        efforts(&renamed),
        [("implement", High), ("review", High), ("ship", High)]
    );
    let tagged = ItemPatch {
        tags: Some(vec!["risk:critical".into()]),
        ..ItemPatch::default()
    };
    let tagged = update(&h, &queued.id, tagged).await;
    assert_eq!(
        efforts(&tagged),
        [("implement", XHigh), ("review", XHigh), ("ship", XHigh)]
    );

    let plain = enqueue(&h, "Add a button", &[]).await;
    let steered = steer(
        &h,
        &plain.id,
        note("It changes how a credential is stored."),
    )
    .await;
    assert_eq!(class(&steered), Some(RiskClass::Elevated));
    assert_eq!(
        efforts(&steered),
        [("implement", High), ("review", High), ("ship", High)]
    );

    // `direct` takes a review level per item; the daemon fills its whole plan.
    let mut asked = request("Add a button", &[], ModelPlanDraft::default());
    asked.pipeline = Some("direct".into());
    let strict = item(h.cli(Request::Enqueue(asked)).await).await;
    assert_eq!(class(&strict), Some(RiskClass::Ordinary));
    let knobs = AutonomyPatch {
        review: Some(ReviewLevel::Strict),
        ..AutonomyPatch::default()
    };
    let patch = ItemPatch {
        autonomy: Some(knobs),
        ..ItemPatch::default()
    };
    let strict = update(&h, &strict.id, patch).await;
    assert_eq!(strict.autonomy.review, ReviewLevel::Strict);
    assert_eq!(class(&strict), Some(RiskClass::Elevated));
    assert_eq!(efforts(&strict)[0], ("implement", High));
}

/// A11 check 1: the run starting a dispatching item has read its plan, so
/// tags that would class it otherwise are refused there, not stored unsized.
#[tokio::test]
async fn tags_that_change_the_class_are_refused_while_the_item_is_starting() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    let now = archon_core::now();
    let mut starting = h.ctx.store.lock().await;
    starting
        .transition_item(&queued.id, QueueState::Dispatching, now)
        .unwrap();
    drop(starting);
    let tagged = |tag: &str| Request::UpdateItem {
        item: queued.id.clone(),
        patch: ItemPatch {
            tags: Some(vec![tag.into()]),
            ..ItemPatch::default()
        },
    };
    let refused = h.orchestrator(tagged("risk:critical")).await.unwrap_err();
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("send its new tags again"),
        "{refused}"
    );
    let unchanged = h.item(&queued.id).await;
    assert!(
        unchanged.tags.is_empty(),
        "nothing of a refused update is stored"
    );
    assert_eq!(unchanged.model_plan, queued.model_plan);
    let labelled = item(h.orchestrator(tagged("frontend")).await).await;
    assert_eq!(labelled.tags, ["frontend"], "a tag the sizer does not read");
    assert_eq!(labelled.model_plan, queued.model_plan);
}

/// Check 1: only the stages not yet started follow a steer; the started one
/// keeps the effort it was issued.
#[tokio::test]
async fn a_steer_of_a_running_item_sizes_only_the_stages_not_yet_started() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    h.dispatch().await;
    assert_eq!(
        attempt(&h, &queued.id, "implement").await.choice.effort,
        Medium
    );
    let steered = steer(
        &h,
        &queued.id,
        note("It changes how a credential is stored."),
    )
    .await;
    assert_eq!(
        efforts(&steered),
        [("implement", Medium), ("review", High), ("ship", High)]
    );
    assert_eq!(reason(&steered, "implement"), "sizer: ordinary -> medium");
    assert_eq!(reason(&steered, "review"), "sizer: elevated -> high");

    let tagged = ItemPatch {
        tags: Some(vec!["risk:critical".into()]),
        ..ItemPatch::default()
    };
    let tagged = update(&h, &queued.id, tagged).await;
    assert_eq!(
        efforts(&tagged),
        [("implement", Medium), ("review", XHigh), ("ship", XHigh)]
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let review = attempt(&h, &queued.id, "review").await;
    assert_eq!(
        review.choice.effort, XHigh,
        "the rebuilt driver runs the new sizing"
    );
    assert_eq!(recorded(&review).0, Some(RiskClass::Critical));
    let implement = attempt(&h, &queued.id, "implement").await;
    assert_eq!(
        recorded(&implement).0,
        Some(RiskClass::Ordinary),
        "as it was issued"
    );
}

/// Check 2: a retry carries the frozen sizing, whatever was learned since.
#[tokio::test]
async fn a_retry_keeps_the_frozen_sizing_and_ignores_a_diff_size_learned_since() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    cancel(&h, &queued.id).await;
    let key = format!("learning/size/{}", queued.id);
    h.ctx.store.lock().await.kv_set(&key, "\"large\"").unwrap();
    let retried = retry(&h, &queued.id).await.unwrap();
    assert_eq!(
        class(&retried),
        Some(RiskClass::Ordinary),
        "a large diff would be elevated"
    );
    assert_eq!(
        retried.model_plan, queued.model_plan,
        "nothing was sized again"
    );
    let fresh = enqueue(&h, "Add a button", &["diff:large"]).await;
    assert_eq!(
        class(&fresh),
        Some(RiskClass::Elevated),
        "the size does count when first sized"
    );
}

/// Ruling (b): a re-plan clips carried sized efforts to the sizer's cap as it
/// is now, with the reason.
#[tokio::test]
async fn a_retry_clips_carried_sized_efforts_to_a_lowered_sizer_cap() {
    let mut h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    cancel(&h, &queued.id).await;
    h.restart_with(sizer_max(High));
    let retried = retry(&h, &queued.id).await.unwrap();
    assert_eq!(
        efforts(&retried),
        [("implement", High), ("review", High), ("ship", High)]
    );
    let why = "sizer: critical -> x_high; clipped from x_high to the cap high";
    assert_eq!(reason(&retried, "implement"), why);
    assert_eq!(effort_source(&retried, "implement"), PlanSource::Sizer);
    assert_eq!(
        class(&retried),
        Some(RiskClass::Critical),
        "the class is kept"
    );
}

/// Ruling (b) with sizing off: only the policy's cap clips a carried sized effort.
#[tokio::test]
async fn with_sizing_off_a_retry_clips_only_to_the_policy_s_cap() {
    let mut h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    cancel(&h, &queued.id).await;
    let mut off = sizing_off();
    off.models.sizer_max_effort = Some(High);
    h.restart_with(off);
    let retried = retry(&h, &queued.id).await.unwrap();
    assert_eq!(
        retried.model_plan, queued.model_plan,
        "sizer_max_effort alone clips nothing"
    );
    cancel(&h, &queued.id).await;
    h.repo_config("[models]\nmax_effort = \"high\"\n");
    let clipped = retry(&h, &queued.id).await.unwrap();
    assert_eq!(
        efforts(&clipped),
        [("implement", High), ("review", High), ("ship", High)]
    );
    let why = "sizer: critical -> x_high; clipped from x_high to the cap high";
    assert_eq!(reason(&clipped, "review"), why);
}

/// Ruling (a): with no re-plan, a lowered `sizer_max_effort` leaves a frozen
/// plan untouched; only the policy's cap clips it at dispatch.
#[tokio::test]
async fn dispatch_clips_a_frozen_sized_effort_to_the_policy_s_cap_only() {
    let mut h = harness(Config::default()).await;
    let frozen = enqueue(&h, "Add a button", &["risk:critical"]).await;
    h.restart_with(sizer_max(High));
    h.dispatch().await;
    let running = attempt(&h, &frozen.id, "implement").await;
    assert_eq!(
        running.choice.effort, XHigh,
        "sized before the cap was lowered"
    );
    assert_eq!(h.item(&frozen.id).await.model_plan, frozen.model_plan);

    let mut h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    h.repo_config("[models]\nmax_effort = \"high\"\n");
    h.restart();
    h.dispatch().await;
    let running = attempt(&h, &queued.id, "implement").await;
    assert_eq!(running.choice.effort, High);
    let why = "sizer: critical -> x_high; clipped from x_high to the cap high";
    assert_eq!(
        recorded(&running),
        (Some(RiskClass::Critical), Some(PlanSource::Sizer), why)
    );
    let stored = h.item(&queued.id).await;
    assert_eq!(
        efforts(&stored),
        [("implement", High), ("review", High), ("ship", High)]
    );
    assert_eq!(reason(&stored, "ship"), why);
}

/// Ruling (c) at dispatch: a pin over a lowered policy cap is never clipped.
#[tokio::test]
async fn dispatch_never_clips_a_pin_over_the_policy_s_cap() {
    let h = harness(Config::default()).await;
    let asked = reasoned(request(
        "Add a button",
        &["risk:critical"],
        pinned("implement", XHigh),
    ));
    let queued = item(h.cli(Request::Enqueue(asked)).await).await;
    h.repo_config("[models]\nmax_effort = \"high\"\n");
    h.dispatch().await;
    let stored = h.item(&queued.id).await;
    assert_eq!(
        efforts(&stored)[0],
        ("implement", XHigh),
        "the pin is as it was"
    );
    assert!(
        h.stages(&queued.id).await.is_empty(),
        "refused as 1.6.3 refuses it"
    );
    assert!(
        !matches!(stored.state, QueueState::Running { .. }),
        "{:?}",
        stored.state
    );
}

/// Check 3: an inserted stage is sized from its own default (high), not from
/// the choice it borrows its model from (medium) or the pipeline's fallback.
#[tokio::test]
async fn an_inserted_stage_is_sized_from_its_own_default_effort() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    let waiting = steer(&h, &queued.id, insert_verify()).await;
    let ordinary = [
        ("implement", Medium),
        ("review", Medium),
        ("ship", Low),
        ("verify", High),
    ];
    assert_eq!(efforts(&waiting), ordinary);
    assert_eq!(reason(&waiting, "verify"), "sizer: ordinary -> high");

    let live = enqueue(&h, "docs: fix a typo", &[]).await;
    h.dispatch().await;
    let live = steer(&h, &live.id, insert_verify()).await;
    assert_eq!(efforts(&live)[3], ("verify", Low), "its own class, too");
    assert_eq!(reason(&live, "verify"), "sizer: mechanical -> low");
}

/// Check 3: a plan carried whole that was never sized stays unsized, and a
/// stage added to it is still sized from its own default.
#[tokio::test]
async fn a_plan_never_sized_stays_unsized_when_it_is_carried_whole() {
    let mut h = harness(sizing_off()).await;
    let queued = enqueue(&h, "Fix the deadlock in the pump", &[]).await;
    let as_1_6_3 = [("implement", Medium), ("review", Medium), ("ship", Low)];
    assert_eq!(efforts(&queued), as_1_6_3);
    assert!(queued.model_plan.sizing().is_none());
    h.restart_with(Config::default());
    cancel(&h, &queued.id).await;
    let retried = retry(&h, &queued.id).await.unwrap();
    assert_eq!(
        retried.model_plan, queued.model_plan,
        "carried whole, never sized"
    );
    let steered = steer(&h, &queued.id, note("It also touches the mutex.")).await;
    assert_eq!(
        steered.model_plan, queued.model_plan,
        "nothing to size again"
    );

    let added = steer(&h, &queued.id, insert_verify()).await;
    assert_eq!(
        efforts(&added)[..3],
        as_1_6_3,
        "the carried keys stay as they were"
    );
    assert_eq!(
        efforts(&added)[3],
        ("verify", High),
        "max(its own high, the class floor)"
    );
    let sizing = added.model_plan.sizing().expect("the new key is recorded");
    assert_eq!(sizing.keys.keys().collect::<Vec<_>>(), ["verify"]);
}

/// Check 5: a queued item keeps its frozen plan across a restart with sizing
/// off; a stage added afterwards is 1.6.3's, and its attempt records no class.
#[tokio::test]
async fn after_a_restart_with_sizing_off_the_plan_is_frozen_and_a_new_stage_is_1_6_3_s() {
    let mut h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
    h.restart_with(sizing_off());
    executor::recover(&h.ctx).await.expect("recover");
    assert_eq!(h.item(&queued.id).await.model_plan, queued.model_plan);
    let added = steer(&h, &queued.id, insert_verify()).await;
    assert_eq!(efforts(&added)[..3], efforts(&queued)[..], "frozen");
    let sizing = added.model_plan.sizing().expect("the frozen record");
    assert!(
        !sizing.keys.contains_key("verify"),
        "the sizer never saw it"
    );
    assert_eq!(reason(&added, "verify"), "");
    assert_ne!(effort_source(&added, "verify"), PlanSource::Sizer);

    h.dispatch().await;
    let implement = attempt(&h, &queued.id, "implement").await;
    assert_eq!(
        implement.choice.effort, XHigh,
        "the frozen decision still runs"
    );
    assert_eq!(recorded(&implement).0, Some(RiskClass::Critical));
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let verify = attempt(&h, &queued.id, "verify").await;
    assert_eq!(
        verify.sizing,
        AttemptSizing::default(),
        "no stale risk class"
    );
    let stored = serde_json::to_value(&verify).unwrap();
    assert!(stored.get("risk_class").is_none() && stored.get("effort_source").is_none());
}

/// Check 7: with sizing off nothing is recorded, on the item or its attempts.
#[tokio::test]
async fn with_sizing_off_an_item_and_its_attempts_are_stored_as_1_6_3_stores_them() {
    let h = harness(sizing_off()).await;
    let queued = enqueue(&h, "Fix the deadlock", &["risk:critical"]).await;
    assert_eq!(
        efforts(&queued),
        [("implement", Medium), ("review", Medium), ("ship", Low)]
    );
    assert_eq!(effort_source(&queued, "implement"), PlanSource::Fallback);
    let stored = serde_json::to_value(&queued).unwrap();
    assert!(stored.get("effort_sizing").is_none(), "{stored}");
    let titled = ItemPatch {
        title: Some("docs: fix a typo".into()),
        ..ItemPatch::default()
    };
    let renamed = update(&h, &queued.id, titled).await;
    assert_eq!(renamed.model_plan, queued.model_plan, "nothing sizes it");
    h.dispatch().await;
    let running = attempt(&h, &queued.id, "implement").await;
    assert_eq!(running.sizing, AttemptSizing::default());
    let stored = serde_json::to_value(&running).unwrap();
    let keys: BTreeMap<String, serde_json::Value> = serde_json::from_value(stored).unwrap();
    for added in ["risk_class", "effort_source", "effort_reasons"] {
        assert!(!keys.contains_key(added), "{added} is not a 1.6.3 field");
    }
}

/// Check 1: an initiative's member is sized with its own title, not the
/// initiative's.
#[tokio::test]
async fn an_initiative_member_is_sized_with_its_own_title() {
    use archon_core::initiative::{EnqueueInitiative, InitiativeMember};
    let h = harness(Config::default()).await;
    let member = |title: &str| InitiativeMember {
        intent: None,
        onto: None,
        repo: RepoRef(REPO_NAME.into()),
        brief: "Change the field name.".into(),
        title: Some(title.into()),
        pipeline: Some(SIZED.into()),
        model_plan: Some(models()),
        model_reasons: None,
        autonomy: Default::default(),
        after: Vec::new(),
    };
    let asked = EnqueueInitiative {
        title: "Rotate every credential".into(),
        contract: "Every service reads account_id.".into(),
        atomic: false,
        idempotency_key: None,
        members: vec![
            member("docs: describe the field"),
            member("Fix the deadlock"),
        ],
    };
    let detail = match h
        .cli(Request::EnqueueInitiative(asked))
        .await
        .expect("accepted")
    {
        Reply::Initiative(detail) => *detail,
        other => panic!("replied {other:?}"),
    };
    let classes: Vec<_> = detail.members.iter().map(class).collect();
    assert_eq!(
        classes,
        [Some(RiskClass::Mechanical), Some(RiskClass::Elevated)]
    );
    assert_eq!(efforts(&detail.members[0])[0], ("implement", Low));
    assert_eq!(efforts(&detail.members[1])[0], ("implement", High));
}
