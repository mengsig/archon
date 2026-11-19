//! The cap rulings of the effort sizer (A11) on recovery, revise and reshape,
//! on the helpers of [`crate::effort_sizing`].
#![allow(unused_imports)]

use crate::effort_sizing::*;
use archon_core::autonomy::{AutonomyPatch, ProposalMode};
use archon_core::config::Config;
use archon_core::effort_sizing::RiskClass;
use archon_core::id::QueueItemId;
use archon_core::model::Effort::{self, High, XHigh};
use archon_core::plan_origin::PlanSource;
use archon_core::protocol::ItemPatch;
use archon_core::protocol::Request;
use archon_core::queue::QueueItem;
use archon_core::run::{StageRun, StageState};
use archon_daemon::executor;

const WHY: &str = "sizer: critical -> x_high; clipped from x_high to the cap high";
const ALL_HIGH: [(&str, Effort); 3] = [("implement", High), ("review", High), ("ship", High)];
const ALL_XHIGH: [(&str, Effort); 3] = [("implement", XHigh), ("review", XHigh), ("ship", XHigh)];

fn policy_cap(effort: Effort) -> Config {
    let mut config = Config::default();
    config.models.max_effort = Some(effort);
    config
}

/// The efforts of the three stages every revision and reshape here carries over.
fn carried(item: &QueueItem) -> Vec<(&str, Effort)> {
    let own = ["implement", "review", "ship"];
    let all = efforts(item);
    all.into_iter()
        .filter(|(key, _)| own.contains(key))
        .collect()
}

fn proposal_lavish() -> ItemPatch {
    let knobs = AutonomyPatch {
        proposal: Some(ProposalMode::Lavish),
        ..AutonomyPatch::default()
    };
    ItemPatch {
        autonomy: Some(knobs),
        ..ItemPatch::default()
    }
}

/// What a stored run keeps of its plan: the plan, its reasons and the attempt.
async fn stored_run(h: &Harness, id: &QueueItemId) -> (QueueItem, StageRun) {
    (h.item(id).await, attempt(h, id, "implement").await)
}

/// Ruling (a) at recovery: a live run's stored plan is read unchanged, whether
/// the global policy cap, the sizer's cap or a repository cap is lowered.
#[tokio::test]
async fn recovery_reads_a_live_run_s_stored_plan_unchanged_under_a_lowered_cap() {
    let lowered = [policy_cap(High), sizer_max(High), {
        let mut both = policy_cap(High);
        both.models.sizer_max_effort = Some(High);
        both
    }];
    for config in lowered {
        let mut h = harness(Config::default()).await;
        let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
        h.dispatch().await;
        let before = stored_run(&h, &queued.id).await;
        assert_eq!(before.1.choice.effort, XHigh, "ran at the sized effort");
        h.repo_config("[models]\nmax_effort = \"high\"\n");
        h.restart_with(config);
        executor::recover(&h.ctx).await.expect("recover");
        let after = stored_run(&h, &queued.id).await;
        assert_eq!(after.0.model_plan, before.0.model_plan);
        assert_eq!(efforts(&after.0), ALL_XHIGH);
        assert_eq!(
            reason(&after.0, "implement"),
            reason(&before.0, "implement")
        );
        assert_eq!(after.1.choice, before.1.choice);
        assert_eq!(after.1.sizing, before.1.sizing);
        assert_eq!(h.stages(&queued.id).await.len(), 1, "no new attempt");
    }
}

/// The request Aether received for item `id`'s stage `stage`, which runs.
async fn issued(h: &Harness, id: &QueueItemId, stage: &str) -> (StageRun, Effort) {
    let attempt = attempt(h, id, stage).await;
    assert_eq!(attempt.state, StageState::Running, "{stage} started");
    let key = attempt.id.as_str();
    let sent = h.backend.requests();
    let sent = sent.iter().rfind(|r| r.idempotency_key == key);
    let effort = sent.expect("sent to Aether").choice.effort;
    assert_eq!(
        effort, attempt.choice.effort,
        "the attempt records what was sent"
    );
    (attempt, effort)
}

/// Ruling (a) after recovery: the recovered driver reads the stored plan as it
/// is. The run's next stage, sent after the restart, is held to the policy's
/// cap (global or the repository's) with the reason; a lowered sizer cap
/// alone leaves it at the frozen x_high. The item's stored plan is never rewritten.
#[tokio::test]
async fn a_recovered_run_s_next_stage_is_held_to_the_policy_s_cap_alone() {
    const CLIPPED: &str =
        "sizer: critical -> x_high; this attempt: clipped from x_high to the cap high";
    let cases = [
        (sizer_max(High), None, XHigh, "sizer: critical -> x_high"),
        (policy_cap(High), None, High, CLIPPED),
        (
            Config::default(),
            Some("[models]\nmax_effort = \"high\"\n"),
            High,
            CLIPPED,
        ),
    ];
    for (config, repo_cap, runs, why) in cases {
        let mut h = harness(Config::default()).await;
        let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
        h.dispatch().await;
        if let Some(toml) = repo_cap {
            h.repo_config(toml);
        }
        h.restart_with(config);
        executor::recover(&h.ctx).await.expect("recover");
        h.settle_running(|_| completed("implemented")).await;
        h.dispatch().await;
        let (review, sent) = issued(&h, &queued.id, "review").await;
        assert_eq!(sent, runs, "{why}");
        let (class, source, reason) = recorded(&review);
        assert_eq!(
            (class, source),
            (Some(RiskClass::Critical), Some(PlanSource::Sizer))
        );
        assert_eq!(reason, why);
        assert_eq!(
            efforts(&h.item(&queued.id).await),
            ALL_XHIGH,
            "the stored plan is never rewritten"
        );
    }
}

/// Ruling (c) at dispatch: a pinned effort over a policy cap lowered after it
/// was admitted is refused as 1.6.3 refuses it, never clipped.
#[tokio::test]
async fn a_pin_over_a_lowered_policy_cap_is_refused_when_its_stage_is_sent() {
    let mut h = harness(Config::default()).await;
    let mut asked = request("Add a button", &[], pinned("review", XHigh));
    asked.model_reasons = Some([("review".to_owned(), "a hard review".to_owned())].into());
    let queued = item(h.cli(Request::Enqueue(asked)).await).await;
    h.dispatch().await;
    h.restart_with(policy_cap(High));
    executor::recover(&h.ctx).await.expect("recover");
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let review = attempt(&h, &queued.id, "review").await;
    assert_eq!(review.choice.effort, XHigh, "a pin is never clipped");
    assert!(
        matches!(review.state, StageState::Failed { .. }),
        "{:?}",
        review.state
    );
    let key = review.id.as_str();
    assert!(
        h.backend.created_for(key).is_none(),
        "refused before Aether"
    );
}

/// Ruling (b) on an autonomy revision that restages a queued item: the carried
/// x_high is clipped to the cap with the reason.
#[tokio::test]
async fn a_restaging_revision_clips_carried_sized_efforts_to_the_cap() {
    for config in [sizer_max(High), policy_cap(High)] {
        let mut h = harness(Config::default()).await;
        let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
        assert_eq!(efforts(&queued), ALL_XHIGH);
        h.restart_with(config);
        let revised = update(&h, &queued.id, proposal_lavish()).await;
        assert_eq!(carried(&revised), ALL_HIGH);
        assert_eq!(reason(&revised, "implement"), WHY);
        assert_eq!(effort_source(&revised, "implement"), PlanSource::Sizer);
    }
}

/// Ruling (b) on a reshape (an inserted stage): carried x_high is clipped to
/// the cap with the reason.
#[tokio::test]
async fn a_reshape_clips_carried_sized_efforts_to_the_cap() {
    for config in [sizer_max(High), policy_cap(High)] {
        let mut h = harness(Config::default()).await;
        let queued = enqueue(&h, "Add a button", &["risk:critical"]).await;
        h.restart_with(config);
        let shaped = steer(&h, &queued.id, insert_verify()).await;
        assert_eq!(efforts(&shaped)[..3], ALL_HIGH);
        assert_eq!(reason(&shaped, "implement"), WHY);
        assert_eq!(reason(&shaped, "ship"), WHY);
        assert_eq!(effort_source(&shaped, "review"), PlanSource::Sizer);
    }
}

/// Ruling (b) with sizing off: revise and reshape clip only to the policy's cap.
#[tokio::test]
async fn with_sizing_off_revise_and_reshape_clip_only_to_the_policy_s_cap() {
    let mut off = sizing_off();
    off.models.sizer_max_effort = Some(High);
    for (config, expected) in [
        (off.clone(), ALL_XHIGH),
        (
            {
                let mut capped = off.clone();
                capped.models.max_effort = Some(High);
                capped
            },
            ALL_HIGH,
        ),
    ] {
        let mut h = harness(Config::default()).await;
        let revised = enqueue(&h, "Add a button", &["risk:critical"]).await;
        let shaped = enqueue(&h, "Add a button", &["risk:critical"]).await;
        h.restart_with(config);
        let revised = update(&h, &revised.id, proposal_lavish()).await;
        assert_eq!(carried(&revised), expected, "revise");
        let shaped = steer(&h, &shaped.id, insert_verify()).await;
        assert_eq!(efforts(&shaped)[..3], expected, "reshape");
    }
}
